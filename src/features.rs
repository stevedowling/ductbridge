//! Tablet-side control features, as pure functions over the bus snapshot,
//! the tablet-side state and the time. Each returns a [`Plan`]: ordinary
//! register-05 / register-03 writes for the session queue, plus the new
//! tablet-side state. The daemon calls [`evaluate`] every 30 s and whenever
//! the state changes, and [`tick_timers`] once a minute.
//!
//! - On/off timers: count down once a minute; at zero turn the unit on/off.
//! - Climate control: keep MyZone on the open sensor zone furthest from its
//!   setpoint in the direction of the current mode.
//! - MyAuto: switch between cool and heat to keep sensor zones within the
//!   cool/heat targets; reported as mode "myauto".
//! - Quiet night: between 22:00 and 06:00, while cooling, switch to fan only
//!   and restore the previous settings at 06:00.
//!
//! Behaviour follows observed behaviour of the original tablet; items marked
//! "unverified" are logged when they act, to be confirmed on install day.

use std::time::Instant;

use crate::api::{view, write, View};
use crate::bridge::{BusState, Plan};
use crate::registers::{SystemStatus, ZoneState};
use crate::state::{targets_valid, AppState, Saved, DEFAULT_COOL_TARGET, DEFAULT_HEAT_TARGET};

const MODE_COOL: u8 = 1;
const MODE_HEAT: u8 = 2;
const MODE_VENT: u8 = 3;
/// 16 °C on the wire (°C × 2).
const QUIET_SET_TEMP: u8 = 32;
/// Quiet night window, local time, minutes after midnight.
pub const QUIET_START: u16 = 22 * 60;
pub const QUIET_END: u16 = 6 * 60;

fn wire(t: f32) -> u8 {
    (t * 2.0).round() as u8
}

/// Which features are currently in control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Running {
    pub climate_control: bool,
    pub my_auto: bool,
    pub quiet_night: bool,
}

/// Is the unit on, per the desired (pending or confirmed) status?
fn desired_status(v: &View) -> Option<[u8; 7]> {
    v.desired(SystemStatus::REG, 0).map(|r| r.payload)
}

pub fn running(st: &BusState, now: Instant) -> Running {
    let Ok(v) = view(st, now) else {
        return Running {
            quiet_night: st.app.quiet_night_running,
            ..Running::default()
        };
    };
    running_in(&v, &st.app)
}

fn running_in(v: &View, app: &AppState) -> Running {
    let p = desired_status(v).unwrap_or_default();
    let on = p[0] == 1;
    let quiet = app.quiet_night_running;
    let my_auto = app.my_auto_enabled && app.my_auto_selected && on && !quiet;
    let climate = app.climate_control_enabled
        && on
        && matches!(p[1], MODE_COOL | MODE_HEAT)
        && !quiet
        && !my_auto;
    Running {
        climate_control: climate,
        my_auto,
        quiet_night: quiet,
    }
}

/// A zone as the features see it.
#[derive(Debug, Clone, Copy)]
struct Zone {
    n: u8,
    /// Desired payload (pending write or confirmed report).
    p: [u8; 7],
    has_sensor: bool,
    measured: Option<f32>,
}

impl Zone {
    fn open(&self) -> bool {
        self.p[1] & 0x80 != 0
    }
    fn set_temp(&self) -> f32 {
        self.p[3] as f32 / 2.0
    }
}

fn zones(v: &View) -> Vec<Zone> {
    (1..=v.zone_count())
        .filter_map(|n| {
            // Sensor and measurement come from the box's report; open,
            // damper and setpoint from the desired value.
            let (z, _) = v.zone(n)?;
            let p = v.desired(ZoneState::REG, n)?.payload;
            Some(Zone {
                n,
                p,
                has_sensor: z.sensor_type > 0,
                measured: z.measured_temp,
            })
        })
        .collect()
}

fn zone_write(v: &View, n: u8, open: bool, percent: u8, set: u8) -> crate::record::Record {
    write(
        v.unit,
        ZoneState::REG,
        [n, u8::from(open) << 7 | (percent & 0x7f), 0, set, 0, 0, 0],
    )
}

fn in_quiet_window(minute: u16) -> bool {
    !(QUIET_END..QUIET_START).contains(&minute)
}

/// Periodic evaluation of quiet night, MyAuto and climate control.
/// `minute` is local time in minutes after midnight.
pub fn evaluate(st: &BusState, now: Instant, minute: u16) -> Plan {
    let Ok(v) = view(st, now) else {
        return Plan::default();
    };
    let mut app = st.app.clone();
    let mut plan = Plan::default();
    quiet_night(&v, &mut app, minute, &mut plan);
    let r = running_in(&v, &app);
    if r.my_auto {
        my_auto(&v, &mut app, &mut plan);
    } else if r.climate_control {
        climate_control(&v, &mut plan);
    }
    if app != st.app {
        plan.app = Some(app);
    }
    plan
}

fn quiet_night(v: &View, app: &mut AppState, minute: u16, plan: &mut Plan) {
    let Some(p) = desired_status(v) else { return };
    let in_window = in_quiet_window(minute);
    if app.quiet_night_running && (!app.quiet_night_enabled || !in_window) {
        // Restore what quiet night replaced; on/off stays as it is now
        // (whether the original also turns the unit off is unverified).
        if let Some(saved) = app.quiet_night_saved.take() {
            let mut s = p;
            s[1..6].copy_from_slice(&saved.status[1..6]);
            if s != p {
                plan.writes.push(write(v.unit, SystemStatus::REG, s));
            }
            for z in zones(v) {
                if let Some((_, sp)) = saved.zones.iter().find(|(n, _)| *n == z.n) {
                    if sp[1] != z.p[1] || sp[3] != z.p[3] {
                        plan.writes
                            .push(zone_write(v, z.n, sp[1] & 0x80 != 0, sp[1], sp[3]));
                    }
                }
            }
        }
        app.quiet_night_running = false;
        plan.log.push(
            "quiet night: ended, previous settings restored (unit left on/off as is; unverified)"
                .into(),
        );
        return;
    }
    // Start while in the window and cooling (cool, or MyAuto cooling, which
    // is "cool" on the bus). Exact trigger unverified.
    if !app.quiet_night_running
        && app.quiet_night_enabled
        && in_window
        && p[0] == 1
        && p[1] == MODE_COOL
    {
        let zs = zones(v);
        app.quiet_night_saved = Some(Saved {
            status: p,
            zones: zs.iter().map(|z| (z.n, z.p)).collect(),
        });
        app.quiet_night_running = true;
        let mut s = p;
        s[1] = MODE_VENT;
        if s[4] == 0 {
            s[3] = QUIET_SET_TEMP;
        }
        plan.writes.push(write(v.unit, SystemStatus::REG, s));
        for z in zs {
            if z.has_sensor && z.p[3] != QUIET_SET_TEMP {
                plan.writes
                    .push(zone_write(v, z.n, z.open(), z.p[1], QUIET_SET_TEMP));
            } else if !z.has_sensor && z.open() && z.p[1] & 0x7f != 100 {
                plan.writes.push(zone_write(v, z.n, true, 100, z.p[3]));
            }
        }
        plan.log
            .push("quiet night: started, fan only until 06:00 (trigger unverified)".into());
    }
}

fn climate_control(v: &View, plan: &mut Plan) {
    let Some(p) = desired_status(v) else { return };
    let heating = p[1] == MODE_HEAT;
    let best = zones(v)
        .into_iter()
        .filter(|z| z.has_sensor && z.open())
        .filter_map(|z| {
            let m = z.measured?;
            let err = if heating {
                z.set_temp() - m
            } else {
                m - z.set_temp()
            };
            (err > 0.0).then_some((z.n, err))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((n, err)) = best {
        if p[4] != n {
            let mut s = p;
            s[4] = n;
            plan.writes.push(write(v.unit, SystemStatus::REG, s));
            plan.log.push(format!(
                "climate control: MyZone {} -> {n} ({err:.1} °C from setpoint)",
                p[4]
            ));
        }
    }
}

fn my_auto(v: &View, app: &mut AppState, plan: &mut Plan) {
    let Some(p) = desired_status(v) else { return };
    if !targets_valid(app.my_auto_cool_target, app.my_auto_heat_target) {
        app.my_auto_cool_target = DEFAULT_COOL_TARGET;
        app.my_auto_heat_target = DEFAULT_HEAT_TARGET;
        plan.log
            .push("MyAuto: invalid targets reset to cool 24 / heat 20".into());
    }
    let (cool_t, heat_t) = (app.my_auto_cool_target, app.my_auto_heat_target);
    let sensed: Vec<(Zone, f32)> = zones(v)
        .into_iter()
        .filter(|z| z.has_sensor && z.open())
        .filter_map(|z| Some((z, z.measured?)))
        .collect();
    let hottest = sensed
        .iter()
        .filter(|(_, m)| *m >= cool_t + 0.5)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(z, _)| z.n);
    let coldest = sensed
        .iter()
        .filter(|(_, m)| *m < heat_t)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(z, _)| z.n);
    let choice = match (hottest, coldest) {
        (Some(h), Some(c)) => {
            plan.log.push(
                "MyAuto: zones both too hot and too cold; staying in the current mode (unverified)"
                    .into(),
            );
            Some(if app.my_auto_cooling {
                (true, h)
            } else {
                (false, c)
            })
        }
        (Some(h), None) => Some((true, h)),
        (None, Some(c)) => Some((false, c)),
        (None, None) => None,
    };
    if let Some((cooling, _)) = choice {
        if cooling != app.my_auto_cooling {
            plan.log.push(format!(
                "MyAuto: switching to {}",
                if cooling { "cool" } else { "heat" }
            ));
        }
        app.my_auto_cooling = cooling;
    }
    let target = wire(if app.my_auto_cooling { cool_t } else { heat_t });
    let mut s = p;
    s[1] = if app.my_auto_cooling {
        MODE_COOL
    } else {
        MODE_HEAT
    };
    s[3] = target;
    // No sensor zones: leave MyZone alone.
    if let Some((_, zone)) = choice {
        s[4] = zone;
    }
    if s != p {
        plan.writes.push(write(v.unit, SystemStatus::REG, s));
    }
    // Every sensor zone's setpoint follows the active target.
    for z in zones(v).into_iter().filter(|z| z.has_sensor) {
        if z.p[3] != target {
            plan.writes
                .push(zone_write(v, z.n, z.open(), z.p[1], target));
        }
    }
}

/// Once a minute: count the on/off timers down and fire them at zero.
/// Timers pause (no countdown) while the bus is not ready.
pub fn tick_timers(st: &BusState, now: Instant) -> Plan {
    let app0 = &st.app;
    if app0.count_down_to_on == 0 && app0.count_down_to_off == 0 {
        return Plan::default();
    }
    let Ok(v) = view(st, now) else {
        return Plan::default();
    };
    let Some(p) = desired_status(&v) else {
        return Plan::default();
    };
    let mut app = app0.clone();
    let mut plan = Plan::default();
    let mut s = p;
    if app.count_down_to_on > 0 {
        app.count_down_to_on -= 1;
        if app.count_down_to_on == 0 {
            s[0] = 1;
            plan.log
                .push("timer: countDownToOn reached 0, turning on".into());
        }
    }
    if app.count_down_to_off > 0 {
        app.count_down_to_off -= 1;
        if app.count_down_to_off == 0 {
            s[0] = 0;
            plan.log
                .push("timer: countDownToOff reached 0, turning off".into());
        }
    }
    if s != p {
        plan.writes.push(write(v.unit, SystemStatus::REG, s));
    }
    plan.app = Some(app);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Bridge, TxMode};
    use crate::frame::encode;
    use crate::record::Record;

    /// Zone payload: number, open, damper %, sensor, setpoint °C, measured °C.
    fn z(n: u8, open: bool, pct: u8, sensor: bool, set: f32, measured: Option<f32>) -> String {
        let (mi, md) = measured.map_or((0, 0), |m| (m as u8, ((m * 10.0).round() as u8) % 10));
        format!(
            "0703abcde03{n:02x}{:02x}{:02x}{:02x}{mi:02x}{md:02x}00",
            u8::from(open) << 7 | pct,
            u8::from(sensor),
            wire(set)
        )
    }

    /// Status payload: on, mode, setpoint °C, MyZone.
    fn s(on: bool, mode: u8, set: f32, my_zone: u8) -> String {
        format!(
            "0703abcde05{:02x}{mode:02x}03{:02x}{my_zone:02x}0100",
            u8::from(on),
            wire(set)
        )
    }

    /// A ready bus state with the given status and zones.
    fn bus(status: &str, zones: &[String], app: AppState) -> BusState {
        let t = Instant::now();
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, t);
        let mut recs = vec![
            format!("0703abcde0120{:02x}0000000000", zones.len()),
            status.to_string(),
        ];
        for (i, zr) in zones.iter().enumerate() {
            recs.push(zr.clone());
            recs.push(format!("0703abcde04{:02x}006400000000", i + 1));
        }
        b.on_bytes(&encode(&format!("getCAN 1 {}", recs.join(" "))), t);
        let mut st = h.snapshot();
        st.app = app;
        st
    }

    fn status_write(p: &Plan) -> Option<[u8; 7]> {
        p.writes
            .iter()
            .find(|r| r.register == SystemStatus::REG)
            .map(|r| r.payload)
    }

    fn zone_writes(p: &Plan) -> Vec<&Record> {
        p.writes
            .iter()
            .filter(|r| r.register == ZoneState::REG)
            .collect()
    }

    const NOON: u16 = 12 * 60;

    #[test]
    fn timers_count_down_and_fire() {
        let zones = [z(1, true, 100, false, 24.0, None)];
        let app = AppState {
            count_down_to_off: 2,
            count_down_to_on: 0,
            ..AppState::default()
        };
        let st = bus(&s(true, MODE_COOL, 24.0, 0), &zones, app);
        let p = tick_timers(&st, Instant::now());
        assert!(p.writes.is_empty());
        let st = BusState {
            app: p.app.unwrap(),
            ..st
        };
        assert_eq!(st.app.count_down_to_off, 1);
        let p = tick_timers(&st, Instant::now());
        assert_eq!(status_write(&p).unwrap()[0], 0, "turned off");
        assert_eq!(p.app.as_ref().unwrap().count_down_to_off, 0);
        assert!(p.log[0].contains("countDownToOff"));

        let app = AppState {
            count_down_to_on: 1,
            ..AppState::default()
        };
        let st = bus(&s(false, MODE_COOL, 24.0, 0), &zones, app);
        let p = tick_timers(&st, Instant::now());
        assert_eq!(status_write(&p).unwrap()[0], 1, "turned on");

        // Nothing set: nothing happens.
        let st = bus(&s(false, MODE_COOL, 24.0, 0), &zones, AppState::default());
        assert_eq!(tick_timers(&st, Instant::now()), Plan::default());
    }

    #[test]
    fn timers_pause_while_the_bus_is_not_ready() {
        let (_b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        let mut st = h.snapshot();
        st.app.count_down_to_off = 3;
        assert_eq!(tick_timers(&st, Instant::now()), Plan::default());
    }

    fn cc() -> AppState {
        AppState {
            climate_control_enabled: true,
            ..AppState::default()
        }
    }

    #[test]
    fn climate_control_picks_the_zone_furthest_from_setpoint() {
        let zones = [
            z(1, true, 100, true, 22.0, Some(25.0)), // 3 °C too warm
            z(2, true, 100, true, 22.0, Some(23.0)), // 1 °C
            z(3, false, 0, true, 22.0, Some(30.0)),  // closed: ignored
            z(4, true, 100, false, 22.0, None),      // no sensor
        ];
        let st = bus(&s(true, MODE_COOL, 22.0, 2), &zones, cc());
        let p = evaluate(&st, Instant::now(), NOON);
        assert_eq!(status_write(&p).unwrap()[4], 1);
        assert!(p.log[0].contains("MyZone 2 -> 1"));
        // Already on the right zone: nothing to do.
        let st = bus(&s(true, MODE_COOL, 22.0, 1), &zones, cc());
        assert!(evaluate(&st, Instant::now(), NOON).writes.is_empty());
        // Heating looks the other way.
        let zones = [
            z(1, true, 100, true, 22.0, Some(21.0)),
            z(2, true, 100, true, 22.0, Some(18.5)),
        ];
        let st = bus(&s(true, MODE_HEAT, 22.0, 1), &zones, cc());
        assert_eq!(
            status_write(&evaluate(&st, Instant::now(), NOON)).unwrap()[4],
            2
        );
        // No zone needs it: MyZone unchanged.
        let zones = [z(1, true, 100, true, 22.0, Some(23.0))];
        let st = bus(&s(true, MODE_HEAT, 22.0, 1), &zones, cc());
        assert!(evaluate(&st, Instant::now(), NOON).writes.is_empty());
    }

    #[test]
    fn climate_control_only_runs_when_on_and_cooling_or_heating() {
        let zones = [
            z(1, true, 100, true, 22.0, Some(25.0)),
            z(2, true, 100, true, 22.0, Some(22.0)),
        ];
        for status in [s(false, MODE_COOL, 22.0, 2), s(true, MODE_VENT, 22.0, 2)] {
            let st = bus(&status, &zones, cc());
            assert!(evaluate(&st, Instant::now(), NOON).writes.is_empty());
            assert!(!running(&st, Instant::now()).climate_control);
        }
        let st = bus(&s(true, MODE_COOL, 22.0, 2), &zones, AppState::default());
        assert!(evaluate(&st, Instant::now(), NOON).writes.is_empty());
    }

    fn my_auto(cooling: bool) -> AppState {
        AppState {
            my_auto_enabled: true,
            my_auto_selected: true,
            my_auto_cooling: cooling,
            ..AppState::default() // cool 24, heat 20
        }
    }

    #[test]
    fn my_auto_switches_to_cool_for_the_hottest_zone() {
        let zones = [
            z(1, true, 100, true, 20.0, Some(24.5)), // ≥ 24.5: too hot
            z(2, true, 100, true, 20.0, Some(26.0)), // hottest
            z(3, true, 100, false, 20.0, None),
        ];
        let st = bus(&s(true, MODE_HEAT, 20.0, 1), &zones, my_auto(false));
        let p = evaluate(&st, Instant::now(), NOON);
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[1], sw[3], sw[4]), (MODE_COOL, wire(24.0), 2));
        assert!(p.app.as_ref().unwrap().my_auto_cooling);
        // Every sensor zone's setpoint follows the cool target; not zone 3.
        let zw = zone_writes(&p);
        assert_eq!(zw.len(), 2);
        assert!(zw.iter().all(|r| r.payload[3] == wire(24.0)));
        assert!(p.log.iter().any(|l| l.contains("switching to cool")));
    }

    #[test]
    fn my_auto_switches_to_heat_for_the_coldest_zone() {
        let zones = [
            z(1, true, 100, true, 24.0, Some(19.5)),
            z(2, true, 100, true, 24.0, Some(18.0)), // coldest
        ];
        let st = bus(&s(true, MODE_COOL, 24.0, 1), &zones, my_auto(true));
        let p = evaluate(&st, Instant::now(), NOON);
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[1], sw[3], sw[4]), (MODE_HEAT, wire(20.0), 2));
        assert!(!p.app.unwrap().my_auto_cooling);
    }

    #[test]
    fn my_auto_ties_stay_in_mode_and_in_range_keeps_it() {
        let zones = [
            z(1, true, 100, true, 24.0, Some(27.0)), // too hot
            z(2, true, 100, true, 24.0, Some(18.0)), // too cold
        ];
        let st = bus(&s(true, MODE_HEAT, 20.0, 2), &zones, my_auto(false));
        let p = evaluate(&st, Instant::now(), NOON);
        // Status already heat / 20 °C / coldest zone: no status write needed.
        assert_eq!(status_write(&p), None);
        assert!(p.app.is_none(), "still heating");
        assert!(zone_writes(&p).iter().all(|r| r.payload[3] == wire(20.0)));
        assert!(p.log.iter().any(|l| l.contains("unverified")));
        // Same tie while cooling stays cooling, MyZone the hottest.
        let st = bus(&s(true, MODE_COOL, 20.0, 2), &zones, my_auto(true));
        let sw = status_write(&evaluate(&st, Instant::now(), NOON)).unwrap();
        assert_eq!((sw[1], sw[3], sw[4]), (MODE_COOL, wire(24.0), 1));
        // All in range: keep the sub-mode and its target, MyZone untouched.
        let zones = [z(1, true, 100, true, 24.0, Some(22.0))];
        let st = bus(&s(true, MODE_COOL, 22.0, 1), &zones, my_auto(true));
        let p = evaluate(&st, Instant::now(), NOON);
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[1], sw[3], sw[4]), (MODE_COOL, wire(24.0), 1));
    }

    #[test]
    fn my_auto_without_sensors_leaves_my_zone_and_resets_bad_targets() {
        let zones = [z(1, true, 100, false, 22.0, None)];
        let app = AppState {
            my_auto_cool_target: 21.0, // gap < 4: invalid
            ..my_auto(true)
        };
        let st = bus(&s(true, MODE_COOL, 22.0, 0), &zones, app);
        let p = evaluate(&st, Instant::now(), NOON);
        let a = p.app.as_ref().unwrap();
        assert_eq!((a.my_auto_cool_target, a.my_auto_heat_target), (24.0, 20.0));
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[3], sw[4]), (wire(24.0), 0));
        assert!(zone_writes(&p).is_empty());
    }

    fn quiet() -> AppState {
        AppState {
            quiet_night_enabled: true,
            climate_control_enabled: true,
            ..AppState::default()
        }
    }

    fn quiet_zones() -> [String; 3] {
        [
            z(1, true, 100, true, 23.0, Some(26.0)),
            z(2, true, 40, false, 23.0, None), // open percent zone
            z(3, false, 0, false, 23.0, None), // closed percent zone
        ]
    }

    #[test]
    fn quiet_night_window_boundaries() {
        let status = s(true, MODE_COOL, 23.0, 0);
        let st = bus(&status, &quiet_zones(), quiet());
        // 21:59: not yet (climate control may still act).
        let p = evaluate(&st, Instant::now(), 21 * 60 + 59);
        assert!(!p.app.as_ref().is_some_and(|a| a.quiet_night_running));
        // 22:00: starts.
        let p = evaluate(&st, Instant::now(), QUIET_START);
        let app = p.app.clone().unwrap();
        assert!(app.quiet_night_running);
        let sw = status_write(&p).unwrap();
        assert_eq!(
            (sw[1], sw[3]),
            (MODE_VENT, QUIET_SET_TEMP),
            "fan only; MyZone 0 so 16 °C"
        );
        let zw = zone_writes(&p);
        assert_eq!(zw.len(), 2);
        assert_eq!(zw[0].payload[3], QUIET_SET_TEMP); // sensor zone 16 °C
        assert_eq!(zw[1].payload[1], 0x80 | 100); // open percent zone 100 %
        assert!(p.log[0].contains("quiet night: started"));

        // 05:59: still running; climate control paused, nothing written.
        let st = BusState {
            app: app.clone(),
            ..bus(&s(true, MODE_VENT, 16.0, 0), &quiet_zones(), quiet())
        };
        assert!(running(&st, Instant::now()).quiet_night);
        let p = evaluate(&st, Instant::now(), QUIET_END - 1);
        assert!(p.writes.is_empty() && p.app.is_none());

        // 06:00: restored (mode, setpoint), unit left on.
        let p = evaluate(&st, Instant::now(), QUIET_END);
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[0], sw[1], sw[3]), (1, MODE_COOL, wire(23.0)));
        assert!(!p.app.as_ref().unwrap().quiet_night_running);
        assert!(p.app.unwrap().quiet_night_saved.is_none());
    }

    #[test]
    fn quiet_night_restores_zones_and_respects_off() {
        let st = bus(&s(true, MODE_COOL, 23.0, 1), &quiet_zones(), quiet());
        let started = evaluate(&st, Instant::now(), 23 * 60);
        // MyZone set: system setpoint untouched.
        assert_eq!(status_write(&started).unwrap()[3], wire(23.0));
        let app = started.app.unwrap();
        // Overnight the zones changed and the user turned the unit off.
        let night = [
            z(1, true, 100, true, 16.0, Some(24.0)),
            z(2, true, 100, false, 23.0, None),
            z(3, false, 0, false, 23.0, None),
        ];
        let st = BusState {
            app,
            ..bus(&s(false, MODE_VENT, 23.0, 1), &night, quiet())
        };
        let p = evaluate(&st, Instant::now(), QUIET_END + 1);
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[0], sw[1]), (0, MODE_COOL), "stays off, mode restored");
        let zw = zone_writes(&p);
        assert_eq!(zw.len(), 2);
        assert_eq!(zw[0].payload[3], wire(23.0)); // zone 1 setpoint back
        assert_eq!(zw[1].payload[1], 0x80 | 40); // zone 2 back to 40 %
    }

    #[test]
    fn quiet_night_needs_cooling_and_ends_when_disabled() {
        let st = bus(&s(true, MODE_HEAT, 23.0, 0), &quiet_zones(), quiet());
        let p = evaluate(&st, Instant::now(), 23 * 60);
        assert!(!p.app.is_some_and(|a| a.quiet_night_running));
        let st = bus(&s(false, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        assert!(evaluate(&st, Instant::now(), 23 * 60).writes.is_empty());
        // Disabled while running: restored at once.
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let mut app = evaluate(&st, Instant::now(), 23 * 60).app.unwrap();
        app.quiet_night_enabled = false;
        let st = BusState {
            app,
            ..bus(&s(true, MODE_VENT, 16.0, 0), &quiet_zones(), quiet())
        };
        let p = evaluate(&st, Instant::now(), 23 * 60 + 5);
        assert_eq!(status_write(&p).unwrap()[1], MODE_COOL);
        assert!(!p.app.unwrap().quiet_night_running);
    }

    #[test]
    fn quiet_night_pauses_my_auto() {
        let app = AppState {
            quiet_night_enabled: true,
            ..my_auto(true)
        };
        let st = bus(&s(true, MODE_COOL, 24.0, 0), &quiet_zones(), app);
        let p = evaluate(&st, Instant::now(), 22 * 60 + 30);
        let app = p.app.unwrap();
        assert!(app.quiet_night_running);
        let st = BusState {
            app,
            ..bus(
                &s(true, MODE_VENT, 16.0, 0),
                &quiet_zones(),
                AppState::default(),
            )
        };
        let r = running(&st, Instant::now());
        assert!(r.quiet_night && !r.my_auto && !r.climate_control);
        assert!(evaluate(&st, Instant::now(), 23 * 60).writes.is_empty());
    }
}
