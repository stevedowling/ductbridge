# Installing ductbridge on a Raspberry Pi

Target: Raspberry Pi 3 (or later), 64-bit Raspberry Pi OS Lite, wired
Ethernet, USB-RS485 adapter (FTDI or CH340).

> Nothing here has been tested against a real control box yet. Keep the
> original tablet so you can plug it back in.

## 1. Wiring

Check the pins with a multimeter first. The cable carries ~14 V DC on pin 5.

| RJ45 pin | Connect to adapter |
|---|---|
| 1 | B (+) |
| 2 | A (−) |
| 4 or 6 | GND |
| 5 | **nothing** |

## 2. Binaries

Download `ductbridge` and `ductbridge-monitor` (aarch64) from the GitHub
release or the CI run's artifact, then:

```bash
sudo install -m 755 ductbridge ductbridge-monitor /usr/local/bin/
```

## 3. Find the adapter

```bash
ls -l /dev/serial/by-id/
```

Use that `by-id` path in the config; `/dev/ttyUSB0` can move if another USB
serial device appears.

## 4. Watch the bus first (listen-only)

With the original tablet still connected:

```bash
sudo ductbridge-monitor /dev/serial/by-id/usb-XXXX
```

You should see `Ping` about once a second and the tablet's replies. No
frames, or only `BAD` frames, usually means A/B are swapped.

## 5. Install the service

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin ductbridge
```

```bash
sudo install -m 644 packaging/ductbridge.conf /etc/ductbridge.conf
```

```bash
sudo install -m 644 packaging/ductbridge.service /etc/systemd/system/
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now ductbridge
```

```bash
journalctl -u ductbridge -f
```

Edit `/etc/ductbridge.conf` to set `port`. The default mode is
`listen-only`: the daemon learns registers from the tablet's traffic and never
transmits.

## 6. Go active

1. Unplug the original tablet.
2. Set `mode = "active"` in `/etc/ductbridge.conf`.
3. `sudo systemctl restart ductbridge`

The log should show `session phase: Flushing`, `unit 07:xxxxx online` (or
`08:` on split systems), then `session phase: Steady`. A warning that
"another device is answering Pings" means the original tablet is still on the
bus: stop the service and unplug it.

To roll back: stop the service and plug the tablet back in.

## 7. HTTP API and Home Assistant

The daemon serves the tablet's local API on port 2025 (`http_bind`). Check
it from another machine on the LAN:

```bash
curl http://PI_ADDRESS:2025/getSystemData
```

It answers 503 until the bus has delivered the aircon status and every
zone's state (a few seconds after start). Set `system_name`, `aircon_name`
and `zone_names` in `/etc/ductbridge.conf`; the original tablet stored these
and they are not on the bus.

In Home Assistant, add the **Advantage Air** integration with the Pi's
address and port 2025. Give the Pi a fixed address (DHCP reservation) first.

In listen-only mode the API is read-only: `setAircon` answers
`{"ack": false, "reason": "ductbridge is in listen-only mode"}`.

Not supported yet: MyAuto, MyTemp, auto-fan and night-mode presets,
on/off timers, lights and "things". They are left out of `getSystemData`,
so Home Assistant does not offer them.

```bash
sudo systemctl stop ductbridge
```

## Troubleshooting

- `no Ping from the control box yet`: wiring, A/B swapped, or wrong port.
- `Permission denied` opening the port: the service user needs the
  `dialout` group (the unit file adds it).
- `debug` log level prints every frame received and sent, and every HTTP
  request.
- `cannot listen on 0.0.0.0:2025`: another process has the port, or the
  address is wrong. The daemon exits and systemd restarts it.
