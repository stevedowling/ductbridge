//! CRC-8 used in `<U>…</U=xx>` frames.
//!
//! Reflected CRC-8, polynomial 0xB2, init 0x00, final XOR 0xFF
//! (aa_interop spec §2.2). Verified against every checksum in a real bus
//! capture.

/// Compute the frame checksum of a payload.
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xB2
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_values_from_spec_and_captures() {
        assert_eq!(crc8(b"Ping"), 0xdb);
        assert_eq!(crc8(b"setCAN "), 0xb2);
        assert_eq!(crc8(b"ackCAN 1"), 0xaa);
        assert_eq!(crc8(b"getSystemData"), 0x15);
        // From the August 2024 Hacker News bus capture.
        assert_eq!(crc8(b"setCAN 0201000000236000000000000 "), 0xce);
    }
}
