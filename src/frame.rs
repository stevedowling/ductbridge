//! `<U>payload</U=xx>` framing.
//!
//! `xx` is the CRC-8 of the payload as two lowercase hex digits.

use crate::crc::crc8;

const START: &[u8] = b"<U>";
const END: &[u8] = b"</U=";
/// Drop buffered bytes beyond this to recover from line noise.
const MAX_BUFFER: usize = 16 * 1024;

/// Wrap a payload in a frame, ready to write to the bus.
pub fn encode(payload: &str) -> Vec<u8> {
    format!("<U>{payload}</U={:02x}>", crc8(payload.as_bytes())).into_bytes()
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub payload: String,
    /// The checksum in the frame matched the payload.
    pub crc_ok: bool,
}

/// Counts of input the decoder had to throw away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecoderStats {
    /// Bytes outside any frame (line noise, separators, buffer overflow).
    pub noise_bytes: u64,
    /// Frames cut short by a new start tag before their end tag.
    pub truncated: u64,
    /// Frames whose `xx>` trailer was not two hex digits and `>`.
    pub malformed: u64,
}

/// Streaming decoder: push raw bytes in, pull complete frames out.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    stats: DecoderStats,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Running totals of discarded input.
    pub fn stats(&self) -> DecoderStats {
        self.stats
    }

    /// Add bytes read from the serial port.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > MAX_BUFFER {
            let cut = self.buf.len() - MAX_BUFFER;
            self.buf.drain(..cut);
            self.stats.noise_bytes += cut as u64;
        }
    }

    /// Return the next complete frame, if one is buffered.
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let start = match find(&self.buf, START, 0) {
                Some(s) => s,
                None => {
                    // Keep a possible partial "<U" at the end.
                    let keep = self.buf.len().min(START.len() - 1);
                    let cut = self.buf.len() - keep;
                    self.buf.drain(..cut);
                    self.stats.noise_bytes += cut as u64;
                    return None;
                }
            };
            if start > 0 {
                self.buf.drain(..start);
                self.stats.noise_bytes += start as u64;
            }
            let body = START.len();
            let end = find(&self.buf, END, body)?;
            // A new start tag before the end tag means the earlier frame was
            // truncated: resynchronise on the newer one.
            if let Some(next) = find(&self.buf, START, body) {
                if next < end {
                    self.buf.drain(..next);
                    self.stats.truncated += 1;
                    continue;
                }
            }
            let tail = end + END.len();
            if self.buf.len() < tail + 3 {
                return None; // need "xx>"
            }
            let crc = match (hex_val(self.buf[tail]), hex_val(self.buf[tail + 1])) {
                (Some(h), Some(l)) if self.buf[tail + 2] == b'>' => Some(h << 4 | l),
                _ => None,
            };
            let payload_bytes = self.buf[body..end].to_vec();
            self.buf.drain(..tail + 3);
            let Some(crc) = crc else {
                self.stats.malformed += 1;
                continue;
            };
            return Some(Frame {
                crc_ok: crc8(&payload_bytes) == crc,
                payload: String::from_utf8_lossy(&payload_bytes).into_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_ping_exactly() {
        assert_eq!(encode("Ping"), b"<U>Ping</U=db>");
        assert_eq!(encode("ackCAN 1"), b"<U>ackCAN 1</U=aa>");
    }

    #[test]
    fn decodes_hacker_news_capture() {
        let capture = "<U>setCAN 0201000000236000000000000 </U=ce><U>getCAN 1 </U=00>\
                       <U>Ping</U=db> <U>ackCAN 1</U=aa><U>Ping</U=db> <U>setCAN </U=b2>";
        let mut d = Decoder::new();
        d.push(capture.as_bytes());
        let frames: Vec<Frame> = std::iter::from_fn(|| d.next_frame()).collect();
        let payloads: Vec<&str> = frames.iter().map(|f| f.payload.as_str()).collect();
        assert_eq!(
            payloads,
            [
                "setCAN 0201000000236000000000000 ",
                "getCAN 1 ",
                "Ping",
                "ackCAN 1",
                "Ping",
                "setCAN "
            ]
        );
        // Every checksum in the real capture verifies.
        assert!(frames.iter().all(|f| f.crc_ok));
    }

    #[test]
    fn handles_byte_by_byte_arrival_and_noise() {
        let mut d = Decoder::new();
        let stream = b"\x00garbage<U>Ping</U=db>noise<U>Ping</U=db>";
        let mut got = vec![];
        for b in stream {
            d.push(&[*b]);
            while let Some(f) = d.next_frame() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|f| f.payload == "Ping" && f.crc_ok));
    }

    #[test]
    fn flags_bad_checksum() {
        let mut d = Decoder::new();
        d.push(b"<U>Ping</U=00>");
        assert_eq!(
            d.next_frame(),
            Some(Frame {
                payload: "Ping".into(),
                crc_ok: false
            })
        );
    }

    #[test]
    fn resyncs_after_truncated_frame() {
        let mut d = Decoder::new();
        d.push(b"<U>getCAN 1 0703<U>Ping</U=db>");
        assert_eq!(d.next_frame().unwrap().payload, "Ping");
        assert_eq!(d.next_frame(), None);
        assert_eq!(d.stats().truncated, 1);
    }

    #[test]
    fn counts_noise_and_malformed_trailers() {
        let mut d = Decoder::new();
        d.push(b"xyz<U>Ping</U=zz><U>Ping</U=db>");
        assert_eq!(d.next_frame().unwrap().payload, "Ping");
        let st = d.stats();
        assert_eq!((st.noise_bytes, st.malformed), (3, 1));
    }
}
