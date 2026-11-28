//! The wall tablet's local HTTP API (port 2025), as pure functions.
//!
//! Shapes follow the aa_interop `docs/tablet_api/README.md`, what Home
//! Assistant's `advantage_air` integration reads and sends, and observed
//! behaviour of the official phone app. Names (system, aircon, zones) are
//! not on the bus — the original tablet stores them — so they come from the
//! state file (set by clients) or else the config file.
//!
//! Tablet-side features (timers, climate control, MyAuto, quiet night) are
//! state in [`AppState`](crate::state::AppState), run by
//! [`features`](crate::features). Auto fan, lights and "things" are not
//! offered: their keys are left out and `setAircon` rejects them.

use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::bridge::{BusState, Plan};
use crate::record::{Record, DEST_CB};
use crate::registers::{
    decode_error, Firmware, Mode, SystemStatus, ZoneConfig, ZoneInfo, ZoneState, MAX_SET_TEMP,
    MAX_ZONES, MIN_SET_TEMP,
};
use crate::state::{sanitize_name, targets_valid, AppState, MAX_TIMER_MINUTES};

/// Default `system.sysType`: the aircon-only tablet's type, as used by the
/// MyPlace reference implementation (Home Assistant's fixture has "e-zone").
pub const DEFAULT_SYS_TYPE: &str = "MyAir5";

/// Names and identity reported to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiInfo {
    pub system_name: String,
    /// Stable id; Home Assistant uses it as the device's unique id.
    pub system_id: String,
    pub aircon_name: String,
    /// `system.sysType`. The official app refuses a system whose type it
    /// does not know ("unable to determine system type").
    pub sys_type: String,
    /// `system.myAppRev` (checked by [`check_my_app_rev`]).
    pub my_app_rev: String,
    /// `system.aaServiceRev`.
    pub aa_service_rev: String,
    /// Zone names, zone 1 first. Missing ones default to "Zone N".
    pub zone_names: Vec<String>,
    /// Whether to send the climate control, MyAuto and quiet night settings.
    /// Some original tablets leave them out, and Home Assistant only offers
    /// those controls when they are sent.
    pub advertise_features: bool,
}

/// The `info` keys left out when `advertise_features` is off.
const FEATURE_KEYS: [&str; 5] = [
    "climateControlModeEnabled",
    "myAutoModeEnabled",
    "myAutoCoolTargetTemp",
    "myAutoHeatTargetTemp",
    "quietNightModeEnabled",
];

impl Default for ApiInfo {
    fn default() -> Self {
        ApiInfo {
            system_name: "ductbridge".into(),
            system_id: "ductbridge".into(),
            aircon_name: "Aircon".into(),
            sys_type: DEFAULT_SYS_TYPE.into(),
            my_app_rev: DEFAULT_MY_APP_REV.into(),
            aa_service_rev: DEFAULT_AA_SERVICE_REV.into(),
            zone_names: Vec::new(),
            advertise_features: true,
        }
    }
}

impl ApiInfo {
    /// Zone name: set by a client (state file), else config, else "Zone N".
    fn zone_name(&self, app: &AppState, n: u8) -> String {
        if let Some(s) = app.names.zones.get(&n) {
            return s.clone();
        }
        self.zone_names
            .get(n as usize - 1)
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Zone {n}"))
    }
    fn system_name(&self, app: &AppState) -> String {
        app.names
            .system
            .clone()
            .unwrap_or_else(|| self.system_name.clone())
    }
    fn aircon_name(&self, app: &AppState) -> String {
        app.names
            .aircon
            .clone()
            .unwrap_or_else(|| self.aircon_name.clone())
    }
}

/// Default `system.myAppRev` and `system.aaServiceRev`: what a current
/// original tablet reports (observed). A lower revision makes the official
/// app offer a tablet upgrade. The app reads the number before the first
/// `.` and only stores the system's `rid` when it is ≥ 13.
pub const DEFAULT_MY_APP_REV: &str = "15.1548";
pub const DEFAULT_AA_SERVICE_REV: &str = "14.118";

/// Check a configured `myAppRev`: `major.build`, major ≥ 13.
pub fn check_my_app_rev(v: &str) -> Result<(), String> {
    let ok = v.split_once('.').is_some_and(|(major, build)| {
        major.parse::<u32>().is_ok_and(|m| m >= 13)
            && !build.is_empty()
            && build.bytes().all(|b| b.is_ascii_digit())
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "my_app_rev {v:?} (expected major.build, major ≥ 13, e.g. {DEFAULT_MY_APP_REV:?})"
        ))
    }
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Cool => "cool",
        Mode::Heat => "heat",
        Mode::Vent => "vent",
        Mode::Auto => "auto",
        Mode::Dry => "dry",
    }
}

fn mode_from_name(s: &str) -> Option<u8> {
    Some(match s {
        "cool" => 1,
        "heat" => 2,
        "vent" => 3,
        "auto" => 4,
        "dry" => 5,
        _ => return None,
    })
}

const FAN_NAMES: [&str; 6] = ["off", "low", "medium", "high", "auto", "autoAA"];

fn fan_from_name(s: &str) -> Option<u8> {
    // "off" is not settable.
    FAN_NAMES
        .iter()
        .position(|n| *n == s)
        .filter(|&i| i > 0)
        .map(|i| i as u8)
}

const FRESH_AIR_NAMES: [&str; 3] = ["none", "off", "on"];

/// What the bus has told us about the primary aircon, in decoded form.
pub(crate) struct View<'a> {
    pub(crate) st: &'a BusState,
    pub(crate) unit: (u8, u32),
    pub(crate) now: Instant,
}

impl View<'_> {
    pub(crate) fn reg(&self, reg: u8, zone: u8) -> Option<&Record> {
        self.st
            .registers
            .get(&(self.unit.0, self.unit.1, reg, zone))
    }
    pub(crate) fn status(&self) -> Option<(SystemStatus, &Record)> {
        let r = self.reg(SystemStatus::REG, 0)?;
        SystemStatus::decode(r).ok().map(|s| (s, r))
    }
    pub(crate) fn zone(&self, n: u8) -> Option<(ZoneState, &Record)> {
        let r = self.reg(ZoneState::REG, n)?;
        ZoneState::decode(r).ok().map(|z| (z, r))
    }
    /// Value a new partial write builds on: pending desired value if any,
    /// else the confirmed register.
    pub(crate) fn desired(&self, reg: u8, zone: u8) -> Option<&Record> {
        self.st
            .desired(&(self.unit.0, self.unit.1, reg, zone), self.now)
    }
    pub(crate) fn info(&self) -> Option<ZoneInfo> {
        self.reg(ZoneInfo::REG, 0)
            .and_then(|r| ZoneInfo::decode(r).ok())
    }
    /// Number of zones: from register 01 if seen, else the highest zone
    /// number with a register 03.
    pub(crate) fn zone_count(&self) -> u8 {
        if let Some(i) = self.info() {
            return i.zones;
        }
        (1..=MAX_ZONES)
            .rev()
            .find(|&n| self.reg(ZoneState::REG, n).is_some())
            .unwrap_or(0)
    }
}

/// How long after the unit is first seen to keep waiting for a complete
/// picture (register 01, and 03 + 04 for every zone) before answering with
/// what has arrived. Clients such as Home Assistant only create entities
/// (zones, motion and signal sensors) on their first successful read.
pub const DISCOVERY_GRACE: Duration = Duration::from_secs(30);

fn missing(v: &View) -> Option<String> {
    if v.info().is_none() {
        return Some("zone configuration (register 01) not received yet".into());
    }
    for n in 1..=v.zone_count() {
        if v.zone(n).is_none() {
            return Some(format!("zone {n} state (register 03) not received yet"));
        }
        if v.reg(ZoneConfig::REG, n).is_none() {
            return Some(format!("zone {n} config (register 04) not received yet"));
        }
    }
    None
}

/// The primary unit and its status, or why the API cannot answer.
pub(crate) fn view(st: &BusState, now: Instant) -> Result<View<'_>, String> {
    let unit = st.unit.ok_or("no aircon unit seen on the bus yet")?;
    let v = View { st, unit, now };
    if v.status().is_none() {
        return Err("aircon status (register 05) not received yet".into());
    }
    if let Some(m) = missing(&v) {
        let since = st.first_seen.get(&unit).copied().unwrap_or(now);
        if now.saturating_duration_since(since) < DISCOVERY_GRACE {
            return Err(m);
        }
        // Grace over: serve what we have, but never a zone without its state.
        for n in 1..=v.zone_count() {
            if v.zone(n).is_none() {
                return Err(m);
            }
        }
    }
    if !st.is_online(unit, now) {
        return Err("aircon unit offline (no registers seen recently)".into());
    }
    Ok(v)
}

/// Temperatures to one decimal, without f32 noise (21.3, not 21.2999…).
fn temp(t: f32) -> Value {
    json!((t as f64 * 10.0).round() / 10.0)
}

/// `GET /getSystemData`. `Err` means the bus has not delivered enough yet;
/// the server answers 503 so clients retry instead of caching an empty system.
pub fn system_data(st: &BusState, info: &ApiInfo, now: Instant) -> Result<Value, String> {
    let v = view(st, now)?;
    let (s, _) = v.status().expect("checked in view");
    let app = &st.app;
    let running = crate::features::running(st, now);
    let zi = v.info();
    let error = v
        .reg(0x08, 0)
        .and_then(|r| decode_error(r).ok())
        .unwrap_or_default();
    let cb_type = v
        .reg(Firmware::REG, 0)
        .and_then(|r| Firmware::decode(r).ok())
        .map_or(0, |f| f.cb_type);

    let mut zones = Map::new();
    for n in 1..=v.zone_count() {
        let (z, _) = v.zone(n).expect("checked in view");
        let zc = v
            .reg(ZoneConfig::REG, n)
            .and_then(|r| ZoneConfig::decode(r).ok());
        zones.insert(
            format!("z{n:02}"),
            json!({
                "name": info.zone_name(app, n),
                "number": n,
                "state": if z.open { "open" } else { "close" },
                "value": z.percent,
                "setTemp": temp(z.set_temp),
                // Zones with a sensor are treated as temperature-controlled.
                "type": u8::from(z.sensor_type > 0),
                "measuredTemp": temp(z.measured_temp.unwrap_or(0.0)),
                "minDamper": zc.map_or(0, |c| c.min_damper),
                "maxDamper": zc.map_or(100, |c| c.max_damper),
                "motion": zc.map_or(0, |c| c.motion),
                "motionConfig": zc.map_or(0, |c| c.motion_config),
                "error": zc.map_or(0, |c| c.error),
                "rssi": zc.map_or(0, |c| c.rssi),
            }),
        );
    }

    let consts = zi.map_or([0; 3], |i| i.constant_zones);
    let mut d = json!({
        "aircons": {
            "ac1": {
                "info": {
                    "name": info.aircon_name(app),
                    // The official app crashes without it. Same characters
                    // as the unit id in the register records; view() only
                    // succeeds once the unit is known, so never empty.
                    "uid": format!("{:05x}", v.unit.1),
                    "state": if s.on { "on" } else { "off" },
                    // MyAuto is a tablet-side mode; the box sees cool/heat.
                    "mode": if app.my_auto_enabled && app.my_auto_selected {
                        "myauto"
                    } else {
                        mode_name(s.mode)
                    },
                    "fan": FAN_NAMES[s.fan as usize],
                    "setTemp": temp(s.set_temp),
                    "myZone": s.my_zone,
                    "freshAirStatus": FRESH_AIR_NAMES[s.fresh_air.min(2) as usize],
                    "filterCleanStatus": u8::from(zi.is_some_and(|i| i.filter_clean)),
                    "constant1": consts[0],
                    "constant2": consts[1],
                    "constant3": consts[2],
                    "noOfZones": v.zone_count(),
                    "countDownToOn": app.count_down_to_on,
                    "countDownToOff": app.count_down_to_off,
                    "airconErrorCode": error,
                    "cbType": cb_type,
                    "climateControlModeEnabled": app.climate_control_enabled,
                    "climateControlModeIsRunning": running.climate_control,
                    "myAutoModeEnabled": app.my_auto_enabled,
                    "myAutoModeIsRunning": running.my_auto,
                    "myAutoModeCurrentSetMode": if app.my_auto_cooling { "cool" } else { "heat" },
                    "myAutoCoolTargetTemp": temp(app.my_auto_cool_target),
                    "myAutoHeatTargetTemp": temp(app.my_auto_heat_target),
                    "quietNightModeEnabled": app.quiet_night_enabled,
                    "quietNightModeIsRunning": running.quiet_night,
                },
                "zones": zones,
            }
        },
        "myLights": { "lights": {} },
        "myThings": { "things": {} },
        "system": {
            "name": info.system_name(app),
            "rid": info.system_id,
            // The official app ignores a reply without mid, name and myAppRev.
            "mid": info.system_id,
            "sysType": info.sys_type,
            "myAppRev": info.my_app_rev,
            "aaServiceRev": info.aa_service_rev,
            "needsUpdate": false,
            "noOfAircons": 1,
            "hasAircons": true,
            "hasLights": false,
            "hasSensors": false,
            "hasThings": false,
            "hasThingsBOG": false,
            "hasThingsLight": false,
        }
    });
    if !info.advertise_features {
        let ac = d["aircons"]["ac1"]["info"]
            .as_object_mut()
            .expect("built above");
        for key in FEATURE_KEYS {
            ac.remove(key);
        }
    }
    Ok(d)
}

/// Decode `%XX` escapes and `+`, leaving anything malformed as it is.
/// Clients send the JSON either properly encoded or completely raw.
fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let escaped = (b[i] == b'%' && i + 2 < b.len())
            .then(|| Some(hex(b[i + 1])? << 4 | hex(b[i + 2])?))
            .flatten();
        match (b[i], escaped) {
            (_, Some(v)) => {
                out.push(v);
                i += 3;
                continue;
            }
            (b'+', _) => out.push(b' '),
            (c, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of `name` in a raw query string, undecoded.
fn query_param_raw<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (percent_decode(k) == name).then_some(v)
    })
}

/// Value of `name` in a raw query string, decoded.
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query_param_raw(query, name).map(percent_decode)
}

/// The `json` parameter: raw JSON as sent by phone apps (where `+` is a
/// literal plus, e.g. in `2.4e+1`), else percent/form-decoded.
fn json_param(query: &str) -> Result<Value, String> {
    let raw = query_param_raw(query, "json").ok_or("missing json parameter")?;
    if let Ok(v) = serde_json::from_str(raw) {
        return Ok(v);
    }
    serde_json::from_str(&percent_decode(raw)).map_err(|e| format!("invalid JSON: {e}"))
}

fn as_temp(v: &Value, what: &str) -> Result<u8, String> {
    let t = v
        .as_f64()
        .ok_or_else(|| format!("{what} must be a number"))?;
    if !(MIN_SET_TEMP as f64..=MAX_SET_TEMP as f64).contains(&t) {
        return Err(format!(
            "{what} {t} outside {MIN_SET_TEMP}–{MAX_SET_TEMP} °C"
        ));
    }
    Ok((t * 2.0).round() as u8)
}

fn as_str<'a>(v: &'a Value, what: &str) -> Result<&'a str, String> {
    v.as_str().ok_or_else(|| format!("{what} must be a string"))
}

fn as_object<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| format!("{what} must be an object"))
}

pub(crate) fn write(unit: (u8, u32), register: u8, payload: [u8; 7]) -> Record {
    Record {
        unit_type: unit.0,
        dest: DEST_CB,
        uid: unit.1,
        register,
        payload,
    }
}

/// `GET /setAircon?json=…`: turn the request into register writes, merged
/// over the current register values, and tablet-side state changes (names,
/// timers, features). All-or-nothing: any unsupported or out-of-range field
/// rejects the whole request.
///
/// Accepts both `{"aircons":{"ac1":…}}` (documented form) and `{"ac1":…}`
/// (what the `advantage_air` Python client sends), each optionally with
/// `"system": {"name": …}` to rename the system.
/// Partial updates build on pending (accepted but unconfirmed) values, so
/// back-to-back requests do not undo each other; run it inside
/// [`Handle::submit`](crate::bridge::Handle::submit) for that to hold.
pub fn set_aircon(query: &str, st: &BusState, now: Instant) -> Result<Plan, String> {
    let req = json_param(query)?;
    let root = as_object(&req, "request")?;
    let mut app = st.app.clone();
    let mut plan = Plan::default();
    let mut acs = Map::new();
    match root.get("aircons") {
        Some(a) => {
            if let Some(k) = root
                .keys()
                .find(|k| !matches!(k.as_str(), "aircons" | "system"))
            {
                return Err(format!("unsupported key {k:?} next to aircons"));
            }
            acs = as_object(a, "aircons")?.clone();
        }
        None => {
            for (k, val) in root {
                if k != "system" {
                    acs.insert(k.clone(), val.clone());
                }
            }
        }
    }
    if let Some(sys) = root.get("system") {
        rename_system(as_object(sys, "system")?, &mut app)?;
    }
    let v = view(st, now)?;
    for (ac, body) in &acs {
        if ac != "ac1" {
            return Err(format!("unknown aircon {ac:?} (only ac1)"));
        }
        for (part, val) in as_object(body, ac)? {
            match part.as_str() {
                "info" => {
                    let w = info_write(&v, as_object(val, "info")?, &mut app, &mut plan.log)?;
                    plan.writes.extend(w);
                }
                "zones" => {
                    for (zk, zb) in as_object(val, "zones")? {
                        let w = zone_write(&v, zk, as_object(zb, zk)?, &mut app)?;
                        plan.writes.extend(w);
                    }
                }
                other => return Err(format!("unsupported section {other:?}")),
            }
        }
    }
    // A client write to a register quiet night is still changing wins: drop
    // those steps so a retry can never undo the user's value.
    if let Some(t) = app.quiet_night_transition.as_mut() {
        t.steps.retain(|s| {
            !plan
                .writes
                .iter()
                .any(|w| w.register == s.register && crate::session::key_of(w).3 == s.zone)
        });
        if t.steps.is_empty() {
            app.quiet_night_transition = None;
        }
    }
    if app != st.app {
        plan.app = Some(app);
    }
    Ok(plan)
}

fn rename_system(fields: &Map<String, Value>, app: &mut AppState) -> Result<(), String> {
    for (k, val) in fields {
        match k.as_str() {
            "name" => app.names.system = Some(sanitize_name(as_str(val, "system name")?)?),
            other => return Err(format!("unsupported field system.{other}")),
        }
    }
    Ok(())
}

/// `GET /setMySystem?json=…`: rename the system. Whether the official app
/// uses this endpoint for that is unverified; accepts `{"name": …}` and
/// `{"system": {"name": …}}`.
pub fn set_my_system(query: &str, st: &BusState) -> Result<Plan, String> {
    let req = json_param(query)?;
    let root = as_object(&req, "request")?;
    let fields = match root.get("system") {
        Some(s) if root.len() == 1 => as_object(s, "system")?,
        _ => root,
    };
    let mut app = st.app.clone();
    rename_system(fields, &mut app)?;
    Ok(Plan {
        log: vec!["setMySystem used to rename the system (unverified endpoint)".into()],
        app: Some(app),
        ..Plan::default()
    })
}

/// Minutes for an on/off timer: an integer or a numeric string (some
/// clients send `"0"`), 0–[`MAX_TIMER_MINUTES`].
fn as_minutes(v: &Value, what: &str) -> Result<u32, String> {
    let n = match v {
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => v.as_u64(),
    };
    n.filter(|&n| n <= MAX_TIMER_MINUTES as u64)
        .map(|n| n as u32)
        .ok_or_else(|| format!("{what} must be 0–{MAX_TIMER_MINUTES} minutes"))
}

fn as_bool(v: &Value, what: &str) -> Result<bool, String> {
    v.as_bool()
        .ok_or_else(|| format!("{what} must be true or false"))
}

fn as_target(v: &Value, what: &str) -> Result<f32, String> {
    v.as_f64()
        .map(|t| t as f32)
        .ok_or_else(|| format!("{what} must be a number"))
}

/// Feature toggles and targets: applied before the other fields, so a
/// request can enable MyAuto and select mode "myauto" together.
const FEATURE_FIELDS: [&str; 5] = [
    "climateControlModeEnabled",
    "myAutoModeEnabled",
    "quietNightModeEnabled",
    "myAutoCoolTargetTemp",
    "myAutoHeatTargetTemp",
];

// Range checks apply to setpoints a client sends. A request that does not
// change the setpoint resends the box's own current value as it is (zones
// without a sensor may report 0), since refusing it would block e.g. a mode
// change.

fn info_write(
    v: &View,
    fields: &Map<String, Value>,
    app: &mut AppState,
    log: &mut Vec<String>,
) -> Result<Option<Record>, String> {
    // Pass 1: feature toggles and targets.
    for (k, val) in fields {
        match k.as_str() {
            "climateControlModeEnabled" => app.climate_control_enabled = as_bool(val, k)?,
            "myAutoModeEnabled" => {
                app.my_auto_enabled = as_bool(val, k)?;
                if !app.my_auto_enabled {
                    app.my_auto_selected = false;
                }
            }
            "quietNightModeEnabled" => app.quiet_night_enabled = as_bool(val, k)?,
            "myAutoCoolTargetTemp" => app.my_auto_cool_target = as_target(val, k)?,
            "myAutoHeatTargetTemp" => app.my_auto_heat_target = as_target(val, k)?,
            _ => {}
        }
    }
    if !targets_valid(app.my_auto_cool_target, app.my_auto_heat_target) {
        return Err(format!(
            "MyAuto targets cool {} / heat {} invalid (cool 20–32, heat 16–28, cool ≥ heat + 4)",
            app.my_auto_cool_target, app.my_auto_heat_target
        ));
    }
    // Pass 2: everything else.
    let rec = v.desired(SystemStatus::REG, 0).expect("checked in view");
    let cur = SystemStatus::decode(rec).map_err(|e| format!("current status: {e:?}"))?;
    let mut p = rec.payload;
    p[0] = u8::from(cur.on);
    p[1] = cur.mode as u8;
    let mut touched = false;
    for (k, val) in fields {
        match k.as_str() {
            k if FEATURE_FIELDS.contains(&k) => continue,
            "state" => {
                p[0] = match as_str(val, "state")? {
                    "on" => 1,
                    "off" => 0,
                    s => return Err(format!("state {s:?} (on or off)")),
                }
            }
            "mode" => {
                let s = as_str(val, "mode")?;
                if s == "myauto" {
                    // Selecting the mode enables the feature (the official
                    // app may not send myAutoModeEnabled first).
                    app.my_auto_enabled = true;
                    // Start in the box's current mode if it is cool or heat.
                    match p[1] {
                        1 => app.my_auto_cooling = true,
                        2 => app.my_auto_cooling = false,
                        _ => {}
                    }
                    app.my_auto_selected = true;
                    p[1] = if app.my_auto_cooling { 1 } else { 2 };
                } else {
                    p[1] = mode_from_name(s).ok_or_else(|| {
                        format!("mode {s:?} (cool, heat, vent, dry, auto or myauto)")
                    })?;
                    app.my_auto_selected = false;
                }
            }
            "fan" => {
                let s = as_str(val, "fan")?;
                p[2] = fan_from_name(s)
                    .ok_or_else(|| format!("fan {s:?} (low, medium, high, auto or autoAA)"))?;
            }
            "setTemp" => p[3] = as_temp(val, "setTemp")?,
            "myZone" => {
                let n = val
                    .as_u64()
                    .filter(|&n| n <= v.zone_count() as u64)
                    .ok_or_else(|| format!("myZone must be 0–{}", v.zone_count()))?
                    as u8;
                if n > 0 && !v.zone(n).is_some_and(|(z, _)| z.sensor_type > 0) {
                    return Err(format!("zone {n} has no temperature sensor"));
                }
                p[4] = n;
            }
            "freshAirStatus" => {
                if cur.fresh_air == 0 {
                    return Err("this system has no fresh air".into());
                }
                p[5] = match as_str(val, "freshAirStatus")? {
                    "off" => 1,
                    "on" => 2,
                    s => return Err(format!("freshAirStatus {s:?} (on or off)")),
                }
            }
            "name" => {
                app.names.aircon = Some(sanitize_name(as_str(val, "name")?)?);
                continue;
            }
            // Every assignment, even to the same value, restarts the
            // timer's minute (via its generation).
            "countDownToOn" => {
                app.count_down_to_on = as_minutes(val, k)?;
                app.count_down_to_on_gen = app.count_down_to_on_gen.wrapping_add(1);
                continue;
            }
            "countDownToOff" => {
                app.count_down_to_off = as_minutes(val, k)?;
                app.count_down_to_off_gen = app.count_down_to_off_gen.wrapping_add(1);
                continue;
            }
            "filterCleanStatus" => {
                if as_minutes(val, k).ok() != Some(0) {
                    return Err("filterCleanStatus can only be set to 0 (reset)".into());
                }
                log.push("filter reset requested; not yet supported on the bus".into());
                continue;
            }
            other => return Err(format!("unsupported field info.{other}")),
        }
        touched = true;
    }
    Ok(touched.then(|| write(v.unit, SystemStatus::REG, p)))
}

fn zone_write(
    v: &View,
    key: &str,
    fields: &Map<String, Value>,
    app: &mut AppState,
) -> Result<Option<Record>, String> {
    // Only the canonical keys getSystemData uses ("z01".."z10"): aliases
    // such as "z1" or "z001" would let one request write a zone twice, the
    // later write silently replacing the earlier.
    let n: u8 = key
        .strip_prefix('z')
        .filter(|d| d.len() == 2 && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse().ok())
        .filter(|&n| (1..=v.zone_count()).contains(&n))
        .ok_or_else(|| format!("unknown zone {key:?} (expected z01–z{:02})", v.zone_count()))?;
    let rec = v.desired(ZoneState::REG, n).expect("checked in view");
    let z = ZoneState::decode(rec).map_err(|e| format!("current zone {n}: {e:?}"))?;
    let mut open = z.open;
    let mut percent = z.percent;
    let mut set = rec.payload[3];
    let mut touched = false;
    for (k, val) in fields {
        match k.as_str() {
            "state" => {
                open = match as_str(val, "state")? {
                    "open" => true,
                    "close" => false,
                    s => return Err(format!("zone state {s:?} (open or close)")),
                }
            }
            "value" => {
                percent = val
                    .as_u64()
                    .filter(|&p| p <= 100)
                    .ok_or("zone value must be 0–100")? as u8;
            }
            "setTemp" => set = as_temp(val, "zone setTemp")?,
            "name" => {
                app.names
                    .zones
                    .insert(n, sanitize_name(as_str(val, "zone name")?)?);
                continue;
            }
            // The official app sends whole zone objects: "number" must
            // match the key; "tempSensorClash" is not a change.
            "number" => {
                if val.as_u64() != Some(n as u64) {
                    return Err(format!("zones.{key}.number {val} does not match the zone"));
                }
                continue;
            }
            "tempSensorClash" => continue,
            other => return Err(format!("unsupported field zones.{key}.{other}")),
        }
        touched = true;
    }
    // Sensor and measured-temperature bytes are written as zero.
    Ok(touched.then(|| {
        write(
            v.unit,
            ZoneState::REG,
            [n, u8::from(open) << 7 | percent, 0, set, 0, 0, 0],
        )
    }))
}

/// The tablet's reply to a set request.
pub fn ack(request: &str, result: &Result<(), String>) -> Value {
    match result {
        Ok(()) => json!({ "ack": true, "request": request }),
        Err(reason) => json!({ "ack": false, "reason": reason, "request": request }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, TxMode};
    use crate::frame::encode;
    use std::time::Instant;

    /// Bus state for unit 07:abcde with 2 zones; zone 2 has a sensor.
    pub(crate) fn state() -> BusState {
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        let recs = [
            "0703abcde0602030300000000", // firmware, cb type 3
            "0703abcde0120020101000001", // 2 zones, constant zone 1, filter clean
            "0703abcde0501010330000100", // on, cool, high, 24 °C, fresh air off
            "0703abcde0301e40030000000", // zone 1 open 100 %, no sensor
            "0703abcde0302320130150300", // zone 2 closed 50 %, RF sensor, 21.3 °C
            "0703abcde0401006400000000", // zone 1 config: damper 0–100
            "0703abcde0402056402020050", // zone 2 config: motion 2, config 2, rssi 80
        ];
        b.on_bytes(
            &encode(&format!("getCAN 1 {}", recs.join(" "))),
            Instant::now(),
        );
        h.snapshot()
    }

    fn sd(st: &BusState, info: &ApiInfo) -> Result<Value, String> {
        system_data(st, info, Instant::now())
    }

    fn sa(query: &str, st: &BusState) -> Result<Vec<Record>, String> {
        set_aircon(query, st, Instant::now()).map(|p| p.writes)
    }

    /// Plan for a raw JSON body (wrapped as `json=…`); panics on rejection.
    fn plan(json: &str, st: &BusState) -> Plan {
        set_aircon(&q(json), st, Instant::now()).unwrap_or_else(|e| panic!("{json}: {e}"))
    }

    fn q(json: &str) -> String {
        format!("json={json}")
    }

    #[test]
    fn system_data_shape() {
        let d = sd(&state(), &ApiInfo::default()).unwrap();
        let info = &d["aircons"]["ac1"]["info"];
        assert_eq!(info["state"], "on");
        assert_eq!(info["mode"], "cool");
        assert_eq!(info["fan"], "high");
        assert_eq!(info["setTemp"], 24.0);
        assert_eq!(info["freshAirStatus"], "off");
        assert_eq!(info["filterCleanStatus"], 1);
        assert_eq!(info["constant1"], 1);
        assert_eq!(info["cbType"], 3);
        assert_eq!(info["countDownToOff"], 0);
        // Implemented tablet-side features are present; auto fan is not.
        assert_eq!(info["myAutoModeEnabled"], false);
        assert_eq!(info["climateControlModeEnabled"], false);
        assert_eq!(info["quietNightModeEnabled"], false);
        assert_eq!(info["myAutoCoolTargetTemp"], 24.0);
        assert!(info.get("aaAutoFanModeEnabled").is_none());
        let z1 = &d["aircons"]["ac1"]["zones"]["z01"];
        assert_eq!(
            (z1["state"].as_str(), z1["value"].as_u64()),
            (Some("open"), Some(100))
        );
        assert_eq!(
            (z1["type"].as_u64(), z1["rssi"].as_u64()),
            (Some(0), Some(0))
        );
        assert_eq!(z1["name"], "Zone 1");
        let z2 = &d["aircons"]["ac1"]["zones"]["z02"];
        assert_eq!(z2["state"], "close");
        assert_eq!(z2["type"], 1);
        assert_eq!(z2["measuredTemp"], 21.3);
        assert_eq!(z2["motionConfig"], 2);
        assert_eq!(z2["rssi"], 0x50);
        assert_eq!(d["system"]["rid"], "ductbridge");
        assert_eq!(d["system"]["sysType"], "MyAir5");
        assert!(d["myLights"]["lights"].as_object().unwrap().is_empty());
    }

    #[test]
    fn feature_settings_can_be_hidden() {
        let info = ApiInfo {
            advertise_features: false,
            ..ApiInfo::default()
        };
        let d = sd(&state(), &info).unwrap();
        let ac = &d["aircons"]["ac1"]["info"];
        for key in [
            "climateControlModeEnabled",
            "myAutoModeEnabled",
            "myAutoCoolTargetTemp",
            "myAutoHeatTargetTemp",
            "quietNightModeEnabled",
        ] {
            assert!(ac.get(key).is_none(), "{key}");
        }
        assert_eq!(ac["quietNightModeIsRunning"], false);
        let d = sd(&state(), &ApiInfo::default()).unwrap();
        assert_eq!(d["aircons"]["ac1"]["info"]["myAutoModeEnabled"], false);
    }

    #[test]
    fn names_come_from_config() {
        let info = ApiInfo {
            system_name: "Home".into(),
            system_id: "id1".into(),
            aircon_name: "Ducted".into(),
            sys_type: "e-zone".into(),
            my_app_rev: "15.2000".into(),
            aa_service_rev: "14.200".into(),
            zone_names: vec!["Living".into(), String::new()],
            advertise_features: true,
        };
        let d = sd(&state(), &info).unwrap();
        assert_eq!(d["system"]["name"], "Home");
        assert_eq!(d["system"]["sysType"], "e-zone");
        assert_eq!(d["system"]["myAppRev"], "15.2000");
        assert_eq!(d["system"]["aaServiceRev"], "14.200");
        assert_eq!(d["aircons"]["ac1"]["info"]["name"], "Ducted");
        assert_eq!(d["aircons"]["ac1"]["zones"]["z01"]["name"], "Living");
        assert_eq!(d["aircons"]["ac1"]["zones"]["z02"]["name"], "Zone 2");
    }

    /// Feed records to a listen-only bridge at time `t`.
    fn fed(t: Instant, recs: &str) -> BusState {
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, t);
        b.on_bytes(&encode(&format!("getCAN 1 {recs}")), t);
        h.snapshot()
    }

    #[test]
    fn not_ready_until_discovery_is_complete() {
        let t = Instant::now();
        let info = ApiInfo::default();
        let (_b, h) = Bridge::new(TxMode::ListenOnly, t);
        assert!(system_data(&h.snapshot(), &info, t).is_err());
        // Status only: zone count unknown.
        let st = fed(t, "0703abcde0501010330000100 0703abcde0301e40030000000");
        let e = system_data(&st, &info, t).unwrap_err();
        assert!(e.contains("register 01"), "{e}");
        // Register 01 says 2 zones but only zone 1 has reported.
        let st = fed(
            t,
            "0703abcde0120020100000000 0703abcde0501010330000100 0703abcde0301e40030000000 0703abcde0401006400000000",
        );
        let e = system_data(&st, &info, t).unwrap_err();
        assert!(e.contains("zone 2 state"), "{e}");
        // Even after the grace period, never a zone without its state.
        assert!(system_data(&st, &info, t + DISCOVERY_GRACE).is_err());
    }

    #[test]
    fn missing_zone_config_waits_then_falls_back() {
        let t = Instant::now();
        let info = ApiInfo::default();
        // All state, no register 04.
        let st = fed(
            t,
            "0703abcde0120010000000000 0703abcde0501010330000100 0703abcde0301e40030000000",
        );
        let e = system_data(&st, &info, t).unwrap_err();
        assert!(e.contains("register 04"), "{e}");
        let later = t + DISCOVERY_GRACE;
        let st = BusState {
            last_seen: st.last_seen.keys().map(|&u| (u, later)).collect(),
            ..st
        };
        let d = system_data(&st, &info, later).unwrap();
        assert_eq!(d["aircons"]["ac1"]["zones"]["z01"]["rssi"], 0);
    }

    #[test]
    fn offline_unit_is_not_served() {
        let st = state();
        let later = Instant::now() + crate::bridge::OFFLINE_WIRED;
        let e = system_data(&st, &ApiInfo::default(), later).unwrap_err();
        assert!(e.contains("offline"), "{e}");
        let e = set_aircon(&q(r#"{"ac1":{"info":{"state":"off"}}}"#), &st, later)
            .map(|p| p.writes)
            .unwrap_err();
        assert!(e.contains("offline"), "{e}");
    }

    #[test]
    fn partial_updates_build_on_pending_values() {
        let mut st = state();
        let now = Instant::now();
        let w = set_aircon(&q(r#"{"ac1":{"info":{"state":"off"}}}"#), &st, now)
            .map(|p| p.writes)
            .unwrap();
        st.pending.insert(
            crate::session::key_of(&w[0]),
            crate::bridge::Pending {
                record: w[0],
                seq: 1,
                accepted: now,
                confirmed: false,
            },
        );
        let w2 = set_aircon(&q(r#"{"ac1":{"info":{"mode":"heat"}}}"#), &st, now)
            .map(|p| p.writes)
            .unwrap();
        // Off from the first request is kept.
        assert_eq!(w2[0].to_string(), "0701abcde0500020330000100");
        // getSystemData still shows the confirmed value.
        let d = system_data(&st, &ApiInfo::default(), now).unwrap();
        assert_eq!(d["aircons"]["ac1"]["info"]["state"], "on");
        // A stale pending value is ignored.
        let late = now + crate::bridge::PENDING_TTL;
        let w3 = set_aircon(&q(r#"{"ac1":{"info":{"mode":"heat"}}}"#), &st, late).map(|p| p.writes);
        // (unit offline by then only if past 80 s; PENDING_TTL is 30 s)
        assert_eq!(w3.unwrap()[0].to_string(), "0701abcde0501020330000100");
    }

    #[test]
    fn set_info_merges_over_current_status() {
        let w = sa(
            &q(r#"{"aircons":{"ac1":{"info":{"state":"off","mode":"heat"}}}}"#),
            &state(),
        )
        .unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].to_string(), "0701abcde0500020330000100");
        // Python-client form, URL-encoded.
        let w = sa(
            "json=%7B%22ac1%22%3A+%7B%22info%22%3A+%7B%22setTemp%22%3A+22.5%2C+%22fan%22%3A+%22autoAA%22%7D%7D%7D",
            &state(),
        )
        .unwrap();
        assert_eq!(w[0].to_string(), "0701abcde050101052d000100");
    }

    #[test]
    fn raw_plus_is_literal_but_encoded_form_still_works() {
        let w = sa(&q(r#"{"ac1":{"info":{"setTemp":2.4e+1}}}"#), &state()).unwrap();
        assert_eq!(w[0].payload[3], 48);
        // Form-encoded: '+' is a space and the JSON is percent-encoded.
        let w = sa(
            "json=%7B%22ac1%22%3A+%7B%22info%22%3A+%7B%22setTemp%22%3A+2.4e%2B1%7D%7D%7D",
            &state(),
        )
        .unwrap();
        assert_eq!(w[0].payload[3], 48);
    }

    #[test]
    fn set_zones() {
        let w = sa(
            &q(r#"{"aircons": { "ac1": { "zones" : { "z01": { "value": 80 }, "z02": {"state":"open","setTemp":23} } } } }"#),
            &state(),
        )
        .unwrap();
        let s: Vec<String> = w.iter().map(|r| r.to_string()).collect();
        // Zone 1 keeps its raw set temperature byte; sensor/measured bytes zeroed.
        assert_eq!(
            s,
            ["0701abcde0301d00030000000", "0701abcde0302b2002e000000"]
        );
        for r in &w {
            ZoneState::decode(r).unwrap();
        }
    }

    #[test]
    fn my_zone_and_fresh_air() {
        let w = sa(
            &q(r#"{"ac1":{"info":{"myZone":2,"freshAirStatus":"on"}}}"#),
            &state(),
        )
        .unwrap();
        assert_eq!(w[0].to_string(), "0701abcde0501010330020200");
        let e = sa(&q(r#"{"ac1":{"info":{"myZone":1}}}"#), &state()).unwrap_err();
        assert!(e.contains("no temperature sensor"), "{e}");
    }

    #[test]
    fn rejects_bad_or_unsupported_requests() {
        let st = state();
        for (json, needle) in [
            (r#"{"ac1":{"info":{"setTemp":33}}}"#, "outside"),
            (r#"{"ac1":{"info":{"setTemp":"24"}}}"#, "number"),
            (r#"{"ac1":{"info":{"fan":"off"}}}"#, "fan"),
            (r#"{"ac1":{"info":{"countDownToOff":721}}}"#, "0–720"),
            (r#"{"ac1":{"info":{"countDownToOn":"soon"}}}"#, "0–720"),
            (r#"{"ac1":{"info":{"countDownToOn":-1}}}"#, "0–720"),
            (r#"{"ac1":{"info":{"myAutoCoolTargetTemp":21}}}"#, "targets"),
            (
                r#"{"ac1":{"info":{"myAutoModeEnabled":"yes"}}}"#,
                "true or false",
            ),
            (
                r#"{"ac1":{"info":{"aaAutoFanModeEnabled":true}}}"#,
                "unsupported",
            ),
            (
                r#"{"ac1":{"info":{"filterCleanStatus":1}}}"#,
                "only be set to 0",
            ),
            (r#"{"ac1":{"info":{"name":"  "}}}"#, "empty"),
            (
                r#"{"ac1":{"zones":{"z01":{"number":2}}}}"#,
                "does not match",
            ),
            (r#"{"system":{"rid":"x"}}"#, "unsupported field system.rid"),
            (r#"{"ac2":{"info":{"state":"on"}}}"#, "unknown aircon"),
            (r#"{"ac1":{"zones":{"z03":{"value":50}}}}"#, "unknown zone"),
            // Aliases of z01 are refused, so a zone cannot be written twice.
            (r#"{"ac1":{"zones":{"z1":{"value":50}}}}"#, "unknown zone"),
            (r#"{"ac1":{"zones":{"z001":{"value":50}}}}"#, "unknown zone"),
            (
                r#"{"ac1":{"zones":{"z1":{"value":50},"z01":{"setTemp":20}}}}"#,
                "unknown zone",
            ),
            (r#"{"ac1":{"zones":{"z+1":{"value":50}}}}"#, "unknown zone"),
            (r#"{"ac1":{"zones":{"z01":{"value":101}}}}"#, "0–100"),
            (r#"{"ac1":{"zones":{"z01":{"motion":1}}}}"#, "unsupported"),
            // All-or-nothing: a good field does not rescue a bad one.
            (
                r#"{"ac1":{"info":{"state":"on"},"zones":{"z09":{"value":5}}}}"#,
                "unknown zone",
            ),
            ("{not json", "invalid JSON"),
            (
                r#"{"aircons":{"ac1":{"info":{"state":"off"}}},"ac2":{"info":{"state":"on"}}}"#,
                "next to aircons",
            ),
        ] {
            let e = sa(&q(json), &st).unwrap_err();
            assert!(e.contains(needle), "{json}: {e}");
        }
        assert!(sa("x=1", &st).unwrap_err().contains("missing json"));
    }

    #[test]
    fn discovery_fields_for_the_official_app() {
        let d = sd(&state(), &ApiInfo::default()).unwrap();
        let sys = &d["system"];
        for k in ["mid", "name", "myAppRev"] {
            assert!(sys[k].is_string(), "system.{k} missing: {sys}");
        }
        assert_eq!(sys["mid"], "ductbridge");
        let rev = sys["myAppRev"].as_str().unwrap();
        let major: u32 = rev.split('.').next().unwrap().parse().unwrap();
        assert_eq!(major, 15);
        // Current tablet revisions: no "upgrade available" prompt.
        assert_eq!(rev, "15.1548");
        assert_eq!(sys["aaServiceRev"], "14.118");
    }

    #[test]
    fn my_app_rev_is_checked() {
        assert!(check_my_app_rev(DEFAULT_MY_APP_REV).is_ok());
        assert!(check_my_app_rev("13.0").is_ok());
        for bad in [
            "",
            "15",
            "12.999",
            "x.1",
            "15.",
            "15.1a",
            "15.0+ductbridge.0.1.0",
        ] {
            assert!(check_my_app_rev(bad).is_err(), "{bad:?}");
        }
    }

    fn uids(d: &Value) -> Vec<String> {
        let acs = d["aircons"].as_object().expect("aircons object");
        assert!(!acs.is_empty());
        acs.iter()
            .map(|(k, ac)| {
                let uid = ac["info"]["uid"].as_str();
                let uid = uid.unwrap_or_else(|| panic!("{k}.info.uid missing or not a string"));
                assert!(
                    uid.len() == 5
                        && uid
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "{k}.info.uid {uid:?} is not 5 lowercase hex digits"
                );
                uid.to_string()
            })
            .collect()
    }

    #[test]
    fn every_aircon_has_a_five_hex_digit_uid() {
        // The official app crashes ("Uid null") without info.uid.
        assert_eq!(uids(&sd(&state(), &ApiInfo::default()).unwrap()), ["abcde"]);
        // Small ids are zero-padded, matching the record's characters 4–8.
        let t = Instant::now();
        let st = fed(
            t,
            "0703000a10120010100000000 0703000a10501010330000100 0703000a10301e40030000000 0703000a10401006400000000",
        );
        assert_eq!(
            uids(&system_data(&st, &ApiInfo::default(), t).unwrap()),
            ["000a1"]
        );
        // Unit not known yet: 503, never a missing or empty uid.
        let (_b, h) = Bridge::new(TxMode::ListenOnly, t);
        assert!(system_data(&h.snapshot(), &ApiInfo::default(), t).is_err());
    }

    #[test]
    fn app_shaped_zone_payload_is_accepted() {
        // The official app sends whole zone objects, setTemp as a float.
        let w = sa(
            &q(r#"{"ac1":{"zones":{"z02":{"number":2,"tempSensorClash":false,"state":"open","setTemp":23.0}}}}"#),
            &state(),
        )
        .unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].to_string(), "0701abcde0302b2002e000000");
        // Only extras, no change: no write at all.
        let w = sa(
            &q(r#"{"ac1":{"zones":{"z01":{"number":1,"tempSensorClash":false}}}}"#),
            &state(),
        )
        .unwrap();
        assert!(w.is_empty());
        let w = sa(&q(r#"{"ac1":{"info":{"setTemp":24.0}}}"#), &state()).unwrap();
        assert_eq!(w[0].payload[3], 48);
    }

    #[test]
    fn timers_accept_numbers_and_numeric_strings() {
        let p = plan(
            r#"{"ac1":{"info":{"countDownToOff":"30","countDownToOn":0}}}"#,
            &state(),
        );
        assert!(p.writes.is_empty());
        let app = p.app.unwrap();
        assert_eq!((app.count_down_to_off, app.count_down_to_on), (30, 0));
        let mut st = state();
        st.app = app;
        let d = sd(&st, &ApiInfo::default()).unwrap();
        assert_eq!(d["aircons"]["ac1"]["info"]["countDownToOff"], 30);
        // Setting one does not clear the other (unverified).
        let app = plan(r#"{"ac1":{"info":{"countDownToOn":5}}}"#, &st)
            .app
            .unwrap();
        assert_eq!((app.count_down_to_on, app.count_down_to_off), (5, 30));
    }

    #[test]
    fn renames_override_config_names() {
        let info = ApiInfo {
            zone_names: vec!["Living".into(), "Bed".into()],
            ..ApiInfo::default()
        };
        let mut st = state();
        let p = plan(
            r#"{"system":{"name":"Home\u0007"},"ac1":{"info":{"name":"Ducted"},"zones":{"z02":{"name":"Study"}}}}"#,
            &st,
        );
        assert!(p.writes.is_empty());
        st.app = p.app.unwrap();
        let d = sd(&st, &info).unwrap();
        assert_eq!(d["system"]["name"], "Home");
        assert_eq!(d["aircons"]["ac1"]["info"]["name"], "Ducted");
        assert_eq!(d["aircons"]["ac1"]["zones"]["z01"]["name"], "Living");
        assert_eq!(d["aircons"]["ac1"]["zones"]["z02"]["name"], "Study");
        // Wrapped form with system, and setMySystem.
        let p = plan(r#"{"aircons":{},"system":{"name":"Flat"}}"#, &st);
        assert_eq!(p.app.unwrap().names.system.as_deref(), Some("Flat"));
        for body in [r#"{"name":"Unit 2"}"#, r#"{"system":{"name":"Unit 2"}}"#] {
            let p = set_my_system(&q(body), &st).unwrap();
            assert_eq!(p.app.unwrap().names.system.as_deref(), Some("Unit 2"));
            assert!(p.log[0].contains("unverified"));
        }
        assert!(sa(
            &q(&format!(
                r#"{{"ac1":{{"info":{{"name":"{}"}}}}}}"#,
                "x".repeat(33)
            )),
            &st
        )
        .is_err());
    }

    #[test]
    fn filter_reset_is_accepted_as_a_no_op() {
        for v in ["0", "\"0\""] {
            let p = plan(
                &format!(r#"{{"ac1":{{"info":{{"filterCleanStatus":{v}}}}}}}"#),
                &state(),
            );
            assert!(p.writes.is_empty() && p.app.is_none());
            assert_eq!(
                p.log,
                ["filter reset requested; not yet supported on the bus"]
            );
        }
    }

    #[test]
    fn myauto_is_a_tablet_side_mode() {
        let mut st = state(); // on, cool
        let p = plan(
            r#"{"ac1":{"info":{"myAutoModeEnabled":true,"mode":"myauto","myAutoCoolTargetTemp":25,"myAutoHeatTargetTemp":19}}}"#,
            &st,
        );
        // The box only ever sees cool or heat.
        assert_eq!(p.writes[0].payload[1], 1);
        st.app = p.app.unwrap();
        assert!(st.app.my_auto_selected && st.app.my_auto_cooling);
        let d = sd(&st, &ApiInfo::default()).unwrap();
        let info = &d["aircons"]["ac1"]["info"];
        assert_eq!(info["mode"], "myauto");
        assert_eq!(info["myAutoModeCurrentSetMode"], "cool");
        assert_eq!(info["myAutoModeIsRunning"], true);
        assert_eq!(info["myAutoCoolTargetTemp"], 25.0);
        // Another mode leaves MyAuto.
        let app = plan(r#"{"ac1":{"info":{"mode":"heat"}}}"#, &st)
            .app
            .unwrap();
        assert!(!app.my_auto_selected);
        // Disabling it too.
        let app = plan(r#"{"ac1":{"info":{"myAutoModeEnabled":false}}}"#, &st)
            .app
            .unwrap();
        assert!(!app.my_auto_selected);
        // Selecting mode myauto alone enables it (the official app may not
        // send myAutoModeEnabled first).
        let app = plan(r#"{"ac1":{"info":{"mode":"myauto"}}}"#, &state())
            .app
            .unwrap();
        assert!(app.my_auto_enabled && app.my_auto_selected);
    }

    #[test]
    fn client_writes_drop_quiet_night_steps_for_those_registers() {
        use crate::state::{Step, Transition};
        let mut st = state();
        st.app.quiet_night_transition = Some(Transition {
            steps: vec![
                Step {
                    register: 3,
                    zone: 2,
                    target: [2, 0x32, 0, 32, 0, 0, 0],
                    from: [2, 0x32, 1, 48, 21, 3, 0],
                },
                Step {
                    register: 5,
                    zone: 0,
                    target: [1, 3, 3, 32, 0, 1, 0],
                    from: [1, 1, 3, 48, 0, 1, 0],
                },
            ],
            attempts: 1,
            restore: false,
        });
        // The user sets zone 2 back to 24 °C: quiet night must not retry 16 °C.
        let app = plan(r#"{"ac1":{"zones":{"z02":{"setTemp":24}}}}"#, &st)
            .app
            .unwrap();
        let steps = app.quiet_night_transition.unwrap().steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].register, 5);
        // A status write drops the last step: transition gone.
        st.app.quiet_night_transition = Some(Transition {
            steps,
            attempts: 1,
            restore: false,
        });
        let app = plan(r#"{"ac1":{"info":{"fan":"low"}}}"#, &st).app.unwrap();
        assert!(app.quiet_night_transition.is_none());
    }

    #[test]
    fn query_decoding_is_lenient() {
        assert_eq!(
            query_param("a=1&json={\"x\": 1}", "json").unwrap(),
            "{\"x\": 1}"
        );
        assert_eq!(query_param("json=%7B%7D", "json").unwrap(), "{}");
        assert_eq!(query_param("json=100%", "json").unwrap(), "100%");
        assert_eq!(query_param("json=%zz", "json").unwrap(), "%zz");
        assert_eq!(query_param("json=%e2%82%ac%", "json").unwrap(), "€%");
        assert_eq!(query_param("json=é%4", "json").unwrap(), "é%4");
        assert_eq!(query_param("json", "json").unwrap(), "");
        assert!(query_param("other=1", "json").is_none());
    }

    #[test]
    fn ack_shape() {
        assert_eq!(
            ack("setAircon", &Ok(())),
            json!({"ack": true, "request": "setAircon"})
        );
        assert_eq!(ack("setAircon", &Err("no".into()))["reason"], "no");
    }
}
