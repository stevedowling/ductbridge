//! Daemon configuration: defaults, then the config file, then the command line.
//!
//! The file is plain `key = value` lines (a subset of TOML); `#` starts a
//! comment and values may be quoted:
//!
//! ```text
//! port = "/dev/ttyUSB0"
//! http_bind = "0.0.0.0:2025"  # or "off"
//! state_file = "/var/lib/ductbridge/state.json"  # or "off"
//! mode = "listen-only"     # or "active"
//! log_level = "info"       # error, warn, info, debug
//! system_name = "Home"     # names shown by apps (not stored on the bus)
//! system_id = "ductbridge" # stable id; Home Assistant's unique id
//! aircon_name = "Aircon"
//! sys_type = "MyAir5"     # system type reported to apps
//! my_app_rev = "15.1548"  # tablet app revision reported to apps
//! aa_service_rev = "14.118"
//! zone_names = "Living, Kitchen, Bed 1"   # zone 1 first, comma-separated
//! ```

use std::io;
use std::path::{Path, PathBuf};

use crate::api::ApiInfo;
use crate::bridge::TxMode;
use crate::log::Level;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/ductbridge.conf";

pub const USAGE: &str = "\
Usage: ductbridge [OPTIONS]

Drives the RS-485 bus in place of the wall tablet.

Options:
  -c, --config PATH      config file (default /etc/ductbridge.conf, if present)
  -p, --port DEVICE      serial device (default /dev/ttyUSB0)
      --http-bind ADDR   HTTP API address (default 0.0.0.0:2025; \"off\" disables)
      --state-file PATH  tablet-side state (default /var/lib/ductbridge/state.json)
      --listen-only      never transmit (default)
      --active           answer the control box's Pings (replaces the tablet)
      --simulate         use a simulated control box instead of the serial port
      --log-level LEVEL  error, warn, info or debug (default info)
  -h, --help             show this help
  -V, --version          show the version
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub port: String,
    pub http_bind: String,
    pub mode: TxMode,
    pub log_level: Level,
    /// Names and id reported by the HTTP API.
    pub api: ApiInfo,
    /// Persisted tablet-side state (names, timers, features); "off" keeps
    /// it in memory only.
    pub state_file: String,
    /// Talk to a simulated control box instead of the serial port (implies
    /// active mode; never touches the bus).
    pub simulate: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: "/dev/ttyUSB0".into(),
            http_bind: "0.0.0.0:2025".into(),
            mode: TxMode::ListenOnly,
            log_level: Level::Info,
            api: ApiInfo::default(),
            state_file: "/var/lib/ductbridge/state.json".into(),
            simulate: false,
        }
    }
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Run(Box<Config>),
    Help,
    Version,
}

fn parse_mode(s: &str) -> Result<TxMode, String> {
    match s {
        "listen-only" | "listen_only" => Ok(TxMode::ListenOnly),
        "active" => Ok(TxMode::Active),
        _ => Err(format!("unknown mode {s:?} (listen-only or active)")),
    }
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

/// Apply a config file's settings on top of `cfg`.
pub fn apply_file(cfg: &mut Config, text: &str) -> Result<(), String> {
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        // Strip comments outside quotes.
        let mut in_quote = None;
        let mut end = raw.len();
        for (i, c) in raw.char_indices() {
            match (c, in_quote) {
                ('"' | '\'', None) => in_quote = Some(c),
                (c, Some(q)) if c == q => in_quote = None,
                ('#', None) => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let line = raw[..end].trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected key = value"))?;
        set(cfg, k.trim(), unquote(v)).map_err(|e| format!("line {line_no}: {e}"))?;
    }
    Ok(())
}

/// Set one setting by key. Shared by the file parser and command-line
/// overrides, so both validate the same way.
fn set(cfg: &mut Config, key: &str, v: &str) -> Result<(), String> {
    match key {
        "port" => cfg.port = v.to_string(),
        "http_bind" => cfg.http_bind = v.to_string(),
        "state_file" => cfg.state_file = v.to_string(),
        "simulate" => {
            cfg.simulate = match v {
                "true" => true,
                "false" => false,
                _ => return Err(format!("simulate {v:?} (true or false)")),
            }
        }
        "system_name" => cfg.api.system_name = v.to_string(),
        "system_id" if v.is_empty() => return Err("system_id must not be empty".into()),
        "system_id" => cfg.api.system_id = v.to_string(),
        "aircon_name" => cfg.api.aircon_name = v.to_string(),
        "sys_type" if v.is_empty() => return Err("sys_type must not be empty".into()),
        "sys_type" => cfg.api.sys_type = v.to_string(),
        "my_app_rev" => {
            crate::api::check_my_app_rev(v)?;
            cfg.api.my_app_rev = v.to_string();
        }
        "aa_service_rev" if v.is_empty() => return Err("aa_service_rev must not be empty".into()),
        "aa_service_rev" => cfg.api.aa_service_rev = v.to_string(),
        "zone_names" => {
            let names: Vec<String> = v.split(',').map(|n| n.trim().to_string()).collect();
            if names.len() > crate::registers::MAX_ZONES as usize {
                return Err("zone_names has more than 10 names".into());
            }
            cfg.api.zone_names = names;
        }
        "mode" => cfg.mode = parse_mode(v)?,
        "log_level" => cfg.log_level = v.parse()?,
        other => return Err(format!("unknown key {other:?}")),
    }
    Ok(())
}

/// Build the configuration from command-line arguments (without the program
/// name). `read` loads a config file; it is a parameter so tests need no files.
pub fn from_args<I, F>(args: I, read: F) -> Result<Action, String>
where
    I: IntoIterator<Item = String>,
    F: Fn(&Path) -> io::Result<String>,
{
    let mut config_path: Option<PathBuf> = None;
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" | "-V" | "--version" | "--listen-only" | "--active" | "--simulate"
                if inline.is_some() =>
            {
                return Err(format!("{flag} takes no value"))
            }
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "-c" | "--config" => config_path = Some(value(&flag)?.into()),
            "-p" | "--port" => overrides.push(("port".into(), value(&flag)?)),
            "--http-bind" => overrides.push(("http_bind".into(), value(&flag)?)),
            "--state-file" => overrides.push(("state_file".into(), value(&flag)?)),
            "--log-level" => overrides.push(("log_level".into(), value(&flag)?)),
            "--listen-only" => overrides.push(("mode".into(), "listen-only".into())),
            "--active" => overrides.push(("mode".into(), "active".into())),
            "--simulate" => overrides.push(("simulate".into(), "true".into())),
            _ => return Err(format!("unknown argument {a:?}\n\n{USAGE}")),
        }
    }

    let mut cfg = Config::default();
    match &config_path {
        Some(p) => {
            let text = read(p).map_err(|e| format!("{}: {e}", p.display()))?;
            apply_file(&mut cfg, &text).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        None => match read(Path::new(DEFAULT_CONFIG_PATH)) {
            Ok(text) => {
                apply_file(&mut cfg, &text).map_err(|e| format!("{DEFAULT_CONFIG_PATH}: {e}"))?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{DEFAULT_CONFIG_PATH}: {e}")),
        },
    }
    for (k, v) in overrides {
        // Set directly (not by re-serialising a config line, where quotes
        // or `#` in the value would be misparsed).
        set(&mut cfg, &k, &v).map_err(|e| format!("--{}: {e}", k.replace('_', "-")))?;
    }
    Ok(Action::Run(Box::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }
    fn no_file(_: &Path) -> io::Result<String> {
        Err(io::ErrorKind::NotFound.into())
    }
    fn run(a: &str, read: impl Fn(&Path) -> io::Result<String>) -> Result<Config, String> {
        match from_args(args(a), read)? {
            Action::Run(c) => Ok(*c),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults_to_listen_only() {
        let c = run("", no_file).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.mode, TxMode::ListenOnly);
        assert_eq!(c.port, "/dev/ttyUSB0");
        assert_eq!(c.state_file, "/var/lib/ductbridge/state.json");
        let c = run("--state-file /tmp/s.json", no_file).unwrap();
        assert_eq!(c.state_file, "/tmp/s.json");
        assert!(!c.simulate);
        assert!(run("--simulate", no_file).unwrap().simulate);
        assert!(run("--simulate=yes", no_file).is_err());
    }

    #[test]
    fn file_then_cli_override() {
        let file = |p: &Path| {
            assert_eq!(p, Path::new("/x.conf"));
            Ok(
                "# comment\nport = \"/dev/serial/by-id/usb-x#1\"  # trailing\n\
                mode = active\nlog_level='debug'\n\nhttp_bind = 127.0.0.1:2025\n\
                system_name = Home\nzone_names = \"Living, Bed 1,,Study\"\n"
                    .to_string(),
            )
        };
        let c = run("-c /x.conf", file).unwrap();
        assert_eq!(c.port, "/dev/serial/by-id/usb-x#1");
        assert_eq!(c.mode, TxMode::Active);
        assert_eq!(c.log_level, Level::Debug);
        assert_eq!(c.http_bind, "127.0.0.1:2025");
        assert_eq!(c.api.system_name, "Home");
        assert_eq!(c.api.zone_names, ["Living", "Bed 1", "", "Study"]);
        assert_eq!(c.api.system_id, "ductbridge");
        assert_eq!(c.api.sys_type, "MyAir5");
        let f = |_: &Path| Ok("sys_type = \"e-zone\"".to_string());
        assert_eq!(run("", f).unwrap().api.sys_type, "e-zone");

        let c = run("--config=/x.conf --listen-only --port /dev/ttyUSB1", file).unwrap();
        assert_eq!(c.mode, TxMode::ListenOnly);
        assert_eq!(c.port, "/dev/ttyUSB1");
        // CLI values are taken literally, quotes and '#' included.
        let c = run("--port a\"#b", no_file).unwrap();
        assert_eq!(c.port, "a\"#b");
    }

    #[test]
    fn default_file_is_used_when_present() {
        let c = run("", |_: &Path| Ok("mode = \"active\"".into())).unwrap();
        assert_eq!(c.mode, TxMode::Active);
    }

    #[test]
    fn errors() {
        assert!(run("-c /missing", no_file)
            .unwrap_err()
            .contains("/missing"));
        assert!(run("--bogus", no_file).is_err());
        assert!(run("--port", no_file)
            .unwrap_err()
            .contains("needs a value"));
        assert!(run("--log-level loud", no_file).is_err());
        assert!(run("--active=false", no_file)
            .unwrap_err()
            .contains("takes no value"));
        assert!(run("--listen-only=1", no_file).is_err());
        let bad = |_: &Path| Ok("mode = transmit".to_string());
        assert!(run("", bad).unwrap_err().contains("line 1"));
        let bad = |_: &Path| Ok("zone_names = a,b,c,d,e,f,g,h,i,j,k".to_string());
        assert!(run("", bad).unwrap_err().contains("more than 10"));
        let bad = |_: &Path| Ok("system_id = \"\"".to_string());
        assert!(run("", bad).is_err());
        let bad = |_: &Path| Ok("sys_type = \"\"".to_string());
        assert!(run("", bad).is_err());
        let bad = |_: &Path| Ok("my_app_rev = 12.5".to_string());
        assert!(run("", bad).unwrap_err().contains("my_app_rev"));
        let bad = |_: &Path| Ok("aa_service_rev = \"\"".to_string());
        assert!(run("", bad).is_err());
        let f = |_: &Path| Ok("my_app_rev = 15.1600\naa_service_rev = 14.120".to_string());
        let c = run("", f).unwrap();
        assert_eq!(
            (c.api.my_app_rev.as_str(), c.api.aa_service_rev.as_str()),
            ("15.1600", "14.120")
        );
        let bad = |_: &Path| Ok("colour = blue".to_string());
        assert!(run("", bad).unwrap_err().contains("unknown key"));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(from_args(args("--help"), no_file), Ok(Action::Help));
        assert_eq!(from_args(args("-V"), no_file), Ok(Action::Version));
    }
}
