//! Connects the session state machine to a byte link and to the API layer.
//!
//! [`Bridge::on_bytes`] is pure: bytes and the current time in, bytes to
//! transmit out. [`run`] is the thin I/O loop that drives it over anything
//! that is `Read + Write` (a serial port, or a mock in tests).
//!
//! The API layer talks to the bus through a [`Handle`]: a snapshot of the
//! register mirror and unit liveness, plus a channel for writes.
//!
//! Rules enforced here, on top of [`Session`]:
//! - listen-only mode never transmits and never touches the session;
//! - only the *latest* Ping in a read is answered, and only if nothing else
//!   followed it (a later frame means the slot is gone, or another tablet is
//!   still on the bus);
//! - a Ping with a bad checksum is not answered;
//! - our own frames read back on a half-duplex adapter are recognised as
//!   echoes, and any other tablet-side frame is counted as foreign.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::frame::{encode, Decoder, DecoderStats, Frame};
use crate::record::{parse_list, Record, DEST_CB, UNIT_AIRCON_RF};
use crate::registers::{SystemStatus, ZoneInfo, ZoneState};
use crate::session::{key_of, Event, Phase, RegKey, Session};
use crate::{log_debug, log_info, log_warn};

/// The original tablet writes at most 63 bytes at a time.
pub const MAX_CHUNK: usize = 63;
/// A wired unit is offline once none of its registers is seen for this long.
pub const OFFLINE_WIRED: Duration = Duration::from_secs(80);
/// Same, for RF/split units (unit type 08).
pub const OFFLINE_RF: Duration = Duration::from_secs(260);
/// Warn when no Ping has been seen for this long.
pub const BUS_SILENT: Duration = Duration::from_secs(5);
/// A frame identical to our last transmission within this window is an echo.
const ECHO_WINDOW: Duration = Duration::from_secs(1);
/// How often [`Bridge::tick`] logs a counter summary.
pub const SUMMARY_INTERVAL: Duration = Duration::from_secs(600);
/// Minimum gap between repeated warnings about the same condition.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// (unit type, unit id)
pub type Unit = (u8, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// Never transmit; only observe the bus.
    ListenOnly,
    /// Answer the control box's Pings in place of the tablet.
    Active,
}

/// Counters, for logs and later the API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub pings: u64,
    pub bad_crc: u64,
    /// Bytes outside any frame.
    pub noise_bytes: u64,
    pub truncated_frames: u64,
    pub malformed_frames: u64,
    /// Pings not answered because another frame followed in the same read.
    pub missed_slots: u64,
    /// Our own frames read back from a half-duplex adapter.
    pub echoes: u64,
    /// Tablet-side frames we did not send (another tablet on the bus?).
    pub foreign_tablet_frames: u64,
    /// Inbound records that failed range checks and were not applied.
    pub invalid_records: u64,
    pub writes_queued: u64,
    pub writes_dropped: u64,
    /// Times the serial port was (re)opened after the first.
    pub reconnects: u64,
}

/// Everything the API layer may read.
#[derive(Debug, Clone)]
pub struct BusState {
    pub mode: TxMode,
    /// Session phase; `None` in listen-only mode.
    pub phase: Option<Phase>,
    /// Primary aircon unit, once seen.
    pub unit: Option<Unit>,
    /// Latest valid value of every register.
    pub registers: HashMap<RegKey, Record>,
    /// When each unit's registers were last seen.
    pub last_seen: HashMap<Unit, Instant>,
    pub last_ping: Option<Instant>,
    pub stats: Stats,
}

impl BusState {
    fn new(mode: TxMode) -> Self {
        BusState {
            mode,
            phase: None,
            unit: None,
            registers: HashMap::new(),
            last_seen: HashMap::new(),
            last_ping: None,
            stats: Stats::default(),
        }
    }

    /// How long a unit may stay silent before it counts as offline.
    pub fn offline_after(unit_type: u8) -> Duration {
        if unit_type == UNIT_AIRCON_RF {
            OFFLINE_RF
        } else {
            OFFLINE_WIRED
        }
    }

    pub fn is_online(&self, unit: Unit, now: Instant) -> bool {
        self.last_seen
            .get(&unit)
            .is_some_and(|t| now.saturating_duration_since(*t) < Self::offline_after(unit.0))
    }

    /// Range-check and store a record from the box. Returns false if invalid.
    fn observe(&mut self, r: Record, now: Instant) -> bool {
        self.last_seen.insert((r.unit_type, r.uid), now);
        if let Err(e) = validate(&r) {
            self.stats.invalid_records += 1;
            log_warn!("invalid record {r} not applied: {e}");
            return false;
        }
        if self.unit.is_none() && r.is_aircon() && r.uid != 0 {
            self.unit = Some((r.unit_type, r.uid));
        }
        self.registers.insert(key_of(&r), r);
        true
    }
}

fn validate(r: &Record) -> Result<(), String> {
    let res = match r.register {
        SystemStatus::REG => SystemStatus::decode(r).map(drop),
        ZoneState::REG => ZoneState::decode(r).map(drop),
        ZoneInfo::REG => ZoneInfo::decode(r).map(drop),
        _ => Ok(()),
    };
    res.map_err(|e| format!("{e:?}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The daemon is in listen-only mode.
    ListenOnly,
    /// The session has not finished its startup dump yet.
    NotReady,
    /// Writes must be addressed to the control box.
    NotToControlBox,
    /// The bus loop has stopped.
    Disconnected,
}

/// Shared access to the bus for the API layer. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    state: Arc<Mutex<BusState>>,
    tx: Sender<Record>,
}

fn lock(m: &Mutex<BusState>) -> MutexGuard<'_, BusState> {
    // A panic while holding the lock leaves plain data; keep going.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Handle {
    pub fn snapshot(&self) -> BusState {
        lock(&self.state).clone()
    }

    /// Queue a register write for the next free slot. The session merges
    /// writes to the same register and resends until confirmed.
    pub fn queue_write(&self, r: Record) -> Result<(), WriteError> {
        {
            let st = lock(&self.state);
            if st.mode == TxMode::ListenOnly {
                return Err(WriteError::ListenOnly);
            }
            if st.phase != Some(Phase::Steady) {
                return Err(WriteError::NotReady);
            }
        }
        if r.dest != DEST_CB {
            return Err(WriteError::NotToControlBox);
        }
        self.tx.send(r).map_err(|_| WriteError::Disconnected)
    }
}

/// Rate limiter for repeated warnings.
#[derive(Default)]
struct Throttle(Option<Instant>);

impl Throttle {
    fn ready(&mut self, now: Instant) -> bool {
        if self
            .0
            .is_some_and(|t| now.saturating_duration_since(t) < WARN_INTERVAL)
        {
            return false;
        }
        self.0 = Some(now);
        true
    }
}

pub struct Bridge {
    mode: TxMode,
    decoder: Decoder,
    /// Decoder counters from before the last reset.
    carry: DecoderStats,
    session: Session,
    state: Arc<Mutex<BusState>>,
    commands: Receiver<Record>,
    last_tx: Option<(String, Instant)>,
    started: Instant,
    silent_warned: bool,
    foreign_warn: Throttle,
    online: HashMap<Unit, bool>,
    last_summary: Instant,
}

fn is_tablet_frame(p: &str) -> bool {
    p.starts_with("setCAN") || p.starts_with("ackCAN") || p == "getSystemData"
}

impl Bridge {
    pub fn new(mode: TxMode, now: Instant) -> (Bridge, Handle) {
        let state = Arc::new(Mutex::new(BusState::new(mode)));
        let (tx, rx) = mpsc::channel();
        let handle = Handle {
            state: state.clone(),
            tx,
        };
        let b = Bridge {
            mode,
            decoder: Decoder::new(),
            carry: DecoderStats::default(),
            session: Session::new(),
            state,
            commands: rx,
            last_tx: None,
            started: now,
            silent_warned: false,
            foreign_warn: Throttle::default(),
            online: HashMap::new(),
            last_summary: now,
        };
        if mode == TxMode::Active {
            b.state().phase = Some(b.session.phase());
        }
        (b, handle)
    }

    fn state(&self) -> MutexGuard<'_, BusState> {
        lock(&self.state)
    }

    /// Start a fresh session (after the serial port was reopened). The
    /// register mirror and unit liveness are kept.
    pub fn reset(&mut self, now: Instant) {
        let d = self.decoder.stats();
        self.carry.noise_bytes += d.noise_bytes;
        self.carry.truncated += d.truncated;
        self.carry.malformed += d.malformed;
        self.decoder = Decoder::new();
        self.session = Session::new();
        self.last_tx = None;
        self.started = now;
        self.silent_warned = false;
        let mut st = self.state();
        st.stats.reconnects += 1;
        if self.mode == TxMode::Active {
            st.phase = Some(Phase::Negotiating);
        }
    }

    /// Feed bytes read from the link. Returns the encoded frame to transmit,
    /// if this read ended with a Ping we should answer.
    pub fn on_bytes(&mut self, bytes: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.decoder.push(bytes);
        let mut frames = Vec::new();
        while let Some(f) = self.decoder.next_frame() {
            frames.push(f);
        }
        self.sync_decoder_stats();
        // Anything after the final frame (even a partial or malformed one)
        // means the slot is already in use.
        let trailing = self.decoder.has_trailing_input();
        let n = frames.len();
        let mut reply = None;
        for (i, f) in frames.into_iter().enumerate() {
            reply = self.on_frame(&f, i + 1 == n && !trailing, now);
        }
        let payload = reply?;
        log_debug!("tx {payload}");
        self.state().stats.tx_frames += 1;
        let bytes = encode(&payload);
        self.last_tx = Some((payload, now));
        Some(bytes)
    }

    fn sync_decoder_stats(&mut self) {
        let d = self.decoder.stats();
        let c = self.carry;
        let mut st = self.state();
        st.stats.noise_bytes = c.noise_bytes + d.noise_bytes;
        st.stats.truncated_frames = c.truncated + d.truncated;
        st.stats.malformed_frames = c.malformed + d.malformed;
    }

    /// Handle one frame; returns a payload only for a Ping we answer.
    fn on_frame(&mut self, f: &Frame, last: bool, now: Instant) -> Option<String> {
        log_debug!("rx{} {}", if f.crc_ok { "" } else { " BAD-CRC" }, f.payload);
        {
            let mut st = self.state();
            st.stats.rx_frames += 1;
            if !f.crc_ok {
                st.stats.bad_crc += 1;
            }
        }
        let p = f.payload.as_str();

        if p == "Ping" {
            if !f.crc_ok {
                return None;
            }
            {
                let mut st = self.state();
                st.stats.pings += 1;
                st.last_ping = Some(now);
            }
            if self.silent_warned {
                self.silent_warned = false;
                log_info!("bus Pings resumed");
            }
            if self.mode == TxMode::ListenOnly {
                return None;
            }
            if !last {
                self.state().stats.missed_slots += 1;
                log_debug!("Ping slot missed: other input followed it");
                return None;
            }
            // Writes wait in the channel until the session is ready, so a
            // reset cannot leak them into a new session's startup.
            if self.session.phase() == Phase::Steady {
                self.drain_commands();
            }
            let out = self.session.on_frame(f);
            self.process_events(now);
            return out;
        }

        if is_tablet_frame(p) {
            if self.mode == TxMode::Active {
                let echo = self.last_tx.as_ref().is_some_and(|(sent, t)| {
                    sent == p && now.saturating_duration_since(*t) < ECHO_WINDOW
                });
                if echo {
                    self.state().stats.echoes += 1;
                } else {
                    self.state().stats.foreign_tablet_frames += 1;
                    if self.foreign_warn.ready(now) {
                        log_warn!(
                            "another device is answering Pings ({p:?}); \
                             unplug the original tablet before running in active mode"
                        );
                    }
                }
            }
            return None;
        }

        match self.mode {
            TxMode::Active => {
                self.session.on_frame(f);
                self.process_events(now);
            }
            TxMode::ListenOnly => self.observe_passive(f, now),
        }
        None
    }

    /// Listen-only: learn registers from the box's replies to the real tablet.
    fn observe_passive(&mut self, f: &Frame, now: Instant) {
        if !f.crc_ok {
            return;
        }
        let Some(rest) = f.payload.strip_prefix("getCAN ") else {
            return;
        };
        let (status, body) = rest.split_once(' ').unwrap_or((rest, ""));
        if status != "1" {
            return;
        }
        let mut st = self.state();
        for r in parse_list(body) {
            if matches!(r.dest, 0x03 | 0x04) {
                st.observe(r, now);
            }
        }
    }

    fn drain_commands(&mut self) {
        let mut n = 0;
        while let Ok(r) = self.commands.try_recv() {
            log_info!("write queued: {}", crate::describe(&r));
            self.session.queue_write(r);
            n += 1;
        }
        self.state().stats.writes_queued += n;
    }

    fn process_events(&mut self, now: Instant) {
        let events = self.session.drain_events();
        let mut st = lock(&self.state);
        for e in events {
            match e {
                Event::Register(r) => {
                    st.observe(r, now);
                }
                Event::PhaseChanged(p) => {
                    log_info!("session phase: {p:?}");
                    st.phase = Some(p);
                }
                Event::WriteDropped(v) => {
                    st.stats.writes_dropped += v.len() as u64;
                    let list: Vec<String> = v.iter().map(|r| r.to_string()).collect();
                    log_warn!(
                        "control box never confirmed write; dropped: {}",
                        list.join(" ")
                    );
                }
                Event::BadChecksum(p) => log_debug!("ignored frame with bad checksum: {p}"),
                Event::LegacyXml(x) => log_info!("legacy XML from control box: {x}"),
            }
        }
        if let Some(u) = self.session.unit() {
            st.unit = Some(u);
        }
    }

    /// Periodic housekeeping: bus-silence warnings and unit online/offline
    /// transitions. Call at least once a second.
    pub fn tick(&mut self, now: Instant) {
        let st = lock(&self.state);
        let since = st.last_ping.unwrap_or(self.started);
        if !self.silent_warned && now.saturating_duration_since(since) >= BUS_SILENT {
            self.silent_warned = true;
            if st.last_ping.is_none() {
                log_warn!(
                    "no Ping from the control box yet; check wiring (A/B may be swapped) and port"
                );
            } else {
                log_warn!("no Ping from the control box for {}s", BUS_SILENT.as_secs());
            }
        }
        for &unit in st.last_seen.keys() {
            let on = st.is_online(unit, now);
            if self.online.insert(unit, on) != Some(on) {
                let (t, id) = unit;
                if on {
                    log_info!("unit {t:02x}:{id:05x} online");
                } else {
                    log_warn!("unit {t:02x}:{id:05x} offline");
                }
            }
        }
        if now.saturating_duration_since(self.last_summary) >= SUMMARY_INTERVAL {
            self.last_summary = now;
            log_info!(
                "phase {:?}, unit {}, {} registers; {:?}",
                st.phase,
                st.unit
                    .map(|(t, u)| format!("{t:02x}:{u:05x}"))
                    .unwrap_or_else(|| "none".into()),
                st.registers.len(),
                st.stats
            );
        }
        drop(st);
        if self.mode == TxMode::ListenOnly {
            // Handle refuses writes in this mode; discard any that raced in.
            while self.commands.try_recv().is_ok() {}
        }
    }
}

/// Write a frame in chunks of at most [`MAX_CHUNK`] bytes.
pub fn write_chunked<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    for c in bytes.chunks(MAX_CHUNK) {
        w.write_all(c)?;
    }
    w.flush()
}

/// Drive the bridge over a link until it reports end of input (`Ok`) or an
/// I/O error (`Err`). Read timeouts are expected and only trigger [`Bridge::tick`].
pub fn run<L: Read + Write>(link: &mut L, bridge: &mut Bridge) -> io::Result<()> {
    let mut buf = [0u8; 512];
    loop {
        match link.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if let Some(out) = bridge.on_bytes(&buf[..n], Instant::now()) {
                    write_chunked(link, &out)?;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
        bridge.tick(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registers::{flush, ZoneState};
    use std::collections::VecDeque;

    /// In-memory link: replays scripted reads, records writes, and can echo
    /// writes back like a half-duplex RS-485 adapter.
    #[derive(Default)]
    struct MockLink {
        input: VecDeque<Vec<u8>>,
        /// Every individual `write` call.
        writes: Vec<Vec<u8>>,
        /// Index of the input chunk each complete frame was sent after.
        sent_after: Vec<usize>,
        reads: usize,
        echo: bool,
        pending: Vec<u8>,
    }

    impl MockLink {
        fn new(chunks: &[&str]) -> Self {
            MockLink {
                input: chunks.iter().map(|c| c.as_bytes().to_vec()).collect(),
                ..Default::default()
            }
        }
        /// The decoded payloads we transmitted.
        fn sent(&self) -> Vec<String> {
            let mut d = Decoder::new();
            d.push(&self.writes.concat());
            std::iter::from_fn(|| d.next_frame())
                .map(|f| {
                    assert!(f.crc_ok);
                    f.payload
                })
                .collect()
        }
    }

    impl Read for MockLink {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(c) = self.input.pop_front() else {
                return Ok(0);
            };
            self.reads += 1;
            buf[..c.len()].copy_from_slice(&c);
            Ok(c.len())
        }
    }

    impl Write for MockLink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.writes.push(b.to_vec());
            self.pending.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            let frame = std::mem::take(&mut self.pending);
            self.sent_after.push(self.reads);
            if self.echo {
                self.input.push_front(frame);
            }
            Ok(())
        }
    }

    fn fr(p: &str) -> String {
        String::from_utf8(encode(p)).unwrap()
    }

    /// The control box's side of a normal startup for unit abcde.
    fn startup_script() -> Vec<String> {
        vec![
            fr("Ping"),
            fr("CAN2 in use"),
            fr("Ping"),
            fr("getCAN 1 0703abcde0602030100000000"),
            fr("Ping"),
            fr("Ping"),
            fr("getCAN 1 0703abcde0501010330000100 0703abcde0301e40030000000"),
            fr("Ping"),
            fr("Ping"),
        ]
    }

    const STARTUP_REPLIES: [&str; 6] = [
        "getSystemData",
        "setCAN 0701000000600000000000000",
        "ackCAN 1",
        "setCAN 0701abcde0700000000000000 0701abcde0600000000000000",
        "ackCAN 1",
        "setCAN ",
    ];

    fn run_script(mode: TxMode, script: &[String], echo: bool) -> (MockLink, Bridge, Handle) {
        let (mut b, h) = Bridge::new(mode, Instant::now());
        let chunks: Vec<&str> = script.iter().map(String::as_str).collect();
        let mut link = MockLink::new(&chunks);
        link.echo = echo;
        run(&mut link, &mut b).unwrap();
        (link, b, h)
    }

    #[test]
    fn active_startup_over_mock_link() {
        let (link, _b, h) = run_script(TxMode::Active, &startup_script(), false);
        assert_eq!(link.sent(), STARTUP_REPLIES);
        // Every reply was sent straight after a Ping (chunks 1,3,5,6,8,9).
        assert_eq!(link.sent_after, [1, 3, 5, 6, 8, 9]);
        let st = h.snapshot();
        assert_eq!(st.phase, Some(Phase::Steady));
        assert_eq!(st.unit, Some((0x07, 0xabcde)));
        assert_eq!(st.registers.len(), 3);
        assert!(st.is_online((0x07, 0xabcde), Instant::now()));
        assert_eq!(st.stats.pings, 6);
        assert_eq!(st.stats.tx_frames, 6);
        assert_eq!(st.stats.foreign_tablet_frames, 0);
    }

    #[test]
    fn half_duplex_echo_is_ignored() {
        let (link, _b, h) = run_script(TxMode::Active, &startup_script(), true);
        assert_eq!(link.sent(), STARTUP_REPLIES);
        let st = h.snapshot();
        assert_eq!(st.stats.echoes, 6);
        assert_eq!(st.stats.foreign_tablet_frames, 0);
        assert_eq!(st.phase, Some(Phase::Steady));
    }

    #[test]
    fn listen_only_never_transmits_but_learns_registers() {
        // The real tablet's traffic is on the bus too.
        let mut script = vec![];
        for (cb, tablet) in startup_script().iter().zip(STARTUP_REPLIES.iter().cycle()) {
            script.push(cb.clone());
            script.push(fr(tablet));
        }
        let (link, _b, h) = run_script(TxMode::ListenOnly, &script, false);
        assert!(link.writes.is_empty());
        let st = h.snapshot();
        assert_eq!(st.phase, None);
        assert_eq!(st.unit, Some((0x07, 0xabcde)));
        assert_eq!(st.registers.len(), 3);
        assert_eq!(st.stats.foreign_tablet_frames, 0);
        assert_eq!(
            h.queue_write(flush(0x07, 0xabcde)),
            Err(WriteError::ListenOnly)
        );
    }

    #[test]
    fn writes_go_out_in_63_byte_chunks() {
        let mut script = startup_script();
        script.push(fr("getCAN 1 "));
        let (mut b, h) = Bridge::new(TxMode::Active, Instant::now());
        let chunks: Vec<&str> = script.iter().map(String::as_str).collect();
        let mut link = MockLink::new(&chunks);
        run(&mut link, &mut b).unwrap();
        for zone in 1..=10 {
            let z = ZoneState {
                zone,
                open: true,
                percent: 50,
                sensor_type: 0,
                set_temp: 22.0,
                measured_temp: None,
            };
            h.queue_write(z.encode_write(0x07, 0xabcde).unwrap())
                .unwrap();
        }
        link.input.push_back(encode("Ping"));
        link.input.push_back(encode("Ping"));
        link.writes.clear();
        run(&mut link, &mut b).unwrap();
        assert!(link.writes.iter().all(|w| w.len() <= MAX_CHUNK));
        let sent = link.sent();
        assert_eq!(sent[0], "ackCAN 1");
        assert!(sent[1].starts_with("setCAN 0701abcde0301b2"));
        assert_eq!(sent[1].split(' ').count(), 11);
        assert!(link.writes.len() > 5);
        assert_eq!(h.snapshot().stats.writes_queued, 10);
    }

    #[test]
    fn writes_refused_until_steady_and_must_target_cb() {
        let (_b, h) = Bridge::new(TxMode::Active, Instant::now());
        assert_eq!(h.queue_write(flush(7, 1)), Err(WriteError::NotReady));
        let (_l, _b, h) = run_script(TxMode::Active, &startup_script(), false);
        let mut r = flush(7, 0xabcde);
        r.dest = 0x03;
        assert_eq!(h.queue_write(r), Err(WriteError::NotToControlBox));
        assert_eq!(h.queue_write(flush(7, 0xabcde)), Ok(()));
    }

    #[test]
    fn answers_only_the_last_ping_in_a_read() {
        let (mut b, h) = Bridge::new(TxMode::Active, Instant::now());
        let t = Instant::now();
        let two = [encode("Ping"), encode("Ping")].concat();
        let out = b.on_bytes(&two, t).unwrap();
        assert_eq!(out, encode("getSystemData"));
        // Ping followed by someone else's reply: the slot is gone.
        let taken = [encode("Ping"), encode("setCAN ")].concat();
        assert_eq!(b.on_bytes(&taken, t), None);
        let st = h.snapshot().stats;
        assert_eq!(st.missed_slots, 2);
        assert_eq!(st.foreign_tablet_frames, 1);
    }

    #[test]
    fn partial_or_malformed_input_after_ping_suppresses_reply() {
        let t = Instant::now();
        for tail in [&b"<U>setCAN "[..], b"<U>setCAN </U=zz>"] {
            let (mut b, h) = Bridge::new(TxMode::Active, t);
            let bytes = [encode("Ping"), tail.to_vec()].concat();
            assert_eq!(b.on_bytes(&bytes, t), None);
            assert_eq!(h.snapshot().stats.missed_slots, 1);
            // The session did not advance: the next clean Ping negotiates.
            b.on_bytes(b"</U=b2>", t);
            assert_eq!(
                b.on_bytes(&encode("Ping"), t),
                Some(encode("getSystemData"))
            );
        }
        // Trailing whitespace is fine.
        let (mut b, _h) = Bridge::new(TxMode::Active, t);
        let bytes = [encode("Ping"), b" \r\n".to_vec()].concat();
        assert_eq!(b.on_bytes(&bytes, t), Some(encode("getSystemData")));
    }

    #[test]
    fn unknown_mode_is_applied_as_auto() {
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        b.on_bytes(
            &encode("getCAN 1 0703abcde0501060330000100"),
            Instant::now(),
        );
        let st = h.snapshot();
        assert_eq!(st.stats.invalid_records, 0);
        assert_eq!(st.registers.len(), 1);
    }

    #[test]
    fn writes_buffered_across_reset_wait_for_steady() {
        let (_l, mut b, h) = run_script(TxMode::Active, &startup_script(), false);
        h.queue_write(flush(0x07, 0x12345)).unwrap();
        b.reset(Instant::now());
        let script = startup_script();
        let chunks: Vec<&str> = script.iter().map(String::as_str).collect();
        let mut link = MockLink::new(&chunks);
        run(&mut link, &mut b).unwrap();
        let sent = link.sent();
        // Startup is unchanged; the write goes out only once Steady.
        assert_eq!(sent[..5], STARTUP_REPLIES[..5]);
        assert_eq!(sent[5], "setCAN 0701123450600000000000000");
    }

    #[test]
    fn bad_checksum_ping_is_not_answered() {
        let (mut b, h) = Bridge::new(TxMode::Active, Instant::now());
        assert_eq!(b.on_bytes(b"<U>Ping</U=00>", Instant::now()), None);
        assert_eq!(h.snapshot().stats.bad_crc, 1);
    }

    #[test]
    fn invalid_records_are_flagged_not_applied() {
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, Instant::now());
        // Zone 11 and mode 2-but-on=2 are out of range; zone 1 is fine.
        let f = encode("getCAN 1 0703abcde030be40030000000 0703abcde0502010330000100 0703abcde0301e40030000000");
        b.on_bytes(&f, Instant::now());
        let st = h.snapshot();
        assert_eq!(st.stats.invalid_records, 2);
        assert_eq!(st.registers.len(), 1);
    }

    #[test]
    fn units_go_offline_after_80s_wired_260s_rf() {
        let t0 = Instant::now();
        let (mut b, h) = Bridge::new(TxMode::ListenOnly, t0);
        b.on_bytes(
            &encode("getCAN 1 0703abcde0501010330000100 0803fffff0501010330000100"),
            t0,
        );
        let st = h.snapshot();
        let wired = (0x07, 0xabcde);
        let rf = (0x08, 0xfffff);
        let at = |s| t0 + Duration::from_secs(s);
        assert!(st.is_online(wired, at(79)) && !st.is_online(wired, at(80)));
        assert!(st.is_online(rf, at(259)) && !st.is_online(rf, at(260)));
        assert!(!st.is_online((0x07, 1), t0));
        b.tick(at(100));
        assert_eq!(b.online.get(&wired), Some(&false));
        assert_eq!(b.online.get(&rf), Some(&true));
    }

    #[test]
    fn counts_noise_and_survives_garbage() {
        let (mut b, h) = Bridge::new(TxMode::Active, Instant::now());
        let out = b.on_bytes(
            b"\x00\xffjunk<U>getCAN 1 0703<U>Ping</U=db>",
            Instant::now(),
        );
        assert_eq!(out, Some(encode("getSystemData")));
        let st = h.snapshot().stats;
        assert_eq!(st.noise_bytes, 6);
        assert_eq!(st.truncated_frames, 1);
    }

    #[test]
    fn reset_restarts_session_but_keeps_mirror() {
        let (_l, mut b, h) = run_script(TxMode::Active, &startup_script(), false);
        b.reset(Instant::now());
        let st = h.snapshot();
        assert_eq!(st.phase, Some(Phase::Negotiating));
        assert_eq!(st.registers.len(), 3);
        assert_eq!(st.stats.reconnects, 1);
        assert_eq!(
            b.on_bytes(&encode("Ping"), Instant::now()),
            Some(encode("getSystemData"))
        );
    }
}
