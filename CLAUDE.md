# CLAUDE.md

Guidance for Claude Code working in this repository.

## Project

ductbridge replaces the Android wall tablet of an Advantage Air ducted
air-conditioning zone controller. It runs on a Raspberry Pi with a USB-RS485
adapter, talks directly to the control box (CB) over RS-485, and will serve
the same local HTTP API (port 2025) the tablet serves, so existing phone apps
and Home Assistant's `advantage_air` integration keep working unchanged.

Target deployment: Raspberry Pi 3 Model B/B+, 64-bit Raspberry Pi OS Lite,
**wired Ethernet only (no Wi-Fi dependency)**, USB-RS485 dongle (FTDI or
CH340) at `/dev/ttyUSB0`, run as a systemd service. Binaries are built by CI
(cross-compiled), not on the Pi.

## Repository rules

- **Public repo.** No personal details (names, addresses, emails, IPs, unit
  IDs from a real system, photos). Use placeholder unit id `abcde` in tests.
- **No vendor material.** Never add Advantage Air code, binaries, APKs,
  decompiled sources, or tables copied from them. Cite the public aa_interop
  spec or "observed behaviour of the original tablet" instead.
- Keep Advantage Air product names out of crate/binary names; describing
  compatibility in docs is fine. Keep the "not affiliated" notice in README.
- Every change: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` must pass. Add tests with each fix or feature.
- Keep protocol logic pure (no I/O) so it is testable; I/O lives in thin
  adapters and binaries.

## Layout

```
src/crc.rs        CRC-8 for frames
src/frame.rs      <U>payload</U=xx> encoder + streaming decoder
src/record.rs     25-hex-char CAN2 register records
src/registers.rs  typed registers 01, 03, 05, 06, 07, 08, 0a + write builders
src/session.rs    tablet-side session state machine (pure)
src/bridge.rs     Bridge (session + decoder + shared BusState, pure on_bytes/tick),
                  Handle for the API layer, run() I/O loop, MockLink tests
src/config.rs     defaults < config file < CLI (pure; file reader injected)
src/log.rs        levelled stderr logging; journald priorities under systemd
src/lib.rs        module wiring + describe() for logs
src/bin/ductbridge.rs           bus daemon (opens port, reconnects, runs bridge)
src/bin/ductbridge-monitor.rs   listen-only bus monitor (never transmits)
packaging/        systemd unit, example config, INSTALL.md
.github/workflows/ci.yml        fmt, clippy, test, aarch64 cross-build, release on v* tag
```

## Protocol facts (confirmed)

Physical (aa_interop spec; verify pins with a multimeter on each system):

| RJ45 pin | Signal |
|---|---|
| 1 | RS-485 B (+) |
| 2 | RS-485 A (−) |
| 4, 6 | GND |
| 5 | ~14 V DC to tablet — never connect to the adapter |

- RS-485 half-duplex, **57600 baud, 8N1**. Confirmed two ways: spec, and the
  original tablet's USB-serial bridge configuration.
- Frame: `<U>payload</U=xx>`; `xx` = CRC-8 of payload, two lowercase hex.
- CRC-8: reflected, poly 0xB2, init 0x00, final XOR 0xFF. Golden values:
  `Ping`→`db`, `setCAN `→`b2`, `ackCAN 1`→`aa`, `getSystemData`→`15`,
  `getCAN 1 `→`00`, `setCAN 0201000000236000000000000 `→`ce` (the last two
  from a real 2024 bus capture).
- Original tablet writes frames in chunks of at most 63 bytes.
- Record: `TT DD UUUUU RR PPPPPPPPPPPPPP` (25 hex chars). TT unit type
  (02 lights, 07 aircon wired, 08 aircon RF/split). DD destination (01 = to
  CB; 03 or 04 = to tablet). UUUUU 20-bit unit id. RR register. 7 payload bytes.
  Records not exactly 25 chars are ignored (as the original tablet does).
- Split-system CBs use unit type 08; always echo the CB's unit type in writes.

Bus conversation:

- CB sends `Ping` about once a second; the tablet may send **one** frame in
  the slot after each Ping, and must never transmit otherwise.
- Startup: reply `getSystemData` to each Ping until the CB answers anything
  (modern CBs answer `CAN2 in use`; older ones may send XML `<request>…`).
- Dump: `setCAN 0701000000600000000000000` (global flush). CB announces with
  register 06 (firmware). Reply to **every** register-06 announcement with
  register 07, all-zero payload, same unit type and id (mandatory). Then send
  a unit-scoped flush (register 06 with the unit's id).
- Steady state reply priority per Ping: `ackCAN` > resend unconfirmed write >
  queued writes > empty poll `setCAN `.
- After any `getCAN`, the next slot is `ackCAN 1` (CRC good) or `ackCAN 0`
  (CRC bad; do not apply its records).
- `getCAN 1 …` = accepted + register updates; `getCAN 0` = nack, resend.
  A write stays in flight until `getCAN 1`; resend on later Pings (bounded).
- Max **25 records per `setCAN`** (original tablet's limit). Records joined
  by single spaces, no trailing space; empty poll is exactly `setCAN `.
- Do not send the empty poll while `CAN2 in use` is outstanding.
- Original tablet marks a unit offline if its registers are not seen for 80 s
  (260 s for RF/split units).

Registers in use (aa_interop `04-registers.md`; the spec's *tables* are
reliable, a few of its *worked examples* contradict them, e.g. an error code
labelled "AA14" whose bytes decode to "AA4"):

- 05 system status: on(0/1), mode(1 cool, 2 heat, 3 vent, 4 auto, 5 dry;
  decode anything else as auto), fan(0–5), setTemp×2 (≤80), myZone(0–10),
  freshAir(0–2), rfSysId(≤16).
- 03 zone state: zone(1–10), bit7 open + bits0–6 percent(≤100),
  sensorType(0–4), setTemp×2, measured int, measured decimal(0–9). On write,
  sensor and measured bytes are 0.
- 04 zone config, 01 zone info, 06 firmware/flush, 07 firmware ack,
  08 error (5 ASCII chars), 0a unit announcement, 12 sensor pairing.

Safety choices already implemented:

- Setpoint writes refused outside **16–32 °C** (the original app's
  thermostat range).
- Inbound records range-checked; invalid ones flagged, not applied.
- Writes to the same register/zone are merged; latest value wins.

## Status

- Milestone 1 (done): protocol core and monitor.
- Milestone 2 (done): `ductbridge` daemon, packaging, CI. 47 tests passing;
  also smoke-tested end to end against a scripted fake CB on a pty.
- **Nothing has been run against a real control box yet.**
- Which session dialect the target CB uses is unknown (register-only "CAN2"
  vs also answering legacy XML). Code must work with register-only.

Bridge behaviour worth knowing (beyond the session rules above):

- Listen-only never calls the session; it learns registers passively from
  `getCAN 1` frames addressed to the tablet. `Handle::queue_write` refuses.
- Active: only the last Ping in a read is answered, and only if no other
  input followed it — a frame, a partial frame or a malformed one; whitespace
  is allowed (slot gone / another tablet). Bad-CRC Pings are ignored.
- Our own frames read back within 1 s are counted as echoes; any other
  tablet-side frame (`setCAN`/`ackCAN`/`getSystemData`) is counted as
  foreign and warned about (rate-limited). It does not stop transmitting.
- Writes are refused until the session is `Steady` and must have dest 01.
  Channel commands are only drained into the session while it is `Steady`
  (so writes buffered across a reconnect wait for the new session).
- Unknown system-status mode codes decode as auto, not as invalid.
- On serial errors the daemon reopens the port every 5 s and restarts the
  session; the register mirror, liveness and counters are kept.

## Next: milestone 3 — port-2025 HTTP API

Reproduce the tablet's local API so existing clients work unchanged:
`GET /getSystemData`, `GET /setAircon?json={…}`, plus zone/MyZone settings
carried inside `setAircon`. Sources for the JSON shape: aa_interop
`docs/tablet_api/README.md` and Home Assistant's `advantage_air`
integration (and its `advantage_air` PyPI client). Known quirk: clients send
**unescaped JSON in the query string**, so parse the raw query manually rather
than relying on strict URL decoding. Lights and other "things" endpoints are
out of scope initially (return empty structures).

## Milestone 4 — install

Multimeter check of pins, connect A/B/GND only, unplug the original tablet
(keep it as a fallback), run `ductbridge-monitor` first, then the daemon,
then test with the phone app and Home Assistant.

## References

- https://github.com/gundy/aa_interop — protocol spec (GFDL)
- https://github.com/retsimx/cb-daemon — Rust reference implementation (MIT)
- https://github.com/iduno/MyPlace — Java implementation
- https://www.home-assistant.io/integrations/advantage_air/
