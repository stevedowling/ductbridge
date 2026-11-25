//! Bus daemon: drives the RS-485 bus in place of the wall tablet.
//!
//! Listen-only unless configured with `mode = "active"` or `--active`.
//! See `ductbridge --help` and `packaging/INSTALL.md`.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ductbridge::bridge::{run, Bridge, Handle, SubmitError, TxMode};
use ductbridge::config::{self, Action, Config};
use ductbridge::{features, http, log, log_debug, log_error, log_info, log_warn, state};

/// Wait between attempts to (re)open the serial port.
const REOPEN_DELAY: Duration = Duration::from_secs(5);
/// Serial read timeout; bounds how often housekeeping runs while idle.
const READ_TIMEOUT: Duration = Duration::from_millis(20);
/// Feature evaluation interval (also run whenever the tablet-side state
/// changes).
const EVALUATE_EVERY: Duration = Duration::from_secs(30);

/// Is the system clock synchronised (by NTP, e.g. systemd-timesyncd)?
/// A Pi has no battery-backed clock, so after boot it can be wrong until
/// NTP has synced.
fn clock_synced() -> bool {
    // SAFETY: adjtimex with modes 0 only reads the kernel's clock state.
    unsafe {
        let mut tx: libc::timex = std::mem::zeroed();
        libc::adjtimex(&mut tx) != libc::TIME_ERROR
    }
}

/// Local time of day in minutes after midnight (the Pi's clock and time
/// zone), or `None` if the clock is not synchronised.
fn local_minute() -> Option<u16> {
    if !clock_synced() {
        return None;
    }
    // SAFETY: time() and localtime_r() with valid pointers; tm is plain data.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some((tm.tm_hour * 60 + tm.tm_min) as u16)
    }
}

fn report(what: &str, r: Result<(), SubmitError>) {
    match r {
        Ok(()) => {}
        // Expected while listen-only or starting up.
        Err(SubmitError::Write(e)) => log_debug!("{what}: {e}"),
        Err(e) => log_warn!("{what}: {e}"),
    }
}

/// Runs the tablet-side features and timers, and saves the tablet-side
/// state whenever it changes.
fn controller(handle: Handle, state_file: Option<PathBuf>) {
    let (mut saved, _) = handle.app();
    let mut evaluated = u64::MAX;
    let mut last_eval = Instant::now();
    let mut last_second = Instant::now();
    let mut timers = features::TimerClock::default();
    let mut clock_warned = false;
    let mut save_failing = false;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = Instant::now();
        let secs = now.duration_since(last_second).as_secs() as u32;
        if secs > 0 {
            last_second += Duration::from_secs(secs as u64);
            let due = timers.advance(&handle.app().1, secs);
            if due.any() {
                let r = handle.submit(now, |st| Ok(features::tick_timers(st, now, due)));
                // Applied, or a no-op because the bus is not ready (pause):
                // either way that minute is used. Refused (listen-only or
                // starting up): it stays due and is retried next second.
                if r.is_ok() {
                    timers.ticked(due);
                }
                report("timers", r);
            }
        }
        let (version, _) = handle.app();
        if version != evaluated || now.duration_since(last_eval) >= EVALUATE_EVERY {
            last_eval = now;
            let minute = local_minute();
            let quiet = handle.app().1.quiet_night_enabled;
            if minute.is_none() && quiet && !clock_warned {
                log_warn!(
                    "clock not synchronised (check NTP: timedatectl); quiet night will not start until it is"
                );
                clock_warned = true;
            } else if minute.is_some() && clock_warned {
                log_info!("clock synchronised");
                clock_warned = false;
            }
            report(
                "features",
                handle.submit(now, |st| Ok(features::evaluate(st, now, minute))),
            );
            evaluated = handle.app().0;
        }
        let (version, app) = handle.app();
        if version != saved {
            match &state_file {
                Some(path) => match state::save(path, &app) {
                    Ok(()) => {
                        log_debug!("saved {}", path.display());
                        if save_failing {
                            log_info!("saved {} again", path.display());
                        }
                        save_failing = false;
                        saved = version;
                    }
                    // Not marked saved: retried every second until it works.
                    Err(e) => {
                        if !save_failing {
                            log_error!("cannot save {}: {e} (retrying)", path.display());
                        }
                        save_failing = true;
                    }
                },
                None => saved = version,
            }
        }
    }
}

fn open(cfg: &Config) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    let port = serialport::new(&cfg.port, 57_600)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(READ_TIMEOUT)
        .open()?;
    // Stale bytes from before we opened would only confuse the first slot.
    let _ = port.clear(serialport::ClearBuffer::All);
    Ok(port)
}

fn main() -> ExitCode {
    let cfg = match config::from_args(std::env::args().skip(1), |p| std::fs::read_to_string(p)) {
        Ok(Action::Run(c)) => c,
        Ok(Action::Help) => {
            print!("{}", config::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Action::Version) => {
            println!("ductbridge {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("ductbridge: {e}");
            return ExitCode::from(2);
        }
    };
    log::init(cfg.log_level);
    log_info!(
        "ductbridge {} starting: port {}, mode {:?}",
        env!("CARGO_PKG_VERSION"),
        cfg.port,
        cfg.mode
    );
    match cfg.mode {
        TxMode::ListenOnly => log_info!("listen-only: will never transmit"),
        TxMode::Active => log_warn!(
            "active mode: answering control-box Pings; the original tablet must be unplugged"
        ),
    }
    let (mut bridge, handle) = Bridge::new(cfg.mode, Instant::now());
    let state_file = (cfg.state_file != "off").then(|| PathBuf::from(&cfg.state_file));
    match &state_file {
        Some(path) => {
            let (app, warnings) = state::load(path);
            for w in warnings {
                log_warn!("{w}");
            }
            log_info!("tablet-side state: {}", path.display());
            handle.set_app(app);
        }
        None => log_info!("tablet-side state not persisted (state_file = off)"),
    }
    {
        let handle = handle.clone();
        std::thread::spawn(move || controller(handle, state_file));
    }
    if cfg.http_bind == "off" {
        log_info!("HTTP API disabled");
    } else {
        let listener = match TcpListener::bind(&cfg.http_bind) {
            Ok(l) => l,
            Err(e) => {
                log_error!("cannot listen on {}: {e}", cfg.http_bind);
                return ExitCode::FAILURE;
            }
        };
        log_info!("HTTP API listening on {}", cfg.http_bind);
        let info = cfg.api.clone();
        std::thread::spawn(move || http::serve(listener, handle, info));
    }
    let mut first = true;
    loop {
        match open(&cfg) {
            Ok(mut port) => {
                if !first {
                    bridge.reset(Instant::now());
                }
                first = false;
                log_info!("opened {} at 57600 8N1", cfg.port);
                match run(&mut port, &mut bridge) {
                    Ok(()) => log_warn!("{}: port closed", cfg.port),
                    Err(e) => log_error!("{}: {e}", cfg.port),
                }
            }
            Err(e) => log_error!("cannot open {}: {e}", cfg.port),
        }
        let until = Instant::now() + REOPEN_DELAY;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(500));
            bridge.tick(Instant::now());
        }
    }
}
