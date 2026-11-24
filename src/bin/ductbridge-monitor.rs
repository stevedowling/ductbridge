//! Listen-only bus monitor. Never transmits.
//!
//! Usage:
//!   ductbridge-monitor [/dev/ttyUSB0]      read the RS-485 bus (57600 8N1)
//!   ductbridge-monitor capture.bin         decode a saved raw capture file

use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ductbridge::frame::{Decoder, Frame};
use ductbridge::record::parse_list;

fn print(f: &Frame) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let flag = if f.crc_ok { "   " } else { "BAD" };
    println!(
        "{}.{:03} {flag} {}",
        t.as_secs(),
        t.subsec_millis(),
        f.payload
    );
    let body = f
        .payload
        .strip_prefix("getCAN ")
        .or_else(|| f.payload.strip_prefix("setCAN "));
    if let Some(body) = body {
        for r in parse_list(body) {
            println!("        {}", ductbridge::describe(&r));
        }
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/ttyUSB0".into());
    let mut source: Box<dyn Read> = if std::fs::metadata(&path)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        Box::new(std::fs::File::open(&path).expect("open capture file"))
    } else {
        let port = serialport::new(&path, 57_600)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(500))
            .open()
            .unwrap_or_else(|e| panic!("open {path}: {e}"));
        eprintln!("Listening on {path} at 57600 8N1 (listen-only)");
        Box::new(port)
    };
    let mut dec = Decoder::new();
    let mut buf = [0u8; 512];
    loop {
        match source.read(&mut buf) {
            Ok(0) => break, // end of capture file
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Some(f) = dec.next_frame() {
                    print(&f);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }
    }
}
