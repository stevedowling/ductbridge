//! Replacement for the Advantage Air wall tablet.
//!
//! Layers, bottom up:
//! - [`crc`]: frame checksum
//! - [`frame`]: `<U>…</U=xx>` encode/decode
//! - [`record`]: 25-character CAN2 register records
//! - [`registers`]: typed register fields
//! - [`session`]: the tablet's side of the bus conversation
//! - [`bridge`]: session + link + shared state for the API layer
//! - [`api`]: the tablet's port-2025 JSON API (pure); [`http`]: its server
//! - [`config`], [`log`]: daemon plumbing

pub mod api;
pub mod bridge;
pub mod config;
pub mod crc;
pub mod features;
pub mod frame;
pub mod http;
pub mod log;
pub mod record;
pub mod registers;
pub mod session;
pub mod sim;
pub mod state;

/// Human-readable one-line description of a record, for logs and the monitor.
pub fn describe(r: &record::Record) -> String {
    use registers::*;
    let head = format!(
        "{r}  unit {:02x}:{:05x} reg {:02x}",
        r.unit_type, r.uid, r.register
    );
    let body = match r.register {
        0x05 => SystemStatus::decode(r).map(|s| format!("{s:?}")),
        0x03 => ZoneState::decode(r).map(|z| format!("{z:?}")),
        0x01 => ZoneInfo::decode(r).map(|z| format!("{z:?}")),
        0x04 => ZoneConfig::decode(r).map(|z| format!("{z:?}")),
        0x06 if r.dest != record::DEST_CB => Firmware::decode(r).map(|f| format!("{f:?}")),
        0x06 => Ok("flush request".into()),
        0x07 => Ok("firmware ack".into()),
        0x08 => decode_error(r).map(|e| format!("aircon error {e:?}")),
        0x0a => Ok("unit announcement".into()),
        _ => Ok(String::new()),
    };
    match body {
        Ok(b) if b.is_empty() => head,
        Ok(b) => format!("{head}  {b}"),
        Err(e) => format!("{head}  INVALID {e:?}"),
    }
}
