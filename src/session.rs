//! The tablet side of the control-box session, as a pure state machine.
//!
//! Feed every decoded frame to [`Session::on_frame`]. When the frame is a
//! `Ping`, it returns the single payload to transmit in the reply slot (or
//! `None` to stay silent). No I/O happens here, so the logic is fully testable.
//!
//! Behaviour follows the aa_interop spec §03 and observed bus traffic:
//! - reply priority: `ackCAN` > negotiation > resend > flush > writes > poll;
//! - `ackCAN 1` for a good checksum, `ackCAN 0` for a bad one;
//! - a `setCAN` with records stays in flight until a `getCAN 1` arrives, and
//!   is resent on later pings, up to [`MAX_RETRIES`];
//! - at most [`MAX_RECORDS`] records per `setCAN`;
//! - every firmware announcement (register 06) is answered with register 07.

use std::collections::{HashMap, VecDeque};

use crate::frame::Frame;
use crate::record::{parse_list, Record, UNIT_AIRCON};
use crate::registers::{firmware_ack, flush};

/// The original tablet never puts more than 25 records in one `setCAN`.
pub const MAX_RECORDS: usize = 25;
/// Resends of an unacknowledged `setCAN` before giving up on it.
pub const MAX_RETRIES: u8 = 3;
/// Pings to wait for a unit announcement after the global flush.
const UNIT_WAIT_PINGS: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Sending `getSystemData` until the box answers at all.
    Negotiating,
    /// Requesting the full register dump.
    Flushing,
    /// Normal operation.
    Steady,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PhaseChanged(Phase),
    /// A register record received from the box.
    Register(Record),
    /// The box never confirmed these writes.
    WriteDropped(Vec<Record>),
    /// The box accepted (`getCAN 1`) these writes.
    WriteConfirmed(Vec<Record>),
    /// A frame with a bad checksum was received and ignored.
    BadChecksum(String),
    /// A legacy XML reply (`<request>…`), passed through untouched.
    LegacyXml(String),
}

/// Key used to merge queued writes and the register mirror.
pub type RegKey = (u8, u32, u8, u8);

pub fn key_of(r: &Record) -> RegKey {
    let zone = if matches!(r.register, 0x03 | 0x04) {
        r.payload[0]
    } else {
        0
    };
    (r.unit_type, r.uid, r.register, zone)
}

struct InFlight {
    payload: String,
    records: Vec<Record>,
    retries: u8,
}

pub struct Session {
    phase: Phase,
    /// `Some(crc_ok)` when a `getCAN` must be acknowledged on the next ping.
    pending_ack: Option<bool>,
    in_flight: Option<InFlight>,
    queue: VecDeque<Record>,
    can2_busy: bool,
    global_flush_sent: bool,
    unit_flush_sent: bool,
    pings_since_flush: u32,
    /// Primary aircon unit: (unit type, uid).
    unit: Option<(u8, u32)>,
    mirror: HashMap<RegKey, Record>,
    events: Vec<Event>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Session {
            phase: Phase::Negotiating,
            pending_ack: None,
            in_flight: None,
            queue: VecDeque::new(),
            can2_busy: false,
            global_flush_sent: false,
            unit_flush_sent: false,
            pings_since_flush: 0,
            unit: None,
            mirror: HashMap::new(),
            events: Vec::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The primary aircon unit, once the box has announced it.
    pub fn unit(&self) -> Option<(u8, u32)> {
        self.unit
    }

    /// Latest known value of every register.
    pub fn mirror(&self) -> &HashMap<RegKey, Record> {
        &self.mirror
    }

    /// Queue a register write. A queued write to the same register (and
    /// zone) is replaced, so only the latest value is sent.
    pub fn queue_write(&mut self, r: Record) {
        let k = key_of(&r);
        self.queue.retain(|q| key_of(q) != k);
        self.queue.push_back(r);
    }

    /// Is a write to this register (and zone) queued or in flight?
    pub fn has_write_for(&self, key: &RegKey) -> bool {
        let in_flight = self
            .in_flight
            .as_ref()
            .is_some_and(|f| f.records.iter().any(|r| key_of(r) == *key));
        in_flight || self.queue.iter().any(|r| key_of(r) == *key)
    }

    /// Remove and return every write not yet confirmed by the box: the
    /// in-flight `setCAN` first, then the queue. Used when the session is
    /// replaced, so the caller can carry them over.
    pub fn take_pending_writes(&mut self) -> Vec<Record> {
        let mut out: Vec<Record> = self.in_flight.take().map(|f| f.records).unwrap_or_default();
        out.extend(self.queue.drain(..));
        out
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn set_phase(&mut self, p: Phase) {
        if self.phase != p {
            self.phase = p;
            self.events.push(Event::PhaseChanged(p));
        }
    }

    /// Handle one frame. Returns the payload to send if this was a `Ping`.
    pub fn on_frame(&mut self, f: &Frame) -> Option<String> {
        let p = f.payload.as_str();
        if p == "Ping" {
            return self.on_ping();
        }
        if let Some(rest) = p.strip_prefix("getCAN ") {
            self.on_get_can(rest, f.crc_ok);
        } else if !f.crc_ok {
            self.events.push(Event::BadChecksum(f.payload.clone()));
        } else if p.starts_with("CAN2 in use") {
            self.can2_busy = true;
            self.answered();
        } else if p.starts_with("<request>") {
            self.events.push(Event::LegacyXml(f.payload.clone()));
            self.answered();
        }
        // Echoes of our own frames and anything unknown are ignored.
        None
    }

    /// The box has answered a negotiation request.
    fn answered(&mut self) {
        if self.phase == Phase::Negotiating {
            self.set_phase(Phase::Flushing);
        }
    }

    fn on_get_can(&mut self, rest: &str, crc_ok: bool) {
        self.pending_ack = Some(crc_ok);
        self.can2_busy = false;
        if !crc_ok {
            self.events
                .push(Event::BadChecksum(format!("getCAN {rest}")));
            return;
        }
        self.answered();
        let (status, body) = rest.split_once(' ').unwrap_or((rest, ""));
        if status != "1" {
            // Nack: leave the in-flight write to be resent.
            return;
        }
        // The box accepted whatever we last sent.
        if let Some(done) = self.in_flight.take() {
            self.events
                .push(Event::WriteConfirmed(done.records.clone()));
            if self.phase == Phase::Flushing
                && self.unit_flush_sent
                && done.records.iter().any(|r| r.register == 0x06)
            {
                self.set_phase(Phase::Steady);
            }
        }
        for r in parse_list(body) {
            self.apply(r);
        }
    }

    fn apply(&mut self, r: Record) {
        // Only records addressed to the tablet (03, or 04 as also observed).
        if !matches!(r.dest, 0x03 | 0x04) {
            return;
        }
        if r.is_aircon() && matches!(r.register, 0x06 | 0x0a) && self.unit.is_none() && r.uid != 0 {
            self.unit = Some((r.unit_type, r.uid));
        }
        if r.register == 0x06 {
            self.queue_write(firmware_ack(&r));
            if self.phase == Phase::Flushing && !self.unit_flush_sent && r.is_aircon() {
                self.queue_write(flush(r.unit_type, r.uid));
                self.unit_flush_sent = true;
            }
        }
        self.mirror.insert(key_of(&r), r);
        self.events.push(Event::Register(r));
    }

    fn send_records(&mut self, records: Vec<Record>) -> String {
        let payload = format!(
            "setCAN {}",
            records
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.in_flight = Some(InFlight {
            payload: payload.clone(),
            records,
            retries: 0,
        });
        payload
    }

    fn on_ping(&mut self) -> Option<String> {
        // 1. Acknowledge a getCAN first.
        if let Some(ok) = self.pending_ack.take() {
            return Some(if ok { "ackCAN 1" } else { "ackCAN 0" }.to_string());
        }
        // 2. Negotiate.
        if self.phase == Phase::Negotiating {
            return Some("getSystemData".to_string());
        }
        // 3. Resend an unconfirmed write.
        if let Some(inf) = self.in_flight.as_mut() {
            if inf.retries < MAX_RETRIES {
                inf.retries += 1;
                return Some(inf.payload.clone());
            }
            let dropped = self.in_flight.take().unwrap();
            self.events.push(Event::WriteDropped(dropped.records));
        }
        // 4. Start the register dump.
        if self.phase == Phase::Flushing {
            if !self.global_flush_sent {
                self.global_flush_sent = true;
                return Some(self.send_records(vec![flush(UNIT_AIRCON, 0)]));
            }
            self.pings_since_flush += 1;
            if !self.unit_flush_sent && self.pings_since_flush > UNIT_WAIT_PINGS {
                // No unit announced itself; carry on with what we have.
                self.set_phase(Phase::Steady);
            }
        }
        // 5. Queued writes, at most 25 per frame.
        if !self.queue.is_empty() {
            let n = self.queue.len().min(MAX_RECORDS);
            let batch: Vec<Record> = self.queue.drain(..n).collect();
            return Some(self.send_records(batch));
        }
        // 6. Empty poll, unless the box said it is busy.
        if self.can2_busy {
            return None;
        }
        Some("setCAN ".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registers::{Fan, Mode, SystemStatus};

    fn frame(p: &str) -> Frame {
        Frame {
            payload: p.to_string(),
            crc_ok: true,
        }
    }
    fn ping(s: &mut Session) -> Option<String> {
        s.on_frame(&frame("Ping"))
    }

    /// Drive a session through negotiation and the dump for unit abcde.
    fn synced() -> Session {
        let mut s = Session::new();
        assert_eq!(ping(&mut s).as_deref(), Some("getSystemData"));
        s.on_frame(&frame("CAN2 in use"));
        assert_eq!(s.phase(), Phase::Flushing);
        assert_eq!(
            ping(&mut s).as_deref(),
            Some("setCAN 0701000000600000000000000")
        );
        s.on_frame(&frame("getCAN 1 0703abcde0602030100000000"));
        assert_eq!(ping(&mut s).as_deref(), Some("ackCAN 1"));
        // Firmware ack and unit-scoped flush go out together.
        assert_eq!(
            ping(&mut s).as_deref(),
            Some("setCAN 0701abcde0700000000000000 0701abcde0600000000000000")
        );
        s.on_frame(&frame(
            "getCAN 1 0703abcde0501010330000100 0703abcde0301e40030000000",
        ));
        assert_eq!(s.phase(), Phase::Steady);
        assert_eq!(ping(&mut s).as_deref(), Some("ackCAN 1"));
        s
    }

    #[test]
    fn full_startup_sequence() {
        let mut s = synced();
        assert_eq!(s.unit(), Some((0x07, 0xabcde)));
        assert_eq!(s.mirror().len(), 3);
        // Steady state: empty poll.
        assert_eq!(ping(&mut s).as_deref(), Some("setCAN "));
    }

    #[test]
    fn keeps_negotiating_until_the_box_answers() {
        let mut s = Session::new();
        for _ in 0..5 {
            assert_eq!(ping(&mut s).as_deref(), Some("getSystemData"));
        }
    }

    #[test]
    fn bad_checksum_is_nacked_and_not_applied() {
        let mut s = synced();
        let before = s.mirror().len();
        s.on_frame(&Frame {
            payload: "getCAN 1 0703abcde0302640030000000".into(),
            crc_ok: false,
        });
        assert_eq!(ping(&mut s).as_deref(), Some("ackCAN 0"));
        assert_eq!(s.mirror().len(), before);
    }

    #[test]
    fn write_is_resent_until_confirmed_then_dropped() {
        let mut s = synced();
        let st = SystemStatus {
            on: false,
            mode: Mode::Cool,
            fan: Fan::Low,
            set_temp: 24.0,
            my_zone: 0,
            fresh_air: 0,
            rf_sys_id: 0,
        };
        s.queue_write(st.encode_write(0x07, 0xabcde).unwrap());
        let sent = ping(&mut s).unwrap();
        assert!(sent.starts_with("setCAN 0701abcde05"));
        // Box nacks: we ack the nack, then resend.
        s.on_frame(&frame("getCAN 0"));
        assert_eq!(ping(&mut s).as_deref(), Some("ackCAN 1"));
        for _ in 0..MAX_RETRIES {
            assert_eq!(ping(&mut s), Some(sent.clone()));
        }
        // Gives up and reports it.
        assert_eq!(ping(&mut s).as_deref(), Some("setCAN "));
        assert!(s
            .drain_events()
            .iter()
            .any(|e| matches!(e, Event::WriteDropped(_))));
    }

    #[test]
    fn confirmed_write_is_not_resent() {
        let mut s = synced();
        s.queue_write(crate::registers::flush(0x07, 0xabcde));
        ping(&mut s).unwrap();
        s.on_frame(&frame("getCAN 1 "));
        assert_eq!(ping(&mut s).as_deref(), Some("ackCAN 1"));
        assert_eq!(ping(&mut s).as_deref(), Some("setCAN "));
    }

    #[test]
    fn caps_records_per_frame_and_merges_duplicates() {
        let mut s = synced();
        for zone in 1..=10u8 {
            for pct in [10u8, 50] {
                let z = crate::registers::ZoneState {
                    zone,
                    open: true,
                    percent: pct,
                    sensor_type: 0,
                    set_temp: 24.0,
                    measured_temp: None,
                };
                s.queue_write(z.encode_write(0x07, 0xabcde).unwrap());
            }
        }
        // 10 zones x 2 values merge to 10 records, all with the latest value.
        let sent = ping(&mut s).unwrap();
        assert_eq!(sent.split(' ').count() - 1, 10);
        assert!(sent.split(' ').skip(1).all(|r| &r[13..15] == "b2")); // 0x80 | 50

        let mut s = synced();
        for i in 0..30u32 {
            s.queue_write(crate::registers::flush(0x07, i + 1));
        }
        let sent = ping(&mut s).unwrap();
        assert_eq!(sent.split(' ').count() - 1, MAX_RECORDS);
    }

    #[test]
    fn take_pending_writes_returns_in_flight_then_queue() {
        let mut s = synced();
        s.queue_write(crate::registers::flush(0x07, 1));
        ping(&mut s).unwrap(); // in flight
        s.queue_write(crate::registers::flush(0x07, 2));
        let uids: Vec<u32> = s.take_pending_writes().iter().map(|r| r.uid).collect();
        assert_eq!(uids, [1, 2]);
        assert!(s.take_pending_writes().is_empty());
        assert_eq!(ping(&mut s).as_deref(), Some("setCAN "));
    }

    #[test]
    fn reports_confirmed_writes_and_queued_keys() {
        let mut s = synced();
        let w = crate::registers::flush(0x07, 1);
        s.queue_write(w);
        assert!(s.has_write_for(&key_of(&w)));
        ping(&mut s).unwrap();
        assert!(s.has_write_for(&key_of(&w))); // in flight
        s.drain_events();
        s.on_frame(&frame("getCAN 1 "));
        assert!(!s.has_write_for(&key_of(&w)));
        assert!(s.drain_events().contains(&Event::WriteConfirmed(vec![w])));
    }

    #[test]
    fn stays_quiet_while_box_is_busy() {
        let mut s = synced();
        s.on_frame(&frame("CAN2 in use"));
        assert_eq!(ping(&mut s), None);
    }

    #[test]
    fn falls_back_to_steady_if_no_unit_announces() {
        let mut s = Session::new();
        ping(&mut s);
        s.on_frame(&frame("CAN2 in use"));
        ping(&mut s); // global flush
        s.on_frame(&frame("getCAN 1 "));
        for _ in 0..30 {
            ping(&mut s);
        }
        assert_eq!(s.phase(), Phase::Steady);
    }
}
