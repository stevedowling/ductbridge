//! CAN2 register records: 25 hex characters, `TT DD UUUUU RR PPPPPPPPPPPPPP`.
//!
//! Layout per the aa_interop spec §2.4, consistent with observed traffic.

use std::fmt;

/// Unit type: lights.
pub const UNIT_LIGHTS: u8 = 0x02;
/// Unit type: air conditioning, wired.
pub const UNIT_AIRCON: u8 = 0x07;
/// Unit type: air conditioning via RF (split systems) and RF devices.
pub const UNIT_AIRCON_RF: u8 = 0x08;

/// Destination: control box.
pub const DEST_CB: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Record {
    pub unit_type: u8,
    pub dest: u8,
    /// 20-bit unit id (5 hex digits).
    pub uid: u32,
    pub register: u8,
    pub payload: [u8; 7],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Length(usize),
    NotHex,
}

fn hex_byte(s: &str, at: usize) -> Result<u8, ParseError> {
    u8::from_str_radix(&s[at..at + 2], 16).map_err(|_| ParseError::NotHex)
}

impl Record {
    pub fn parse(s: &str) -> Result<Record, ParseError> {
        if s.len() != 25 {
            return Err(ParseError::Length(s.len()));
        }
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseError::NotHex);
        }
        let mut payload = [0u8; 7];
        for (i, p) in payload.iter_mut().enumerate() {
            *p = hex_byte(s, 11 + i * 2)?;
        }
        Ok(Record {
            unit_type: hex_byte(s, 0)?,
            dest: hex_byte(s, 2)?,
            uid: u32::from_str_radix(&s[4..9], 16).map_err(|_| ParseError::NotHex)?,
            register: hex_byte(s, 9)?,
            payload,
        })
    }

    /// Is this an air-conditioning record (wired or RF)?
    pub fn is_aircon(&self) -> bool {
        self.unit_type == UNIT_AIRCON || self.unit_type == UNIT_AIRCON_RF
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}{:02x}{:05x}{:02x}",
            self.unit_type,
            self.dest,
            self.uid & 0xFFFFF,
            self.register
        )?;
        for b in self.payload {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Split a `getCAN`/`setCAN` body into records, skipping anything that is not
/// exactly 25 characters (as the original tablet does).
pub fn parse_list(body: &str) -> Vec<Record> {
    body.split(' ')
        .filter(|t| t.len() == 25)
        .filter_map(|t| Record::parse(t).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for s in [
            "0701000000600000000000000",
            "0703abcde0501010330000100",
            "0201000000236000000000000",
            "0703abcde0301e40030000000",
        ] {
            assert_eq!(Record::parse(s).unwrap().to_string(), s);
        }
    }

    #[test]
    fn fields() {
        let r = Record::parse("0703abcde0501010330000100").unwrap();
        assert_eq!(r.unit_type, UNIT_AIRCON);
        assert_eq!(r.dest, 0x03);
        assert_eq!(r.uid, 0xabcde);
        assert_eq!(r.register, 0x05);
        assert_eq!(r.payload, [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Record::parse("0703"), Err(ParseError::Length(4)));
        assert_eq!(
            Record::parse("0703abcdeZZ01010330000100"),
            Err(ParseError::NotHex)
        );
    }

    #[test]
    fn list_skips_junk() {
        let v = parse_list("1 0703abcde0a00000000000000 junk 0703abcde0501010330000100 ");
        assert_eq!(v.len(), 2);
    }
}
