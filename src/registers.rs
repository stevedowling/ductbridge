//! Typed views of the registers we need for aircon control.
//!
//! Field layouts follow the aa_interop spec (`04-registers.md`) register
//! tables. A few of the spec's worked examples disagree with its tables;
//! the tables win.

use crate::record::{Record, DEST_CB};

/// Setpoint limits for writes (16–32 °C, the usual thermostat range).
pub const MIN_SET_TEMP: f32 = 16.0;
pub const MAX_SET_TEMP: f32 = 32.0;
pub const MAX_ZONES: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Cool = 1,
    Heat = 2,
    Vent = 3,
    Auto = 4,
    Dry = 5,
}

impl Mode {
    /// Decode as the original tablet does: 1 cool, 2 heat, 3 vent, 5 dry,
    /// anything else auto.
    pub fn from_wire(v: u8) -> Mode {
        match v {
            1 => Mode::Cool,
            2 => Mode::Heat,
            3 => Mode::Vent,
            5 => Mode::Dry,
            _ => Mode::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fan {
    Off = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    Auto = 4,
    AutoAA = 5,
}

impl Fan {
    pub fn from_wire(v: u8) -> Option<Fan> {
        Some(match v {
            0 => Fan::Off,
            1 => Fan::Low,
            2 => Fan::Medium,
            3 => Fan::High,
            4 => Fan::Auto,
            5 => Fan::AutoAA,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegError {
    WrongRegister { expected: u8, got: u8 },
    OutOfRange(&'static str),
}

fn check(r: &Record, reg: u8) -> Result<(), RegError> {
    if r.register == reg {
        Ok(())
    } else {
        Err(RegError::WrongRegister {
            expected: reg,
            got: r.register,
        })
    }
}

fn temp_to_wire(t: f32) -> Result<u8, RegError> {
    if !(MIN_SET_TEMP..=MAX_SET_TEMP).contains(&t) {
        return Err(RegError::OutOfRange("set temperature"));
    }
    Ok((t * 2.0).round() as u8)
}

/// Register 05: system status (JZ5 write / JZ15 read).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemStatus {
    pub on: bool,
    pub mode: Mode,
    pub fan: Fan,
    pub set_temp: f32,
    /// 0 = MyZone disabled.
    pub my_zone: u8,
    pub fresh_air: u8,
    pub rf_sys_id: u8,
}

impl SystemStatus {
    pub const REG: u8 = 0x05;

    /// Validated against the spec's ranges.
    pub fn decode(r: &Record) -> Result<Self, RegError> {
        check(r, Self::REG)?;
        let p = r.payload;
        if p[0] > 1 {
            return Err(RegError::OutOfRange("system on"));
        }
        if p[1] > 5 {
            return Err(RegError::OutOfRange("mode"));
        }
        let fan = Fan::from_wire(p[2]).ok_or(RegError::OutOfRange("fan"))?;
        if p[3] > 80 {
            return Err(RegError::OutOfRange("set temperature"));
        }
        if p[4] > MAX_ZONES {
            return Err(RegError::OutOfRange("my zone"));
        }
        if p[5] > 2 {
            return Err(RegError::OutOfRange("fresh air"));
        }
        if p[6] > 16 {
            return Err(RegError::OutOfRange("rf system id"));
        }
        Ok(SystemStatus {
            on: p[0] == 1,
            mode: Mode::from_wire(p[1]),
            fan,
            set_temp: p[3] as f32 / 2.0,
            my_zone: p[4],
            fresh_air: p[5],
            rf_sys_id: p[6],
        })
    }

    /// Build a write record to the control box. Fails on an out-of-range setpoint.
    pub fn encode_write(&self, unit_type: u8, uid: u32) -> Result<Record, RegError> {
        if self.my_zone > MAX_ZONES {
            return Err(RegError::OutOfRange("my zone"));
        }
        Ok(Record {
            unit_type,
            dest: DEST_CB,
            uid,
            register: Self::REG,
            payload: [
                self.on as u8,
                self.mode as u8,
                self.fan as u8,
                temp_to_wire(self.set_temp)?,
                self.my_zone,
                self.fresh_air,
                self.rf_sys_id,
            ],
        })
    }
}

/// Register 03: zone state (JZ11).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneState {
    pub zone: u8,
    pub open: bool,
    /// Damper opening, 0–100 %.
    pub percent: u8,
    pub sensor_type: u8,
    pub set_temp: f32,
    /// None until a sensor is attached and reporting.
    pub measured_temp: Option<f32>,
}

impl ZoneState {
    pub const REG: u8 = 0x03;

    pub fn decode(r: &Record) -> Result<Self, RegError> {
        check(r, Self::REG)?;
        let p = r.payload;
        if !(1..=MAX_ZONES).contains(&p[0]) {
            return Err(RegError::OutOfRange("zone number"));
        }
        let percent = p[1] & 0x7F;
        if percent > 100 {
            return Err(RegError::OutOfRange("damper percent"));
        }
        if p[2] > 4 {
            return Err(RegError::OutOfRange("sensor type"));
        }
        if p[3] > 80 {
            return Err(RegError::OutOfRange("set temperature"));
        }
        if p[5] > 9 {
            return Err(RegError::OutOfRange("measured decimal"));
        }
        let measured_temp = if p[2] == 0 && p[4] == 0 && p[5] == 0 {
            None
        } else {
            Some(p[4] as f32 + p[5] as f32 / 10.0)
        };
        Ok(ZoneState {
            zone: p[0],
            open: p[1] & 0x80 != 0,
            percent,
            sensor_type: p[2],
            set_temp: p[3] as f32 / 2.0,
            measured_temp,
        })
    }

    /// Build a zone write. Sensor type and measured temperature are sent as
    /// zero; the control box fills them in.
    pub fn encode_write(&self, unit_type: u8, uid: u32) -> Result<Record, RegError> {
        if !(1..=MAX_ZONES).contains(&self.zone) {
            return Err(RegError::OutOfRange("zone number"));
        }
        if self.percent > 100 {
            return Err(RegError::OutOfRange("damper percent"));
        }
        Ok(Record {
            unit_type,
            dest: DEST_CB,
            uid,
            register: Self::REG,
            payload: [
                self.zone,
                (self.open as u8) << 7 | self.percent,
                0,
                temp_to_wire(self.set_temp)?,
                0,
                0,
                0,
            ],
        })
    }
}

/// Register 01: zone configuration (JZ7 read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneInfo {
    pub zones: u8,
    pub constant_zones: [u8; 3],
    pub filter_clean: bool,
}

impl ZoneInfo {
    pub const REG: u8 = 0x01;

    pub fn decode(r: &Record) -> Result<Self, RegError> {
        check(r, Self::REG)?;
        let p = r.payload;
        if p[1] > MAX_ZONES {
            return Err(RegError::OutOfRange("number of zones"));
        }
        Ok(ZoneInfo {
            zones: p[1],
            constant_zones: [p[3], p[4], p[5]],
            filter_clean: p[6] == 1,
        })
    }
}

/// Register 06 from the box: firmware announcement (JZ17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Firmware {
    pub major: u8,
    pub minor: u8,
    pub cb_type: u8,
    pub rf_major: u8,
}

impl Firmware {
    pub const REG: u8 = 0x06;

    pub fn decode(r: &Record) -> Result<Self, RegError> {
        check(r, Self::REG)?;
        let p = r.payload;
        Ok(Firmware {
            major: p[0],
            minor: p[1],
            cb_type: p[2],
            rf_major: p[3],
        })
    }
}

/// Register 08: aircon error code (JZ22), up to 5 ASCII characters.
pub fn decode_error(r: &Record) -> Result<String, RegError> {
    check(r, 0x08)?;
    Ok(r.payload[..5]
        .iter()
        .filter(|b| b.is_ascii_graphic())
        .map(|&b| b as char)
        .collect())
}

/// Register 06 write: request a register flush. `uid` 0 = all units.
pub fn flush(unit_type: u8, uid: u32) -> Record {
    Record {
        unit_type,
        dest: DEST_CB,
        uid,
        register: 0x06,
        payload: [0; 7],
    }
}

/// Register 07 write: mandatory reply to a firmware announcement. Echoes the
/// announcing unit's type and id.
pub fn firmware_ack(announcement: &Record) -> Record {
    Record {
        unit_type: announcement.unit_type,
        dest: DEST_CB,
        uid: announcement.uid,
        register: 0x07,
        payload: [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::UNIT_AIRCON;

    fn rec(s: &str) -> Record {
        Record::parse(s).unwrap()
    }

    #[test]
    fn system_status_decode() {
        let s = SystemStatus::decode(&rec("0703abcde0501010330000100")).unwrap();
        assert!(s.on);
        assert_eq!(s.mode, Mode::Cool);
        assert_eq!(s.fan, Fan::High);
        assert_eq!(s.set_temp, 24.0);
        assert_eq!(s.my_zone, 0);
    }

    #[test]
    fn system_status_rejects_out_of_range() {
        assert!(SystemStatus::decode(&rec("0703abcde0502010330000100")).is_err()); // on=2
        assert!(SystemStatus::decode(&rec("0703abcde0501010351000100")).is_err()); // 0x51 = 40.5 °C
        assert!(SystemStatus::decode(&rec("0703abcde0501010330000111")).is_err());
        // rf id 17
    }

    #[test]
    fn system_status_write_round_trip() {
        let s = SystemStatus {
            on: true,
            mode: Mode::Heat,
            fan: Fan::Auto,
            set_temp: 22.5,
            my_zone: 3,
            fresh_air: 0,
            rf_sys_id: 0,
        };
        let r = s.encode_write(UNIT_AIRCON, 0xabcde).unwrap();
        assert_eq!(
            r.to_string(),
            "0701abcde05010204 2d03 0000".replace(' ', "")
        );
        assert_eq!(SystemStatus::decode(&r).unwrap(), s);
    }

    #[test]
    fn setpoint_limits() {
        let mut s = SystemStatus::decode(&rec("0703abcde0501010330000100")).unwrap();
        s.set_temp = 35.0;
        assert_eq!(
            s.encode_write(UNIT_AIRCON, 1),
            Err(RegError::OutOfRange("set temperature"))
        );
        s.set_temp = 15.5;
        assert!(s.encode_write(UNIT_AIRCON, 1).is_err());
    }

    #[test]
    fn zone_state() {
        let z = ZoneState::decode(&rec("0703abcde0301e40030000000")).unwrap();
        assert_eq!(
            (z.zone, z.open, z.percent, z.set_temp, z.measured_temp),
            (1, true, 100, 24.0, None)
        );

        let z = ZoneState::decode(&rec("0703abcde0302640130140500")).unwrap();
        assert_eq!((z.zone, z.open, z.percent), (2, false, 100));
        assert_eq!(z.measured_temp, Some(20.5));

        let w = ZoneState {
            zone: 1,
            open: true,
            percent: 100,
            sensor_type: 1,
            set_temp: 24.0,
            measured_temp: Some(20.5),
        };
        assert_eq!(
            w.encode_write(UNIT_AIRCON, 0xabcde).unwrap().to_string(),
            "0701abcde0301e40030000000"
        );
    }

    #[test]
    fn zone_state_rejects() {
        assert!(ZoneState::decode(&rec("0703abcde030be40030000000")).is_err()); // zone 11
        assert!(ZoneState::decode(&rec("0703abcde0301e60030000000")).is_err()); // 0x60 = 96? no: 0xe6&0x7f=102
    }

    #[test]
    fn firmware_ack_echoes_unit() {
        let ann = rec("0803abcde0602030100000000");
        assert_eq!(firmware_ack(&ann).to_string(), "0801abcde0700000000000000");
        assert_eq!(
            flush(UNIT_AIRCON, 0).to_string(),
            "0701000000600000000000000"
        );
    }

    #[test]
    fn error_code() {
        assert_eq!(
            decode_error(&rec("0703abcde0841413400000000")).unwrap(),
            "AA4"
        );
    }
}
