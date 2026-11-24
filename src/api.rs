//! The wall tablet's local HTTP API (port 2025), as pure functions.
//!
//! Shapes follow the aa_interop `docs/tablet_api/README.md` and what Home
//! Assistant's `advantage_air` integration reads and sends. Names (system,
//! aircon, zones) are not on the bus — the original tablet stores them — so
//! they come from configuration.
//!
//! Tablet-side features (MyAuto, MyTemp, auto fan, night mode, timers, lights
//! and "things") are not offered: their keys are left out of `getSystemData`
//! so clients do not expose them, and `setAircon` rejects them.

use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::bridge::BusState;
use crate::record::{Record, DEST_CB};
use crate::registers::{
    decode_error, Firmware, Mode, SystemStatus, ZoneConfig, ZoneInfo, ZoneState, MAX_SET_TEMP,
    MAX_ZONES, MIN_SET_TEMP,
};

/// Names and identity reported to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiInfo {
    pub system_name: String,
    /// Stable id; Home Assistant uses it as the device's unique id.
    pub system_id: String,
    pub aircon_name: String,
    /// Zone names, zone 1 first. Missing ones default to "Zone N".
    pub zone_names: Vec<String>,
}

impl Default for ApiInfo {
    fn default() -> Self {
        ApiInfo {
            system_name: "ductbridge".into(),
            system_id: "ductbridge".into(),
            aircon_name: "Aircon".into(),
            zone_names: Vec::new(),
        }
    }
}

impl ApiInfo {
    fn zone_name(&self, n: u8) -> String {
        self.zone_names
            .get(n as usize - 1)
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Zone {n}"))
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
struct View<'a> {
    st: &'a BusState,
    unit: (u8, u32),
    now: Instant,
}

impl View<'_> {
    fn reg(&self, reg: u8, zone: u8) -> Option<&Record> {
        self.st
            .registers
            .get(&(self.unit.0, self.unit.1, reg, zone))
    }
    fn status(&self) -> Option<(SystemStatus, &Record)> {
        let r = self.reg(SystemStatus::REG, 0)?;
        SystemStatus::decode(r).ok().map(|s| (s, r))
    }
    fn zone(&self, n: u8) -> Option<(ZoneState, &Record)> {
        let r = self.reg(ZoneState::REG, n)?;
        ZoneState::decode(r).ok().map(|z| (z, r))
    }
    /// Value a new partial write builds on: pending desired value if any,
    /// else the confirmed register.
    fn desired(&self, reg: u8, zone: u8) -> Option<&Record> {
        self.st
            .desired(&(self.unit.0, self.unit.1, reg, zone), self.now)
    }
    fn info(&self) -> Option<ZoneInfo> {
        self.reg(ZoneInfo::REG, 0)
            .and_then(|r| ZoneInfo::decode(r).ok())
    }
    /// Number of zones: from register 01 if seen, else the highest zone
    /// number with a register 03.
    fn zone_count(&self) -> u8 {
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
fn view(st: &BusState, now: Instant) -> Result<View<'_>, String> {
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
                "name": info.zone_name(n),
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
    Ok(json!({
        "aircons": {
            "ac1": {
                "info": {
                    "name": info.aircon_name,
                    "state": if s.on { "on" } else { "off" },
                    "mode": mode_name(s.mode),
                    "fan": FAN_NAMES[s.fan as usize],
                    "setTemp": temp(s.set_temp),
                    "myZone": s.my_zone,
                    "freshAirStatus": FRESH_AIR_NAMES[s.fresh_air.min(2) as usize],
                    "filterCleanStatus": u8::from(zi.is_some_and(|i| i.filter_clean)),
                    "constant1": consts[0],
                    "constant2": consts[1],
                    "constant3": consts[2],
                    "noOfZones": v.zone_count(),
                    "countDownToOn": 0,
                    "countDownToOff": 0,
                    "airconErrorCode": error,
                    "cbType": cb_type,
                },
                "zones": zones,
            }
        },
        "myLights": { "lights": {} },
        "myThings": { "things": {} },
        "system": {
            "name": info.system_name,
            "rid": info.system_id,
            "sysType": "ductbridge",
            "myAppRev": env!("CARGO_PKG_VERSION"),
            "needsUpdate": false,
            "noOfAircons": 1,
            "hasAircons": true,
            "hasLights": false,
            "hasSensors": false,
            "hasThings": false,
            "hasThingsBOG": false,
            "hasThingsLight": false,
        }
    }))
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

fn write(unit: (u8, u32), register: u8, payload: [u8; 7]) -> Record {
    Record {
        unit_type: unit.0,
        dest: DEST_CB,
        uid: unit.1,
        register,
        payload,
    }
}

/// `GET /setAircon?json=…`: turn the request into register writes, merged
/// over the current register values. All-or-nothing: any unsupported or
/// out-of-range field rejects the whole request.
///
/// Accepts both `{"aircons":{"ac1":…}}` (documented form) and `{"ac1":…}`
/// (what the `advantage_air` Python client sends).
/// Partial updates build on pending (accepted but unconfirmed) values, so
/// back-to-back requests do not undo each other; run it inside
/// [`Handle::submit`](crate::bridge::Handle::submit) for that to hold.
pub fn set_aircon(query: &str, st: &BusState, now: Instant) -> Result<Vec<Record>, String> {
    let req = json_param(query)?;
    let root = as_object(&req, "request")?;
    let acs = match root.get("aircons") {
        Some(a) => {
            if let Some(k) = root.keys().find(|k| *k != "aircons") {
                return Err(format!("unsupported key {k:?} next to aircons"));
            }
            as_object(a, "aircons")?
        }
        None => root,
    };
    let v = view(st, now)?;
    let mut out = Vec::new();
    for (ac, body) in acs {
        if ac != "ac1" {
            return Err(format!("unknown aircon {ac:?} (only ac1)"));
        }
        for (part, val) in as_object(body, ac)? {
            match part.as_str() {
                "info" => out.extend(info_write(&v, as_object(val, "info")?)?),
                "zones" => {
                    for (zk, zb) in as_object(val, "zones")? {
                        out.push(zone_write(&v, zk, as_object(zb, zk)?)?);
                    }
                }
                other => return Err(format!("unsupported section {other:?}")),
            }
        }
    }
    Ok(out)
}

fn info_write(v: &View, fields: &Map<String, Value>) -> Result<Option<Record>, String> {
    if fields.is_empty() {
        return Ok(None);
    }
    let rec = v.desired(SystemStatus::REG, 0).expect("checked in view");
    let cur = SystemStatus::decode(rec).map_err(|e| format!("current status: {e:?}"))?;
    let mut p = rec.payload;
    p[0] = u8::from(cur.on);
    p[1] = cur.mode as u8;
    for (k, val) in fields {
        match k.as_str() {
            "state" => {
                p[0] = match as_str(val, "state")? {
                    "on" => 1,
                    "off" => 0,
                    s => return Err(format!("state {s:?} (on or off)")),
                }
            }
            "mode" => {
                let s = as_str(val, "mode")?;
                p[1] = mode_from_name(s)
                    .ok_or_else(|| format!("mode {s:?} (cool, heat, vent, dry or auto)"))?;
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
            other => return Err(format!("unsupported field info.{other}")),
        }
    }
    Ok(Some(write(v.unit, SystemStatus::REG, p)))
}

fn zone_write(v: &View, key: &str, fields: &Map<String, Value>) -> Result<Record, String> {
    let n: u8 = key
        .strip_prefix('z')
        .and_then(|d| d.parse().ok())
        .filter(|&n| (1..=v.zone_count()).contains(&n))
        .ok_or_else(|| format!("unknown zone {key:?}"))?;
    let rec = v.desired(ZoneState::REG, n).expect("checked in view");
    let z = ZoneState::decode(rec).map_err(|e| format!("current zone {n}: {e:?}"))?;
    let mut open = z.open;
    let mut percent = z.percent;
    let mut set = rec.payload[3];
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
            other => return Err(format!("unsupported field zones.{key}.{other}")),
        }
    }
    // Sensor and measured-temperature bytes are written as zero.
    Ok(write(
        v.unit,
        ZoneState::REG,
        [n, u8::from(open) << 7 | percent, 0, set, 0, 0, 0],
    ))
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
        set_aircon(query, st, Instant::now())
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
        // Tablet-only features are absent, so clients don't offer them.
        assert!(info.get("myAutoModeEnabled").is_none());
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
        assert_eq!(d["system"]["sysType"], "ductbridge");
        assert!(d["myLights"]["lights"].as_object().unwrap().is_empty());
    }

    #[test]
    fn names_come_from_config() {
        let info = ApiInfo {
            system_name: "Home".into(),
            system_id: "id1".into(),
            aircon_name: "Ducted".into(),
            zone_names: vec!["Living".into(), String::new()],
        };
        let d = sd(&state(), &info).unwrap();
        assert_eq!(d["system"]["name"], "Home");
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
        let e = set_aircon(&q(r#"{"ac1":{"info":{"state":"off"}}}"#), &st, later).unwrap_err();
        assert!(e.contains("offline"), "{e}");
    }

    #[test]
    fn partial_updates_build_on_pending_values() {
        let mut st = state();
        let now = Instant::now();
        let w = set_aircon(&q(r#"{"ac1":{"info":{"state":"off"}}}"#), &st, now).unwrap();
        st.pending.insert(
            crate::session::key_of(&w[0]),
            crate::bridge::Pending {
                record: w[0],
                seq: 1,
                accepted: now,
                confirmed: false,
            },
        );
        let w2 = set_aircon(&q(r#"{"ac1":{"info":{"mode":"heat"}}}"#), &st, now).unwrap();
        // Off from the first request is kept.
        assert_eq!(w2[0].to_string(), "0701abcde0500020330000100");
        // getSystemData still shows the confirmed value.
        let d = system_data(&st, &ApiInfo::default(), now).unwrap();
        assert_eq!(d["aircons"]["ac1"]["info"]["state"], "on");
        // A stale pending value is ignored.
        let late = now + crate::bridge::PENDING_TTL;
        let w3 = set_aircon(&q(r#"{"ac1":{"info":{"mode":"heat"}}}"#), &st, late);
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
            (r#"{"ac1":{"info":{"mode":"myauto"}}}"#, "mode"),
            (r#"{"ac1":{"info":{"fan":"off"}}}"#, "fan"),
            (
                r#"{"ac1":{"info":{"countDownToOff":10}}}"#,
                "unsupported field info.countDownToOff",
            ),
            (
                r#"{"ac1":{"info":{"myAutoModeEnabled":true}}}"#,
                "unsupported",
            ),
            (r#"{"ac2":{"info":{"state":"on"}}}"#, "unknown aircon"),
            (r#"{"ac1":{"zones":{"z03":{"value":50}}}}"#, "unknown zone"),
            (r#"{"ac1":{"zones":{"z01":{"value":101}}}}"#, "0–100"),
            (r#"{"ac1":{"zones":{"z01":{"name":"x"}}}}"#, "unsupported"),
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
