//! Minimal levelled logging to stderr.
//!
//! Under systemd (`JOURNAL_STREAM` set) each line carries an `<N>` priority
//! prefix so journald records the level and adds its own timestamp.
//! Otherwise lines start with a Unix timestamp and the level name.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl FromStr for Level {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            _ => {
                return Err(format!(
                    "unknown log level {s:?} (error, warn, info, debug)"
                ))
            }
        })
    }
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
    /// syslog priority.
    fn priority(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warn => 4,
            Level::Info => 6,
            Level::Debug => 7,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static JOURNAL: AtomicBool = AtomicBool::new(false);

/// Set the most verbose level that is printed.
pub fn init(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
    JOURNAL.store(
        std::env::var_os("JOURNAL_STREAM").is_some(),
        Ordering::Relaxed,
    );
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub fn write(level: Level, args: fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    // eprintln! (not a raw stderr handle) so test output is captured.
    if JOURNAL.load(Ordering::Relaxed) {
        eprintln!("<{}>{args}", level.priority());
    } else {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        eprintln!(
            "{}.{:03} {} {args}",
            t.as_secs(),
            t.subsec_millis(),
            level.name()
        );
    }
}

#[macro_export]
macro_rules! log_error { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_warn { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Warn, format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_info { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($t)*)) } }
#[macro_export]
macro_rules! log_debug { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Debug, format_args!($($t)*)) } }
