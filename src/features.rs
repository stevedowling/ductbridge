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
use crate::state::{
    targets_valid, AppState, Saved, Step, Transition, DEFAULT_COOL_TARGET, DEFAULT_HEAT_TARGET,
};

const MODE_COOL: u8 = 1;
const MODE_HEAT: u8 = 2;
const MODE_VENT: u8 = 3;
/// 16 °C on the wire (°C × 2).
const QUIET_SET_TEMP: u8 = 32;
/// Quiet night window, local time, minutes after midnight.
pub const QUIET_START: u16 = 22 * 60;
pub const QUIET_END: u16 = 6 * 60;
/// Evaluations (30 s apart) a quiet-night transition may take before it is
/// given up: about 10 minutes.
pub const TRANSITION_ATTEMPTS: u32 = 20;

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
/// `minute` is local time in minutes after midnight, or `None` when the
/// clock is not known to be right (not synchronised by NTP): quiet night
/// then does not start, and a running one waits for the clock (or for
/// being disabled) before ending.
pub fn evaluate(st: &BusState, now: Instant, minute: Option<u16>) -> Plan {
    let Ok(v) = view(st, now) else {
        return Plan::default();
    };
    let mut app = st.app.clone();
    let mut plan = Plan::default();
    advance_transition(&v, &mut app, &mut plan);
    // Stopping (disabled, or the window ended) supersedes an unfinished
    // start: drop its outstanding writes and restore instead.
    let stop = app.quiet_night_running
        && (!app.quiet_night_enabled || minute.map(in_quiet_window) == Some(false));
    let mut force = Vec::new();
    if stop
        && app
            .quiet_night_transition
            .as_ref()
            .is_some_and(|t| !t.restore)
    {
        // Its writes may still be queued or in flight: restore every key it
        // touched, even where the mirror already shows the saved value, so
        // the restore is ordered after them on the bus.
        let t = app.quiet_night_transition.take().expect("checked");
        force = t.steps.iter().map(|s| (s.register, s.zone)).collect();
        plan.writes.clear();
        plan.log
            .push("quiet night: stopping before its start completed".into());
    }
    if app.quiet_night_transition.is_none() {
        quiet_night(&v, &mut app, minute, &force, &mut plan);
    }
    // While quiet night is starting or ending, the other features wait:
    // they would build on the values being replaced.
    if app.quiet_night_transition.is_none() && plan.writes.is_empty() {
        let r = running_in(&v, &app);
        if r.my_auto {
            my_auto(&v, &mut app, &mut plan);
        } else if r.climate_control {
            climate_control(&v, &mut plan);
        }
    }
    if app != st.app {
        plan.app = Some(app);
    }
    plan
}

/// Does `read` show `target` for the bytes a write sets?
fn shows(register: u8, read: &[u8; 7], target: &[u8; 7]) -> bool {
    if register == SystemStatus::REG {
        read[1..6] == target[1..6]
    } else {
        read[1] == target[1] && read[3] == target[3]
    }
}

/// The write for a step, built on the current desired value (status keeps
/// its on/off and RF id; zones use the write format).
fn step_write(v: &View, step: &Step) -> crate::record::Record {
    if step.register == SystemStatus::REG {
        let mut s = desired_status(v).unwrap_or(step.target);
        s[1..6].copy_from_slice(&step.target[1..6]);
        write(v.unit, SystemStatus::REG, s)
    } else {
        let t = step.target;
        zone_write(v, step.zone, t[1] & 0x80 != 0, t[1], t[3])
    }
}

/// Follow a quiet-night start/end until the box reflects every write. A
/// dropped write (no longer pending, register unchanged) is reissued; a
/// register the user has changed since is left alone.
fn advance_transition(v: &View, app: &mut AppState, plan: &mut Plan) {
    let Some(mut t) = app.quiet_night_transition.take() else {
        return;
    };
    t.attempts += 1;
    let mut left = Vec::new();
    for step in t.steps {
        let key = (v.unit.0, v.unit.1, step.register, step.zone);
        let Some(read) = v.st.registers.get(&key).map(|r| r.payload) else {
            continue;
        };
        // A fresh pending write means the mirror may not show it yet, even
        // where it already matches (e.g. a restore to unchanged values).
        let pending = v.st.pending.get(&key).is_some_and(|p| {
            v.now.saturating_duration_since(p.accepted) < crate::bridge::PENDING_TTL
        });
        if pending {
            left.push(step); // still on its way
        } else if shows(step.register, &read, &step.target) {
            // done
        } else if shows(step.register, &read, &step.from) {
            plan.writes.push(step_write(v, &step)); // dropped: reissue
            left.push(step);
        }
        // else: changed by someone else since; leave it.
    }
    if left.is_empty() {
        return;
    }
    if t.attempts > TRANSITION_ATTEMPTS {
        plan.log.push(format!(
            "quiet night: {} change(s) never reflected by the control box; giving up",
            left.len()
        ));
        plan.writes.clear();
        return;
    }
    if !plan.writes.is_empty() {
        plan.log
            .push("quiet night: reissuing writes the control box dropped".into());
    }
    app.quiet_night_transition = Some(Transition {
        steps: left,
        attempts: t.attempts,
        restore: t.restore,
    });
}

/// Queue a write and record it as a step of the current transition.
fn transition_write(v: &View, steps: &mut Vec<Step>, plan: &mut Plan, step: Step) {
    plan.writes.push(step_write(v, &step));
    steps.push(step);
}

/// `force`: registers (register, zone) to restore even if they already show
/// the saved value (outstanding writes of a superseded start).
fn quiet_night(
    v: &View,
    app: &mut AppState,
    minute: Option<u16>,
    force: &[(u8, u8)],
    plan: &mut Plan,
) {
    let Some(p) = desired_status(v) else { return };
    // Unknown time: neither in nor out of the window.
    let in_window = minute.map(in_quiet_window);
    let mut steps = Vec::new();
    if app.quiet_night_running && (!app.quiet_night_enabled || in_window == Some(false)) {
        // Restore what quiet night replaced; on/off stays as it is now
        // (whether the original also turns the unit off is unverified).
        if let Some(saved) = app.quiet_night_saved.take() {
            let mut s = p;
            s[1..6].copy_from_slice(&saved.status[1..6]);
            if s != p || force.contains(&(SystemStatus::REG, 0)) {
                let step = Step {
                    register: SystemStatus::REG,
                    zone: 0,
                    target: s,
                    from: p,
                };
                transition_write(v, &mut steps, plan, step);
            }
            for z in zones(v) {
                if let Some((_, sp)) = saved.zones.iter().find(|(n, _)| *n == z.n) {
                    if sp[1] != z.p[1] || sp[3] != z.p[3] || force.contains(&(ZoneState::REG, z.n))
                    {
                        let target = [z.n, sp[1], 0, sp[3], 0, 0, 0];
                        let step = Step {
                            register: ZoneState::REG,
                            zone: z.n,
                            target,
                            from: z.p,
                        };
                        transition_write(v, &mut steps, plan, step);
                    }
                }
            }
        }
        app.quiet_night_running = false;
        plan.log.push(
            "quiet night: ended, previous settings restored (unit left on/off as is; unverified)"
                .into(),
        );
    } else if !app.quiet_night_running
        && app.quiet_night_enabled
        && in_window == Some(true)
        && p[0] == 1
        && p[1] == MODE_COOL
    {
        // Start while in the window and cooling (cool, or MyAuto cooling,
        // which is "cool" on the bus). Exact trigger unverified.
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
        let step = Step {
            register: SystemStatus::REG,
            zone: 0,
            target: s,
            from: p,
        };
        transition_write(v, &mut steps, plan, step);
        for z in zs {
            let target = if z.has_sensor && z.p[3] != QUIET_SET_TEMP {
                [z.n, z.p[1], 0, QUIET_SET_TEMP, 0, 0, 0]
            } else if !z.has_sensor && z.open() && z.p[1] & 0x7f != 100 {
                [z.n, 0x80 | 100, 0, z.p[3], 0, 0, 0]
            } else {
                continue;
            };
            let step = Step {
                register: ZoneState::REG,
                zone: z.n,
                target,
                from: z.p,
            };
            transition_write(v, &mut steps, plan, step);
        }
        plan.log
            .push("quiet night: started, fan only until 06:00 (trigger unverified)".into());
    }
    if !steps.is_empty() {
        app.quiet_night_transition = Some(Transition {
            steps,
            attempts: 0,
            restore: !app.quiet_night_running,
        });
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

/// Which timers have had a full minute since they were set or last counted:
/// the generation each was due for (see [`AppState::count_down_to_on_gen`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Due {
    pub on: Option<u32>,
    pub off: Option<u32>,
}

impl Due {
    pub fn any(self) -> bool {
        self.on.is_some() || self.off.is_some()
    }
}

/// Measures each timer's minutes from when it was set, not from daemon
/// start, so a 1-minute timer always runs a full minute. Every client
/// assignment (even to the same value) bumps the timer's generation, which
/// restarts its minute here.
#[derive(Debug, Clone, Default)]
pub struct TimerClock {
    elapsed: [u32; 2],
    gen: [Option<u32>; 2],
}

impl TimerClock {
    /// Advance by `secs` of wall time and report which timers are due.
    pub fn advance(&mut self, app: &AppState, secs: u32) -> Due {
        let timers = [
            (app.count_down_to_on, app.count_down_to_on_gen),
            (app.count_down_to_off, app.count_down_to_off_gen),
        ];
        let mut due = [None; 2];
        for (i, &(value, gen)) in timers.iter().enumerate() {
            if value == 0 {
                self.elapsed[i] = 0;
                self.gen[i] = None;
                continue;
            }
            if self.gen[i] != Some(gen) {
                // Newly (re-)armed during this interval: count from now, so
                // time before the assignment never counts.
                self.gen[i] = Some(gen);
                self.elapsed[i] = 0;
                continue;
            }
            // Capped: a minute that ends while the bus is not ready counts
            // once, as soon as it is ready again.
            self.elapsed[i] = (self.elapsed[i] + secs).min(60);
            if self.elapsed[i] >= 60 {
                due[i] = Some(gen);
            }
        }
        Due {
            on: due[0],
            off: due[1],
        }
    }

    /// Call after `tick_timers` for `due` was applied (or did nothing
    /// because the bus was not ready): restart those timers' minutes, unless
    /// a client re-armed them meanwhile (then `advance` restarts them).
    pub fn ticked(&mut self, due: Due) {
        for (i, d) in [due.on, due.off].into_iter().enumerate() {
            if d.is_some() && self.gen[i] == d {
                self.elapsed[i] = 0;
            }
        }
    }
}

/// Count down the timers that are `due` by one minute and fire them at
/// zero. Timers pause (no countdown) while the bus is not ready.
pub fn tick_timers(st: &BusState, now: Instant, due: Due) -> Plan {
    let app0 = &st.app;
    if !due.any() {
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
    // Checked under the submission lock: a timer re-armed since `due` was
    // computed (different generation) is not counted down now.
    if due.on == Some(app.count_down_to_on_gen) && app.count_down_to_on > 0 {
        app.count_down_to_on -= 1;
        if app.count_down_to_on == 0 {
            s[0] = 1;
            plan.log
                .push("timer: countDownToOn reached 0, turning on".into());
        }
    }
    if due.off == Some(app.count_down_to_off_gen) && app.count_down_to_off > 0 {
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
    if app != *app0 {
        plan.app = Some(app);
    }
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
    const BOTH: Due = Due {
        on: Some(0),
        off: Some(0),
    };

    /// A client assignment of the off timer (bumps its generation).
    fn set_off(app: &mut AppState, minutes: u32) {
        app.count_down_to_off = minutes;
        app.count_down_to_off_gen = app.count_down_to_off_gen.wrapping_add(1);
    }

    #[test]
    fn a_timer_set_late_in_a_minute_still_runs_a_full_minute() {
        let mut clock = TimerClock::default();
        let mut app = AppState::default();
        // 59 s pass with no timer; then a 1-minute timer is set.
        assert_eq!(clock.advance(&app, 59), Due::default());
        set_off(&mut app, 1);
        // The interval in which it was set does not count (it may have been
        // set at any point in it).
        assert_eq!(clock.advance(&app, 1).off, None, "seen being set");
        assert_eq!(clock.advance(&app, 59).off, None, "59 s counted");
        let due = clock.advance(&app, 1);
        assert_eq!(due.off, Some(1), "60 s counted: due");
        // After the tick (value now 0) nothing is due.
        app.count_down_to_off = 0;
        clock.ticked(due);
        assert_eq!(clock.advance(&app, 60), Due::default());
    }

    #[test]
    fn re_arming_to_the_same_value_restarts_the_minute() {
        let mut clock = TimerClock::default();
        let mut app = AppState::default();
        set_off(&mut app, 1);
        clock.advance(&app, 0);
        assert!(!clock.advance(&app, 50).any());
        // Set to 1 again after 50 s: the value is unchanged, the minute is not.
        set_off(&mut app, 1);
        clock.advance(&app, 0);
        assert!(
            !clock.advance(&app, 10).any(),
            "would fire 10 s after re-arming"
        );
        assert_eq!(clock.advance(&app, 50).off, Some(2));
    }

    #[test]
    fn timer_clock_counts_each_minute_and_long_gaps_once() {
        let mut clock = TimerClock::default();
        let mut app = AppState {
            count_down_to_on: 3,
            count_down_to_off: 2,
            ..AppState::default()
        };
        clock.advance(&app, 0);
        assert_eq!(clock.advance(&app, 60), BOTH);
        app.count_down_to_on = 2; // as tick_timers would (same generation)
        app.count_down_to_off = 1;
        clock.ticked(BOTH);
        assert!(!clock.advance(&app, 30).any());
        // A long gap (bus not ready): each timer is due once, not ten times.
        assert_eq!(clock.advance(&app, 600), BOTH);
        clock.ticked(BOTH);
        assert_eq!(clock.advance(&app, 59), Due::default());
    }

    #[test]
    fn a_timer_re_armed_after_due_was_computed_is_not_counted_down() {
        let zones = [z(1, true, 100, false, 24.0, None)];
        let mut clock = TimerClock::default();
        let mut app = AppState::default();
        set_off(&mut app, 2);
        clock.advance(&app, 0);
        let due = clock.advance(&app, 60);
        assert_eq!(due.off, Some(1));
        // Before the tick takes the lock, a client sets it to 1 (new gen).
        set_off(&mut app, 1);
        let st = bus(&s(true, MODE_COOL, 24.0, 0), &zones, app.clone());
        let p = tick_timers(&st, Instant::now(), due);
        assert!(p.writes.is_empty() && p.app.is_none(), "not fired early");
        clock.ticked(due);
        // The re-armed timer runs its own full minute.
        clock.advance(&app, 0);
        assert!(!clock.advance(&app, 59).any());
        assert_eq!(clock.advance(&app, 1).off, Some(2));
    }

    #[test]
    fn timers_count_down_and_fire() {
        let zones = [z(1, true, 100, false, 24.0, None)];
        let app = AppState {
            count_down_to_off: 2,
            count_down_to_on: 0,
            ..AppState::default()
        };
        let st = bus(&s(true, MODE_COOL, 24.0, 0), &zones, app);
        let p = tick_timers(&st, Instant::now(), BOTH);
        assert!(p.writes.is_empty());
        let st = BusState {
            app: p.app.unwrap(),
            ..st
        };
        assert_eq!(st.app.count_down_to_off, 1);
        let p = tick_timers(&st, Instant::now(), BOTH);
        assert_eq!(status_write(&p).unwrap()[0], 0, "turned off");
        assert_eq!(p.app.as_ref().unwrap().count_down_to_off, 0);
        assert!(p.log[0].contains("countDownToOff"));

        let app = AppState {
            count_down_to_on: 1,
            ..AppState::default()
        };
        let st = bus(&s(false, MODE_COOL, 24.0, 0), &zones, app);
        let p = tick_timers(&st, Instant::now(), BOTH);
        assert_eq!(status_write(&p).unwrap()[0], 1, "turned on");

        // Nothing set: nothing happens.
        let st = bus(&s(false, MODE_COOL, 24.0, 0), &zones, AppState::default());
        assert_eq!(tick_timers(&st, Instant::now(), BOTH), Plan::default());
    }

    #[test]
    fn timers_pause_while_the_bus_is_not_ready() {
        let (_b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        let mut st = h.snapshot();
        st.app.count_down_to_off = 3;
        assert_eq!(tick_timers(&st, Instant::now(), BOTH), Plan::default());
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
        let p = evaluate(&st, Instant::now(), Some(NOON));
        assert_eq!(status_write(&p).unwrap()[4], 1);
        assert!(p.log[0].contains("MyZone 2 -> 1"));
        // Already on the right zone: nothing to do.
        let st = bus(&s(true, MODE_COOL, 22.0, 1), &zones, cc());
        assert!(evaluate(&st, Instant::now(), Some(NOON)).writes.is_empty());
        // Heating looks the other way.
        let zones = [
            z(1, true, 100, true, 22.0, Some(21.0)),
            z(2, true, 100, true, 22.0, Some(18.5)),
        ];
        let st = bus(&s(true, MODE_HEAT, 22.0, 1), &zones, cc());
        assert_eq!(
            status_write(&evaluate(&st, Instant::now(), Some(NOON))).unwrap()[4],
            2
        );
        // No zone needs it: MyZone unchanged.
        let zones = [z(1, true, 100, true, 22.0, Some(23.0))];
        let st = bus(&s(true, MODE_HEAT, 22.0, 1), &zones, cc());
        assert!(evaluate(&st, Instant::now(), Some(NOON)).writes.is_empty());
    }

    #[test]
    fn climate_control_only_runs_when_on_and_cooling_or_heating() {
        let zones = [
            z(1, true, 100, true, 22.0, Some(25.0)),
            z(2, true, 100, true, 22.0, Some(22.0)),
        ];
        for status in [s(false, MODE_COOL, 22.0, 2), s(true, MODE_VENT, 22.0, 2)] {
            let st = bus(&status, &zones, cc());
            assert!(evaluate(&st, Instant::now(), Some(NOON)).writes.is_empty());
            assert!(!running(&st, Instant::now()).climate_control);
        }
        let st = bus(&s(true, MODE_COOL, 22.0, 2), &zones, AppState::default());
        assert!(evaluate(&st, Instant::now(), Some(NOON)).writes.is_empty());
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
        let p = evaluate(&st, Instant::now(), Some(NOON));
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
        let p = evaluate(&st, Instant::now(), Some(NOON));
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
        let p = evaluate(&st, Instant::now(), Some(NOON));
        // Status already heat / 20 °C / coldest zone: no status write needed.
        assert_eq!(status_write(&p), None);
        assert!(p.app.is_none(), "still heating");
        assert!(zone_writes(&p).iter().all(|r| r.payload[3] == wire(20.0)));
        assert!(p.log.iter().any(|l| l.contains("unverified")));
        // Same tie while cooling stays cooling, MyZone the hottest.
        let st = bus(&s(true, MODE_COOL, 20.0, 2), &zones, my_auto(true));
        let sw = status_write(&evaluate(&st, Instant::now(), Some(NOON))).unwrap();
        assert_eq!((sw[1], sw[3], sw[4]), (MODE_COOL, wire(24.0), 1));
        // All in range: keep the sub-mode and its target, MyZone untouched.
        let zones = [z(1, true, 100, true, 24.0, Some(22.0))];
        let st = bus(&s(true, MODE_COOL, 22.0, 1), &zones, my_auto(true));
        let p = evaluate(&st, Instant::now(), Some(NOON));
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
        let p = evaluate(&st, Instant::now(), Some(NOON));
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

    /// The zones once quiet night's writes are reflected.
    fn night_zones() -> [String; 3] {
        [
            z(1, true, 100, true, 16.0, Some(26.0)),
            z(2, true, 100, false, 23.0, None),
            z(3, false, 0, false, 23.0, None),
        ]
    }

    #[test]
    fn quiet_night_window_boundaries() {
        let status = s(true, MODE_COOL, 23.0, 0);
        let st = bus(&status, &quiet_zones(), quiet());
        // 21:59: not yet (climate control may still act).
        let p = evaluate(&st, Instant::now(), Some(21 * 60 + 59));
        assert!(!p.app.as_ref().is_some_and(|a| a.quiet_night_running));
        // 22:00: starts.
        let p = evaluate(&st, Instant::now(), Some(QUIET_START));
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
            ..bus(&s(true, MODE_VENT, 16.0, 0), &night_zones(), quiet())
        };
        assert!(running(&st, Instant::now()).quiet_night);
        let p = evaluate(&st, Instant::now(), Some(QUIET_END - 1));
        // The start is reflected: the transition completes, nothing written.
        assert!(p.writes.is_empty());
        let app = p.app.unwrap();
        assert!(app.quiet_night_running && app.quiet_night_transition.is_none());
        let st = BusState { app, ..st };

        // 06:00: restored (mode, setpoint), unit left on.
        let p = evaluate(&st, Instant::now(), Some(QUIET_END));
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[0], sw[1], sw[3]), (1, MODE_COOL, wire(23.0)));
        assert!(!p.app.as_ref().unwrap().quiet_night_running);
        assert!(p.app.unwrap().quiet_night_saved.is_none());
    }

    #[test]
    fn quiet_night_restores_zones_and_respects_off() {
        let st = bus(&s(true, MODE_COOL, 23.0, 1), &quiet_zones(), quiet());
        let started = evaluate(&st, Instant::now(), Some(23 * 60));
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
        let p = evaluate(&st, Instant::now(), Some(QUIET_END + 1));
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
        let p = evaluate(&st, Instant::now(), Some(23 * 60));
        assert!(!p.app.is_some_and(|a| a.quiet_night_running));
        let st = bus(&s(false, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        assert!(evaluate(&st, Instant::now(), Some(23 * 60))
            .writes
            .is_empty());
        // Disabled while running: restored at once.
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let mut app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        app.quiet_night_enabled = false;
        let st = BusState {
            app,
            ..bus(&s(true, MODE_VENT, 16.0, 0), &night_zones(), quiet())
        };
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 5));
        assert_eq!(status_write(&p).unwrap()[1], MODE_COOL);
        assert!(!p.app.unwrap().quiet_night_running);
    }

    #[test]
    fn quiet_night_waits_for_a_synchronised_clock() {
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        // Unknown time: does not start.
        let p = evaluate(&st, Instant::now(), None);
        assert!(!p.app.is_some_and(|a| a.quiet_night_running));
        // Running and the clock becomes unknown: keeps running...
        let app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        let st = BusState {
            app,
            ..bus(&s(true, MODE_VENT, 16.0, 0), &night_zones(), quiet())
        };
        let p = evaluate(&st, Instant::now(), None);
        assert!(p.writes.is_empty());
        let app = p.app.unwrap(); // only the completed start transition
        assert!(app.quiet_night_running && app.quiet_night_transition.is_none());
        let st = BusState { app, ..st };
        // ...but disabling still restores at once.
        let mut app = st.app.clone();
        app.quiet_night_enabled = false;
        let st = BusState { app, ..st };
        assert_eq!(
            status_write(&evaluate(&st, Instant::now(), None)).unwrap()[1],
            MODE_COOL
        );
    }

    #[test]
    fn dropped_quiet_night_writes_are_reissued() {
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        assert_eq!(app.quiet_night_transition.as_ref().unwrap().steps.len(), 3);
        // Nothing reflected, nothing pending (the session dropped them).
        let st = BusState { app, ..st };
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 1));
        assert_eq!(status_write(&p).unwrap()[1], MODE_VENT);
        assert_eq!(zone_writes(&p).len(), 2);
        assert!(p.log.iter().any(|l| l.contains("reissuing")));
        // Still on their way (pending): wait, write nothing.
        let mut st = BusState {
            app: p.app.clone().unwrap(),
            ..st
        };
        for r in &p.writes {
            st.pending.insert(
                crate::session::key_of(r),
                crate::bridge::Pending {
                    record: *r,
                    seq: 1,
                    accepted: Instant::now(),
                    confirmed: false,
                },
            );
        }
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 2));
        assert!(p.writes.is_empty());
        assert_eq!(p.app.unwrap().quiet_night_transition.unwrap().attempts, 2);
    }

    #[test]
    fn transition_leaves_user_changes_alone_and_gives_up() {
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        // The user switched to heat and changed the zones meanwhile: none of
        // quiet night's writes is reissued over that.
        let changed = [
            z(1, true, 100, true, 21.0, Some(26.0)),
            z(2, true, 70, false, 23.0, None),
            z(3, false, 0, false, 23.0, None),
        ];
        let st = BusState {
            app: app.clone(),
            ..bus(&s(true, MODE_HEAT, 23.0, 0), &changed, quiet())
        };
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 1));
        assert!(p.writes.is_empty());
        assert!(p.app.unwrap().quiet_night_transition.is_none());
        // Never reflected and never dropped (always pending): give up.
        let mut app = app;
        app.quiet_night_transition.as_mut().unwrap().attempts = TRANSITION_ATTEMPTS;
        let mut st = BusState {
            app,
            ..bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet())
        };
        let w = write((7, 0xabcde), SystemStatus::REG, [1, 3, 3, 32, 0, 1, 0]);
        st.pending.insert(
            crate::session::key_of(&w),
            crate::bridge::Pending {
                record: w,
                seq: 1,
                accepted: Instant::now(),
                confirmed: false,
            },
        );
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 2));
        assert!(p.log.iter().any(|l| l.contains("giving up")));
        assert!(p.app.unwrap().quiet_night_transition.is_none());
    }

    #[test]
    fn my_auto_waits_for_the_quiet_night_restore() {
        let app = AppState {
            quiet_night_enabled: true,
            ..my_auto(true)
        };
        let st = bus(&s(true, MODE_COOL, 24.0, 0), &quiet_zones(), app);
        let app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        // Overnight: quiet values reflected, fan changed by the user.
        let night = format!(
            "0703abcde05{:02x}{:02x}01{:02x}000100",
            1, MODE_VENT, QUIET_SET_TEMP
        );
        let st = BusState {
            app,
            ..bus(&night, &night_zones(), AppState::default())
        };
        let st = BusState {
            app: evaluate(&st, Instant::now(), Some(QUIET_START + 10))
                .app
                .unwrap_or(st.app.clone()),
            ..st
        };
        assert!(st.app.quiet_night_transition.is_none(), "start completed");
        // 06:00: only the restore is written, fan back to the saved high.
        let p = evaluate(&st, Instant::now(), Some(QUIET_END));
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[1], sw[2]), (MODE_COOL, 3));
        assert_eq!(
            p.writes
                .iter()
                .filter(|r| r.register == SystemStatus::REG)
                .count(),
            1
        );
        // Next round, restore still pending: MyAuto does not write yet.
        let mut st = BusState {
            app: p.app.unwrap(),
            ..st
        };
        for r in &p.writes {
            st.pending.insert(
                crate::session::key_of(r),
                crate::bridge::Pending {
                    record: *r,
                    seq: 1,
                    accepted: Instant::now(),
                    confirmed: false,
                },
            );
        }
        assert!(evaluate(&st, Instant::now(), Some(QUIET_END + 1))
            .writes
            .is_empty());
    }

    #[test]
    fn stopping_supersedes_an_unfinished_start() {
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        assert!(!app.quiet_night_transition.as_ref().unwrap().restore);
        // Only the status change got through; zones still unchanged, and
        // nothing pending (dropped). Then 06:00 arrives.
        let st = BusState {
            app,
            ..bus(&s(true, MODE_VENT, 16.0, 0), &quiet_zones(), quiet())
        };
        let p = evaluate(&st, Instant::now(), Some(QUIET_END));
        // No fan-only/16 °C reissue: the restore replaces the start.
        assert!(p
            .log
            .iter()
            .any(|l| l.contains("stopping before its start completed")));
        let sw = status_write(&p).unwrap();
        assert_eq!((sw[1], sw[3]), (MODE_COOL, wire(23.0)));
        assert!(zone_writes(&p)
            .iter()
            .all(|r| r.payload[3] != QUIET_SET_TEMP));
        let app = p.app.unwrap();
        assert!(!app.quiet_night_running);
        assert!(app.quiet_night_transition.unwrap().restore);
        // Disabling mid-start does the same.
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let mut app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        app.quiet_night_enabled = false;
        let st = BusState { app, ..st };
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 1));
        assert!(p
            .writes
            .iter()
            .all(|r| r.payload[1] != MODE_VENT || r.register != SystemStatus::REG));
        assert!(!p.app.unwrap().quiet_night_running);
    }

    #[test]
    fn time_before_a_re_arm_never_counts() {
        // The controller stalled for 60 s; the timer was re-armed near the
        // end of that gap. The new generation is not due straight away.
        let mut clock = TimerClock::default();
        let mut app = AppState::default();
        set_off(&mut app, 1);
        clock.advance(&app, 0);
        set_off(&mut app, 1);
        assert!(!clock.advance(&app, 60).any());
    }

    #[test]
    fn stopping_restores_every_key_a_superseded_start_touched() {
        // Start accepted, pending entries since expired, mirror unchanged
        // (writes still buffered, e.g. across a reconnect). Then 06:00.
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let app = evaluate(&st, Instant::now(), Some(23 * 60)).app.unwrap();
        let st = BusState { app, ..st };
        let p = evaluate(&st, Instant::now(), Some(QUIET_END));
        // The mirror already shows the saved values, yet every key is
        // rewritten so the restore lands after the buffered start writes.
        assert_eq!(status_write(&p).unwrap()[1], MODE_COOL);
        assert_eq!(zone_writes(&p).len(), 2);
        assert!(p.app.unwrap().quiet_night_transition.unwrap().restore);
    }

    #[test]
    fn a_fresh_pending_write_is_waited_for_even_if_the_mirror_matches() {
        let st = bus(&s(true, MODE_COOL, 23.0, 0), &quiet_zones(), quiet());
        let p = evaluate(&st, Instant::now(), Some(23 * 60));
        let mut st = BusState {
            app: p.app.unwrap(),
            ..st
        };
        // Pretend the restore-like target equals the mirror: use the start's
        // own writes but make the mirror show their targets... simpler: a
        // fresh pending entry for a key keeps its step even if it matches.
        let t = st.app.quiet_night_transition.clone().unwrap();
        let step = &t.steps[0];
        let w = write((7, 0xabcde), step.register, step.target);
        st.pending.insert(
            crate::session::key_of(&w),
            crate::bridge::Pending {
                record: w,
                seq: 1,
                accepted: Instant::now(),
                confirmed: false,
            },
        );
        // Mirror shows the target for the status register.
        let st = BusState {
            app: st.app.clone(),
            pending: st.pending.clone(),
            ..bus(&s(true, MODE_VENT, 16.0, 0), &night_zones(), quiet())
        };
        let p = evaluate(&st, Instant::now(), Some(23 * 60 + 1));
        let left = p
            .app
            .map(|a| a.quiet_night_transition)
            .unwrap_or(st.app.quiet_night_transition.clone());
        assert!(
            left.unwrap()
                .steps
                .iter()
                .any(|s| s.register == SystemStatus::REG),
            "still waiting"
        );
    }

    #[test]
    fn quiet_night_pauses_my_auto() {
        let app = AppState {
            quiet_night_enabled: true,
            ..my_auto(true)
        };
        let st = bus(&s(true, MODE_COOL, 24.0, 0), &quiet_zones(), app);
        let p = evaluate(&st, Instant::now(), Some(22 * 60 + 30));
        let app = p.app.unwrap();
        assert!(app.quiet_night_running);
        let st = BusState {
            app,
            ..bus(
                &s(true, MODE_VENT, 16.0, 0),
                &night_zones(),
                AppState::default(),
            )
        };
        let r = running(&st, Instant::now());
        assert!(r.quiet_night && !r.my_auto && !r.climate_control);
        assert!(evaluate(&st, Instant::now(), Some(23 * 60))
            .writes
            .is_empty());
    }
}
