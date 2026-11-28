# Installing ductbridge on a Raspberry Pi

Target: Raspberry Pi 3 (or later), 64-bit Raspberry Pi OS Lite, wired
Ethernet, USB-RS485 adapter (FTDI or CH340).

> Nothing here has been tested against a real control box yet. Keep the
> original tablet so you can plug it back in.

> The phone app will only work on your home network after the swap, and push alerts stop. Both need Advantage Air's cloud, which only the original tablet is signed in to. To use the app from outside, make the Pi a VPN server: see [9. Use the app away from home](#9-use-the-app-away-from-home).

## 0. Try it without the hardware (optional)

`--simulate` runs ductbridge against a simulated control box (four zones,
two with temperature sensors) instead of the serial port. Use it to try
the phone app and Home Assistant before install day. It never touches the
bus, so the adapter doesn't need to be plugged in.

On the Pi (or any 64-bit Linux machine on your network), with the service
stopped so port 2025 is free:

```bash
sudo systemctl stop ductbridge
```

```bash
ductbridge --simulate --state-file /tmp/ductbridge-sim.json --log-level debug
```

Then open the MyPlace app on a phone on the same network: it should find a
system called "ductbridge". Changes you make (on/off, mode, fan, zones,
names, timers, MyAuto, climate control) go to the simulated box and come
back as they would from a real one; zone temperatures drift towards their
setpoints while the unit runs. Stop with Ctrl-C. The `--state-file` keeps
the simulation's names and settings out of the real state file.

If the app does not find the system or something misbehaves, send the
terminal output (debug level logs every HTTP request).

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

Supported tablet-side features (run by ductbridge, settings kept in
`/var/lib/ductbridge/state.json`): renaming the system, aircon and zones from
the app; on/off timers; climate control (automatic MyZone); MyAuto; quiet
night mode (22:00–06:00 by the Pi's clock: check `timedatectl` shows NTP on
and the right time zone). The Pi has no battery-backed clock, so quiet night
only starts once NTP has synchronised the clock; until then the log says
"clock not synchronised". Everything else works without internet access. Not supported: auto-fan, lights and "things".

## 8. Verify on install day

These behaviours come from analysis of the app and the original tablet and
have not been confirmed on a real system. Each logs a line containing
"unverified" when it acts; check the journal and compare with the original
tablet where you can:

- Setting one on/off timer leaves the other one running.
- `setMySystem` is how the app renames the system.
- MyAuto, zones both too hot and too cold: it stays in its current mode.
- Quiet night starts when the unit is on and cooling inside the window.
- Quiet night end restores the settings and leaves the unit on/off as it is
  (the original may turn the unit off).
- Filter reset: capture what the original tablet sends when you reset the
  filter, so it can be implemented (currently accepted and ignored).
- With the VPN on and Wi-Fi off, the app still finds and controls the system.

## 9. Use the app away from home

The Pi can be the way in from outside as well. With a VPN running on the Pi, a phone away from home reaches the Pi's port 2025 just as it does on your Wi-Fi. Air-conditioning control never depends on the VPN or the internet: if either is down, the system keeps working at home.

Tailscale is the simplest: it needs no port forwarding on your router. On the Pi, with your home network's range in place of `192.168.1.0/24`:

```bash
curl -fsSL https://tailscale.com/install.sh | sh
echo 'net.ipv4.ip_forward = 1' | sudo tee /etc/sysctl.d/99-tailscale.conf
sudo sysctl -p /etc/sysctl.d/99-tailscale.conf
sudo tailscale up --advertise-routes=192.168.1.0/24
```

Approve the route in the Tailscale admin console, and turn off key expiry for the Pi on its Machines page, or remote access stops without warning when the key expires. Then install Tailscale on each phone and sign in to the same account. The phone can now reach the Pi at its home address.

WireGuard on the Pi works too, but needs a UDP port forwarded on your router and a fixed public address or dynamic DNS.

Not yet known: whether the app finds the system over a VPN by itself, or only once it has seen it on your Wi-Fi. Open the app on your Wi-Fi first, then try it on mobile data with the VPN on.

## Troubleshooting

- `no Ping from the control box yet`: wiring, A/B swapped, or wrong port.
- `Permission denied` opening the port: the service user needs the
  `dialout` group (the unit file adds it).
- `debug` log level prints every frame received and sent, and every HTTP
  request.
- `cannot listen on 0.0.0.0:2025`: another process has the port, or the
  address is wrong. The daemon exits and systemd restarts it.
