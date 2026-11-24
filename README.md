# ductbridge

An open replacement for the Android wall tablet used by some ducted
air-conditioning zone controllers, including Advantage Air control boxes.
Runs on a Raspberry Pi with a USB-RS485 adapter and talks directly to the
control box, so the system keeps working when the tablet fails.

> Not affiliated with, endorsed by, or supported by Advantage Air. Product
> names are used only to describe compatibility. Use at your own risk: see
> [Safety](#safety).

## Status

**Milestone 3: port-2025 HTTP API.** Not yet tested against a real control
box.

| Piece | State |
|---|---|
| CRC-8 | Done. Reproduces every checksum in a real bus capture |
| Frame encode/decode | Done. Streaming; resyncs on noise and truncated frames |
| Register records (25 hex chars) | Done |
| Typed registers 01, 03, 05, 06, 07, 08, 0a | Done, with range checks |
| Session state machine | Done (pure logic, no I/O): negotiate, two-phase flush, `ackCAN 1`/`0`, resend until `getCAN 1` (max 3), max 25 records per `setCAN`, firmware ack, quiet while `CAN2 in use` |
| `ductbridge-monitor` | Done. Listen-only; never transmits |
| `ductbridge` bus daemon | Done. Listen-only by default; `--active` replaces the tablet. Reconnects to the serial port, tracks units online/offline, logs every frame at debug level. See [packaging/INSTALL.md](packaging/INSTALL.md) |
| Local HTTP API on port 2025 | Done. `getSystemData` and `setAircon` (aircon on/off, mode, fan, setpoint, MyZone, fresh air; zone open/close, damper %, setpoint). Tested with the `advantage_air` Python client used by Home Assistant; discovery fields for the official phone app |
| Tablet-side features | Done, persisted in a state file: renaming system/aircon/zones, on/off timers, climate control (automatic MyZone), MyAuto (automatic heat/cool), quiet night mode. Some details unverified: see [INSTALL.md](packaging/INSTALL.md#8-verify-on-install-day). Not offered: auto-fan, lights, things |

## Safety

- The bus cable also carries ~14 V DC. Check pins with a multimeter before
  connecting anything, and never connect pin 5 to the adapter.
- Setpoint writes are refused outside 16–32 °C.
- Inbound records are range-checked; invalid ones are flagged, not applied.
- Keep the original tablet: plugging it back in restores normal operation.

## Wiring

From the aa_interop spec. Verify on your own system first.

| RJ45 pin | Signal |
|---|---|
| 1 | RS-485 B (+) |
| 2 | RS-485 A (−) |
| 4, 6 | GND |
| 5 | ~14 V DC — do not connect |

Serial settings: 57600 baud, 8N1, half-duplex.

## Use

```
cargo test
cargo run --bin ductbridge-monitor -- /dev/ttyUSB0   # live bus, listen-only
cargo run --bin ductbridge-monitor -- capture.bin    # replay a raw capture
cargo run --bin ductbridge -- --help                 # bus daemon
```

Installing on a Pi as a systemd service: [packaging/INSTALL.md](packaging/INSTALL.md).

CI runs `cargo fmt`, `clippy` and tests, then cross-compiles for 64-bit
Raspberry Pi OS (`aarch64-unknown-linux-gnu`). Push a `v*` tag to attach the
binaries to a GitHub release.

## Acknowledgements

- [gundy/aa_interop](https://github.com/gundy/aa_interop): protocol documentation (GFDL)
- [retsimx/cb-daemon](https://github.com/retsimx/cb-daemon): Rust reference implementation (MIT)
- [iduno/MyPlace](https://github.com/iduno/MyPlace): Java implementation
- The August 2024 Hacker News discussion that first published bus captures

## License

MIT
