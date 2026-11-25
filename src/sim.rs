//! A simulated control box, for trying ductbridge (and the apps that talk to
//! it) without the real hardware: `ductbridge --simulate`.
//!
//! It speaks the bus protocol as the CB does (per the aa_interop spec and
//! observed traffic): Ping about once a second, `CAN2 in use` in answer to
//! `getSystemData`, the register dump on flush requests, and `getCAN 1`
//! replies that apply register 05 / 03 writes. Zone temperatures drift
//! towards their setpoints while the unit runs, so features such as climate
//! control and MyAuto have something to do.
//!
//! [`SimBox`] is pure (bytes and time in, bytes out); [`SimLink`] wraps it as
//! a `Read + Write` link for [`bridge::run`](crate::bridge::run).

use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use crate::frame::{encode, Decoder};
use crate::record::{parse_list, Record, DEST_CB};

/// Placeholder unit (never a real system's id).
pub const SIM_UNIT: (u8, u32) = (0x07, 0xabcde);
const PING_EVERY: Duration = Duration::from_secs(1);
/// Delay before the box answers a tablet frame (well inside the slot).
const REPLY_DELAY: Duration = Duration::from_millis(20);
/// Temperature drift step interval.
const DRIFT_EVERY: Duration = Duration::from_secs(10);
/// Re-report the status this often even if nothing changed, so the unit
/// stays online (the tablet marks it offline after 80 s of silence).
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Room temperature the zones drift to when not conditioned.
const AMBIENT: f32 = 26.0;

/// Register keys the box tracks: (register, zone).
type Key = (u8, u8);

pub struct SimBox {
    /// Register 05 payload.
    status: [u8; 7],
    /// Register 03 payload per zone (index = zone − 1).
    zones: Vec<[u8; 7]>,
    /// Measured temperature per zone (sensor zones only).
    measured: Vec<Option<f32>>,
    decoder: Decoder,
    out: VecDeque<(Instant, Vec<u8>)>,
    next_ping: Instant,
    next_drift: Instant,
    next_heartbeat: Instant,
    /// Registers changed since last reported.
    dirty: BTreeSet<Key>,
}

fn temp_bytes(t: f32) -> (u8, u8) {
    let tenths = (t * 10.0).round() as i32;
    ((tenths / 10) as u8, (tenths % 10) as u8)
}

impl SimBox {
    /// Four zones: 1 and 2 with temperature sensors, 3 and 4 damper-only.
    /// Unit off, cool, fan medium, 24 °C, no MyZone, fresh air fitted (off).
    pub fn new(now: Instant) -> Self {
        let zone = |n: u8, open: bool, pct: u8, sensor: u8, set: f32| {
            [
                n,
                u8::from(open) << 7 | pct,
                sensor,
                (set * 2.0) as u8,
                0,
                0,
                0,
            ]
        };
        let mut b = SimBox {
            status: [0, 1, 2, 48, 0, 1, 0],
            zones: vec![
                zone(1, true, 100, 2, 23.0),
                zone(2, true, 100, 2, 22.0),
                zone(3, true, 60, 0, 24.0),
                zone(4, false, 0, 0, 24.0),
            ],
            measured: vec![Some(25.4), Some(27.1), None, None],
            decoder: Decoder::new(),
            out: VecDeque::new(),
            next_ping: now + PING_EVERY,
            next_drift: now + DRIFT_EVERY,
            next_heartbeat: now + HEARTBEAT,
            dirty: BTreeSet::new(),
        };
        b.write_measured();
        b
    }

    fn rec(&self, register: u8, payload: [u8; 7]) -> Record {
        Record {
            unit_type: SIM_UNIT.0,
            dest: 0x03,
            uid: SIM_UNIT.1,
            register,
            payload,
        }
    }

    fn record_for(&self, (register, zone): Key) -> Record {
        let n = self.zones.len() as u8;
        match register {
            0x01 => self.rec(0x01, [0x20, n, 1, 1, 0, 0, 0]),
            0x03 => self.rec(0x03, self.zones[zone as usize - 1]),
            0x04 => self.rec(0x04, [zone, 0, 100, 0, 0, 0, 0]),
            0x05 => self.rec(0x05, self.status),
            _ => self.rec(0x06, [2, 3, 3, 0, 0, 0, 0]), // firmware 2.3, cb type 3
        }
    }

    fn full_dump(&self) -> Vec<Record> {
        let mut v = vec![self.record_for((0x01, 0)), self.record_for((0x05, 0))];
        for z in 1..=self.zones.len() as u8 {
            v.push(self.record_for((0x03, z)));
            v.push(self.record_for((0x04, z)));
        }
        v
    }

    fn write_measured(&mut self) {
        for (i, m) in self.measured.iter().enumerate() {
            if let Some(t) = m {
                let (int, dec) = temp_bytes(*t);
                self.zones[i][4] = int;
                self.zones[i][5] = dec;
            }
        }
    }

    fn send(&mut self, payload: &str, now: Instant) {
        self.out.push_back((now + REPLY_DELAY, encode(payload)));
    }

    fn reply(&mut self, recs: &[Record], now: Instant) {
        let body: Vec<String> = recs.iter().map(|r| r.to_string()).collect();
        self.send(&format!("getCAN 1 {}", body.join(" ")), now);
    }

    /// Bytes the tablet (ductbridge) transmitted.
    pub fn on_tx(&mut self, bytes: &[u8], now: Instant) {
        self.decoder.push(bytes);
        while let Some(f) = self.decoder.next_frame() {
            if !f.crc_ok {
                continue;
            }
            let p = f.payload.as_str();
            if p == "getSystemData" {
                self.send("CAN2 in use", now);
            } else if let Some(body) = p.strip_prefix("setCAN") {
                self.on_set_can(body.trim_start(), now);
            }
            // ackCAN and anything else: nothing to answer.
        }
    }

    fn on_set_can(&mut self, body: &str, now: Instant) {
        let mut out: Vec<Record> = Vec::new();
        let writes = parse_list(body);
        for w in &writes {
            if w.dest != DEST_CB {
                continue;
            }
            match w.register {
                // Global flush: announce the unit. Unit flush: full dump.
                0x06 if w.uid == 0 => out.push(self.record_for((0x06, 0))),
                0x06 if w.uid == SIM_UNIT.1 => out.extend(self.full_dump()),
                0x05 if w.uid == SIM_UNIT.1 => {
                    let keep = self.status[6];
                    self.status = w.payload;
                    self.status[6] = keep;
                    out.push(self.record_for((0x05, 0)));
                    self.dirty.remove(&(0x05, 0));
                }
                0x03 if w.uid == SIM_UNIT.1 => {
                    let n = w.payload[0];
                    if (1..=self.zones.len() as u8).contains(&n) {
                        let z = &mut self.zones[n as usize - 1];
                        z[1] = w.payload[1];
                        z[3] = w.payload[3];
                        out.push(self.record_for((0x03, n)));
                        self.dirty.remove(&(0x03, n));
                    }
                }
                _ => {}
            }
        }
        if writes.is_empty() {
            // Empty poll: report what changed, if anything.
            if self.dirty.is_empty() {
                return;
            }
            let keys: Vec<Key> = std::mem::take(&mut self.dirty).into_iter().collect();
            out = keys.into_iter().map(|k| self.record_for(k)).collect();
        }
        self.reply(&out, now);
    }

    fn drift(&mut self) {
        let on = self.status[0] == 1;
        let conditioning = on && matches!(self.status[1], 1 | 2);
        for i in 0..self.zones.len() {
            let Some(t) = self.measured[i] else { continue };
            let z = self.zones[i];
            let open = z[1] & 0x80 != 0 && z[1] & 0x7f > 0;
            let target = if conditioning && open {
                z[3] as f32 / 2.0
            } else {
                AMBIENT
            };
            let step = if (target - t).abs() < 0.1 {
                target - t
            } else {
                0.1f32.copysign(target - t)
            };
            if step != 0.0 {
                self.measured[i] = Some(t + step);
                self.dirty.insert((0x03, i as u8 + 1));
            }
        }
        self.write_measured();
    }

    /// The next bytes the box sends, if any are due by `now`.
    pub fn poll(&mut self, now: Instant) -> Option<Vec<u8>> {
        if now >= self.next_drift {
            self.next_drift = now + DRIFT_EVERY;
            self.drift();
        }
        if now >= self.next_heartbeat {
            self.next_heartbeat = now + HEARTBEAT;
            self.dirty.insert((0x05, 0));
        }
        if self.out.front().is_some_and(|(t, _)| *t <= now) {
            return self.out.pop_front().map(|(_, b)| b);
        }
        if now >= self.next_ping {
            self.next_ping = now + PING_EVERY;
            return Some(encode("Ping"));
        }
        None
    }

    /// When something is next due.
    pub fn next_due(&self) -> Instant {
        let mut t = self.next_ping;
        if let Some((r, _)) = self.out.front() {
            t = t.min(*r);
        }
        t
    }
}

/// [`SimBox`] as a serial-port stand-in for [`bridge::run`](crate::bridge::run).
pub struct SimLink {
    sim: SimBox,
}

impl SimLink {
    pub fn new() -> Self {
        SimLink {
            sim: SimBox::new(Instant::now()),
        }
    }
}

impl Default for SimLink {
    fn default() -> Self {
        Self::new()
    }
}

/// Like a serial read with a 20 ms timeout.
const READ_TIMEOUT: Duration = Duration::from_millis(20);

impl Read for SimLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let now = Instant::now();
            if let Some(bytes) = self.sim.poll(now) {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                return Ok(n);
            }
            if now >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let wait = self
                .sim
                .next_due()
                .min(deadline)
                .saturating_duration_since(now);
            std::thread::sleep(wait.max(Duration::from_millis(1)));
        }
    }
}

impl Write for SimLink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sim.on_tx(buf, Instant::now());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{self, ApiInfo};
    use crate::bridge::{Bridge, TxMode};
    use crate::session::Phase;

    /// Run the bridge against the simulated box for `secs` of simulated time.
    fn drive(b: &mut Bridge, sim: &mut SimBox, t: &mut Instant, secs: u64) {
        let end = *t + Duration::from_secs(secs);
        while *t < end {
            *t += Duration::from_millis(10);
            while let Some(bytes) = sim.poll(*t) {
                if let Some(reply) = b.on_bytes(&bytes, *t) {
                    sim.on_tx(&reply, *t);
                }
            }
        }
    }

    #[test]
    fn bridge_reaches_steady_and_serves_the_simulated_system() {
        let mut t = Instant::now();
        let mut sim = SimBox::new(t);
        let (mut b, h) = Bridge::new(TxMode::Active, t);
        drive(&mut b, &mut sim, &mut t, 10);
        let st = h.snapshot();
        assert_eq!(st.phase, Some(Phase::Steady));
        assert_eq!(st.unit, Some(SIM_UNIT));
        let d = api::system_data(&st, &ApiInfo::default(), t).unwrap();
        let zones = d["aircons"]["ac1"]["zones"].as_object().unwrap();
        assert_eq!(zones.len(), 4);
        assert_eq!(d["aircons"]["ac1"]["zones"]["z01"]["type"], 1);
        assert_eq!(d["aircons"]["ac1"]["zones"]["z03"]["type"], 0);
        assert_eq!(d["aircons"]["ac1"]["info"]["freshAirStatus"], "off");
    }

    #[test]
    fn writes_are_applied_and_reported_back() {
        let mut t = Instant::now();
        let mut sim = SimBox::new(t);
        let (mut b, h) = Bridge::new(TxMode::Active, t);
        drive(&mut b, &mut sim, &mut t, 10);
        let now = t;
        h.submit(now, |st| {
            api::set_aircon(
                r#"json={"ac1":{"info":{"state":"on","mode":"heat"},"zones":{"z03":{"value":80}}}}"#,
                st,
                now,
            )
        })
        .unwrap();
        drive(&mut b, &mut sim, &mut t, 5);
        let d = api::system_data(&h.snapshot(), &ApiInfo::default(), t).unwrap();
        assert_eq!(d["aircons"]["ac1"]["info"]["state"], "on");
        assert_eq!(d["aircons"]["ac1"]["info"]["mode"], "heat");
        assert_eq!(d["aircons"]["ac1"]["zones"]["z03"]["value"], 80);
        assert!(h.snapshot().pending.is_empty(), "confirmed and reflected");
    }

    #[test]
    fn temperatures_drift_and_the_unit_stays_online() {
        let mut t = Instant::now();
        let mut sim = SimBox::new(t);
        let (mut b, h) = Bridge::new(TxMode::Active, t);
        drive(&mut b, &mut sim, &mut t, 10);
        let now = t;
        h.submit(now, |st| {
            api::set_aircon(
                r#"json={"ac1":{"info":{"state":"on","mode":"cool"}}}"#,
                st,
                now,
            )
        })
        .unwrap();
        let before = api::system_data(&h.snapshot(), &ApiInfo::default(), t).unwrap()["aircons"]
            ["ac1"]["zones"]["z02"]["measuredTemp"]
            .as_f64()
            .unwrap();
        // Well past the 80 s offline window.
        drive(&mut b, &mut sim, &mut t, 200);
        let d = api::system_data(&h.snapshot(), &ApiInfo::default(), t).expect("still online");
        let after = d["aircons"]["ac1"]["zones"]["z02"]["measuredTemp"]
            .as_f64()
            .unwrap();
        assert!(after < before, "cooling towards 22 °C: {before} -> {after}");
    }
}
