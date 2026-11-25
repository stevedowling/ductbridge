//! Tablet-side state that is not on the bus, persisted across restarts:
//! names, on/off timers, and the climate control / MyAuto / quiet night
//! features (flags, targets, quiet night's saved settings).
//!
//! The file is JSON, written atomically (temp file + rename). A missing or
//! corrupt file gives defaults and a warning, never a crash.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Longest accepted name, in characters.
pub const MAX_NAME: usize = 32;
/// Longest on/off timer, in minutes (12 h, as Home Assistant clamps it).
pub const MAX_TIMER_MINUTES: u32 = 720;

pub const DEFAULT_COOL_TARGET: f32 = 24.0;
pub const DEFAULT_HEAT_TARGET: f32 = 20.0;

/// Names set by clients. They override the names in the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Names {
    pub system: Option<String>,
    pub aircon: Option<String>,
    /// Zone number → name.
    pub zones: BTreeMap<u8, String>,
}

/// Settings quiet night replaced, restored when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// Register 05 payload when quiet night started.
    pub status: [u8; 7],
    /// Register 03 payload of each zone when quiet night started.
    pub zones: Vec<(u8, [u8; 7])>,
}

/// One register change a quiet-night transition (start or end) is waiting
/// to see reflected by the control box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub register: u8,
    /// Zone number for register 03, else 0.
    pub zone: u8,
    /// The payload written.
    pub target: [u8; 7],
    /// The payload it replaced. A dropped write is only reissued while the
    /// register still shows this, so a user's later change is never undone.
    pub from: [u8; 7],
}

/// A quiet-night start or end still being carried out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub steps: Vec<Step>,
    /// Evaluations so far; given up after a limit.
    pub attempts: u32,
    /// Ending quiet night (restoring), else starting it.
    #[serde(default)]
    pub restore: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppState {
    pub names: Names,
    /// Minutes until the unit is turned on; 0 = off.
    pub count_down_to_on: u32,
    /// Minutes until the unit is turned off; 0 = off.
    pub count_down_to_off: u32,
    /// Bumped on every client assignment of the on timer (even to the same
    /// value), so its minute restarts from that moment.
    pub count_down_to_on_gen: u32,
    /// Same for the off timer.
    pub count_down_to_off_gen: u32,
    pub climate_control_enabled: bool,
    pub my_auto_enabled: bool,
    /// The client selected mode "myauto".
    pub my_auto_selected: bool,
    /// MyAuto's current sub-mode: cooling (else heating).
    pub my_auto_cooling: bool,
    pub my_auto_cool_target: f32,
    pub my_auto_heat_target: f32,
    pub quiet_night_enabled: bool,
    pub quiet_night_running: bool,
    pub quiet_night_saved: Option<Saved>,
    /// Quiet night start/end writes not yet reflected by the control box.
    pub quiet_night_transition: Option<Transition>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            names: Names::default(),
            count_down_to_on: 0,
            count_down_to_off: 0,
            count_down_to_on_gen: 0,
            count_down_to_off_gen: 0,
            climate_control_enabled: false,
            my_auto_enabled: false,
            my_auto_selected: false,
            my_auto_cooling: true,
            my_auto_cool_target: DEFAULT_COOL_TARGET,
            my_auto_heat_target: DEFAULT_HEAT_TARGET,
            quiet_night_enabled: false,
            quiet_night_running: false,
            quiet_night_saved: None,
            quiet_night_transition: None,
        }
    }
}

/// Is a saved or target payload in range for its register?
fn payload_valid(register: u8, payload: [u8; 7]) -> bool {
    use crate::record::{Record, DEST_CB};
    use crate::registers::{SystemStatus, ZoneState};
    let r = Record {
        unit_type: crate::record::UNIT_AIRCON,
        dest: DEST_CB,
        uid: 0,
        register,
        payload,
    };
    match register {
        SystemStatus::REG => SystemStatus::decode(&r).is_ok(),
        ZoneState::REG => ZoneState::decode(&r).is_ok(),
        _ => false,
    }
}

/// MyAuto targets: cool 20–32 °C, heat 16–28 °C, cool at least 4 °C above heat.
pub fn targets_valid(cool: f32, heat: f32) -> bool {
    (20.0..=32.0).contains(&cool) && (16.0..=28.0).contains(&heat) && cool - heat >= 4.0
}

/// Strip control characters and surrounding spaces; 1–[`MAX_NAME`] chars.
pub fn sanitize_name(s: &str) -> Result<String, String> {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return Err("name must not be empty".into());
    }
    if clean.chars().count() > MAX_NAME {
        return Err(format!("name longer than {MAX_NAME} characters"));
    }
    Ok(clean.to_string())
}

impl AppState {
    /// Repair anything out of range (e.g. a hand-edited file): reset invalid
    /// MyAuto targets to cool 24 / heat 20, cap timers, drop bad names.
    /// Returns what was repaired.
    pub fn normalize(&mut self) -> Vec<String> {
        let mut fixed = Vec::new();
        if !targets_valid(self.my_auto_cool_target, self.my_auto_heat_target) {
            fixed.push(format!(
                "MyAuto targets cool {} / heat {} invalid; reset to {DEFAULT_COOL_TARGET} / {DEFAULT_HEAT_TARGET}",
                self.my_auto_cool_target, self.my_auto_heat_target
            ));
            self.my_auto_cool_target = DEFAULT_COOL_TARGET;
            self.my_auto_heat_target = DEFAULT_HEAT_TARGET;
        }
        for t in [&mut self.count_down_to_on, &mut self.count_down_to_off] {
            if *t > MAX_TIMER_MINUTES {
                fixed.push(format!("timer {t} capped to {MAX_TIMER_MINUTES} minutes"));
                *t = MAX_TIMER_MINUTES;
            }
        }
        let fix = |n: &mut Option<String>, fixed: &mut Vec<String>| {
            if let Some(s) = n.as_deref() {
                match sanitize_name(s) {
                    Ok(c) => *n = Some(c),
                    Err(e) => {
                        fixed.push(format!("dropped name {s:?}: {e}"));
                        *n = None;
                    }
                }
            }
        };
        fix(&mut self.names.system, &mut fixed);
        fix(&mut self.names.aircon, &mut fixed);
        let zones = std::mem::take(&mut self.names.zones);
        for (n, s) in zones {
            match sanitize_name(&s) {
                Ok(c) if (1..=crate::registers::MAX_ZONES).contains(&n) => {
                    self.names.zones.insert(n, c);
                }
                _ => fixed.push(format!("dropped zone {n} name {s:?}")),
            }
        }
        // Never restore or reissue out-of-range bytes onto the bus.
        if let Some(saved) = &self.quiet_night_saved {
            let ok = payload_valid(crate::registers::SystemStatus::REG, saved.status)
                && saved.zones.iter().all(|(n, p)| {
                    p[0] == *n && payload_valid(crate::registers::ZoneState::REG, *p)
                });
            if !ok {
                fixed.push("quiet night saved settings out of range; discarded".into());
                self.quiet_night_saved = None;
            }
        }
        if let Some(t) = &self.quiet_night_transition {
            // Payloads in range, and the step's zone consistent with them:
            // a zone step must name the zone both payloads are for.
            let ok = t.steps.iter().all(|s| {
                let zone_ok = match s.register {
                    crate::registers::SystemStatus::REG => s.zone == 0,
                    _ => s.target[0] == s.zone && s.from[0] == s.zone,
                };
                zone_ok && payload_valid(s.register, s.target) && payload_valid(s.register, s.from)
            });
            if !ok {
                fixed.push("quiet night transition out of range; discarded".into());
                self.quiet_night_transition = None;
            }
        }
        if self.quiet_night_running && self.quiet_night_saved.is_none() {
            fixed.push("quiet night marked running without saved settings; cleared".into());
            self.quiet_night_running = false;
        }
        fixed
    }

    /// Parse a state file's contents. Errors describe why it was unusable.
    pub fn from_json(text: &str) -> Result<(AppState, Vec<String>), String> {
        let mut s: AppState = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let fixed = s.normalize();
        Ok((s, fixed))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("state serializes")
    }
}

/// Load the state file. Missing → defaults; unreadable or corrupt →
/// defaults plus a warning. The second value lists warnings to log.
pub fn load(path: &Path) -> (AppState, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match AppState::from_json(&text) {
            Ok((s, fixed)) => (s, fixed),
            Err(e) => (
                AppState::default(),
                vec![format!(
                    "{}: corrupt state file ({e}); starting with defaults",
                    path.display()
                )],
            ),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (AppState::default(), Vec::new()),
        Err(e) => (
            AppState::default(),
            vec![format!(
                "{}: cannot read state file ({e}); starting with defaults",
                path.display()
            )],
        ),
    }
}

/// Write the state file atomically: a temp file in the same directory,
/// synced, then renamed over the old one.
pub fn save(path: &Path, state: &AppState) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(state.to_json().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ductbridge-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip() {
        let d = tmpdir("roundtrip");
        let p = d.join("state.json");
        let mut s = AppState {
            count_down_to_off: 30,
            climate_control_enabled: true,
            quiet_night_running: true,
            quiet_night_saved: Some(Saved {
                status: [1, 1, 3, 48, 2, 1, 0],
                zones: vec![(1, [1, 0xe4, 0, 48, 0, 0, 0])],
            }),
            ..Default::default()
        };
        s.names.system = Some("Home".into());
        s.names.zones.insert(2, "Bed 1".into());
        save(&p, &s).unwrap();
        let (back, warnings) = load(&p);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, s);
        assert!(!d.join("state.json.tmp").exists());
    }

    #[test]
    fn missing_file_is_defaults_without_warning() {
        let d = tmpdir("missing");
        let (s, w) = load(&d.join("nope.json"));
        assert_eq!(s, AppState::default());
        assert!(w.is_empty());
    }

    #[test]
    fn corrupt_file_is_defaults_with_warning() {
        let d = tmpdir("corrupt");
        let p = d.join("state.json");
        std::fs::write(&p, "{not json").unwrap();
        let (s, w) = load(&p);
        assert_eq!(s, AppState::default());
        assert!(w[0].contains("corrupt"), "{w:?}");
    }

    #[test]
    fn partial_and_out_of_range_files_are_repaired() {
        let (s, fixed) = AppState::from_json(
            r#"{"countDownToOn": 5000, "myAutoCoolTarget": 21, "myAutoHeatTarget": 20,
                "names": {"aircon": "\u0007  ", "zones": {"3": "Study", "11": "x"}},
                "quietNightRunning": true}"#,
        )
        .unwrap();
        assert_eq!(s.count_down_to_on, MAX_TIMER_MINUTES);
        assert_eq!((s.my_auto_cool_target, s.my_auto_heat_target), (24.0, 20.0));
        assert_eq!(s.names.aircon, None);
        assert_eq!(s.names.zones.get(&3).map(String::as_str), Some("Study"));
        assert!(!s.names.zones.contains_key(&11));
        assert!(!s.quiet_night_running);
        assert_eq!(fixed.len(), 5, "{fixed:?}");
    }

    #[test]
    fn invalid_quiet_night_payloads_are_discarded() {
        // Fan 255 / setpoint 127.5 °C must never reach the bus.
        let (s, fixed) = AppState::from_json(
            r#"{"quietNightRunning": true,
                "quietNightSaved": {"status": [1,1,255,255,0,1,0], "zones": []},
                "quietNightTransition": {"steps": [{"register": 3, "zone": 1,
                    "target": [1,228,0,200,0,0,0], "from": [1,228,0,48,0,0,0]}], "attempts": 0}}"#,
        )
        .unwrap();
        assert!(s.quiet_night_saved.is_none());
        assert!(s.quiet_night_transition.is_none());
        assert!(!s.quiet_night_running);
        assert_eq!(fixed.len(), 3, "{fixed:?}");
        // In range, but the step names zone 2 while the payloads are zone 1's.
        let (s, fixed) = AppState::from_json(
            r#"{"quietNightTransition": {"steps": [{"register": 3, "zone": 2,
                "target": [1,228,0,32,0,0,0], "from": [1,228,0,48,0,0,0]}], "attempts": 0}}"#,
        )
        .unwrap();
        assert!(s.quiet_night_transition.is_none(), "{fixed:?}");
        // A status step must use zone 0.
        let (s, _) = AppState::from_json(
            r#"{"quietNightTransition": {"steps": [{"register": 5, "zone": 1,
                "target": [1,3,3,32,0,1,0], "from": [1,1,3,48,0,1,0]}], "attempts": 0}}"#,
        )
        .unwrap();
        assert!(s.quiet_night_transition.is_none());
        // Valid ones are kept.
        let (s, fixed) = AppState::from_json(
            r#"{"quietNightRunning": true,
                "quietNightSaved": {"status": [1,1,3,48,0,1,0], "zones": [[2,[2,178,1,48,21,3,0]]]}}"#,
        )
        .unwrap();
        assert!(s.quiet_night_running && fixed.is_empty(), "{fixed:?}");
    }

    #[test]
    fn names_and_targets() {
        assert_eq!(
            sanitize_name("  Living\u{1b}[0m room ").unwrap(),
            "Living[0m room"
        );
        assert!(sanitize_name("\n\t").is_err());
        assert!(sanitize_name(&"x".repeat(33)).is_err());
        assert!(sanitize_name(&"é".repeat(32)).is_ok());
        assert!(targets_valid(24.0, 20.0));
        assert!(!targets_valid(23.0, 20.0)); // gap < 4
        assert!(!targets_valid(33.0, 20.0));
        assert!(!targets_valid(24.0, 15.0));
    }
}
