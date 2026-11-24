//! Bus daemon: drives the RS-485 bus in place of the wall tablet.
//!
//! Listen-only unless configured with `mode = "active"` or `--active`.
//! See `ductbridge --help` and `packaging/INSTALL.md`.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use ductbridge::bridge::{run, Bridge, TxMode};
use ductbridge::config::{self, Action, Config};
use ductbridge::{log, log_error, log_info, log_warn};

/// Wait between attempts to (re)open the serial port.
const REOPEN_DELAY: Duration = Duration::from_secs(5);
/// Serial read timeout; bounds how often housekeeping runs while idle.
const READ_TIMEOUT: Duration = Duration::from_millis(20);

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
    log_info!(
        "HTTP API ({}) is not implemented yet (milestone 3)",
        cfg.http_bind
    );

    // The handle is for the HTTP API (milestone 3).
    let (mut bridge, _handle) = Bridge::new(cfg.mode, Instant::now());
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
