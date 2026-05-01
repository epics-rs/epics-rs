use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// Multi-bit binary input direct record.
// VAL holds the full unsigned 16-bit value; B0-BF expose individual bits as Char (0/1).
// On process: RVAL is shifted right by SHFT, masked by MASK, stored in VAL and bit fields.
pub struct MbbiDirectRecord {
    pub val: u32,
    pub rval: i32,
    pub oraw: i32,
    pub mask: i32,
    pub shft: i16,
    pub nobt: i16,
    pub mlst: u32,
    pub bits: [u8; 16], // B0-BF
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    skip_convert: bool,
}

impl Default for MbbiDirectRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            shft: 0,
            nobt: 0,
            mlst: 0,
            bits: [0; 16],
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            skip_convert: false,
        }
    }
}

impl MbbiDirectRecord {
    fn val_to_bits(&mut self) {
        for i in 0..16 {
            self.bits[i] = ((self.val >> i) & 1) as u8;
        }
    }

    fn bits_to_val(&mut self) {
        self.val = 0;
        for i in 0..16 {
            self.val |= (self.bits[i] as u32 & 1) << i;
        }
    }
}

const BIT_NAMES: [&str; 16] = [
    "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7",
    "B8", "B9", "BA", "BB", "BC", "BD", "BE", "BF",
];

static MBBI_DIRECT_FIELDS: &[FieldDesc] = &[
    FieldDesc { name: "VAL",  dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "RVAL", dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "ORAW", dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "MASK", dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "SHFT", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "NOBT", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "MLST", dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "B0",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B1",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B2",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B3",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B4",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B5",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B6",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B7",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B8",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "B9",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "BA",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "BB",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "BC",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "BD",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "BE",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "BF",   dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "SIMM", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "SIML", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "SIOL", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "SIMS", dbf_type: DbFieldType::Short, read_only: false },
];

impl Record for MbbiDirectRecord {
    fn record_type(&self) -> &'static str {
        "mbbiDirect"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        MBBI_DIRECT_FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 16 {
                self.mask = ((1i64 << self.nobt) - 1) as i32;
            }
            self.mlst = self.val;
            self.oraw = self.rval;
            self.val_to_bits();
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            let mut raw = self.rval;
            if self.mask != 0 {
                raw &= self.mask;
            }
            if self.shft > 0 {
                raw = ((raw as u32) >> (self.shft as u32)) as i32;
            }
            self.val = raw as u32;
            self.val_to_bits();
        }
        self.skip_convert = false;
        self.oraw = self.rval;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL"  => Some(EpicsValue::Long(self.val as i32)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "MASK" => Some(EpicsValue::Long(self.mask)),
            "SHFT" => Some(EpicsValue::Short(self.shft)),
            "NOBT" => Some(EpicsValue::Short(self.nobt)),
            "MLST" => Some(EpicsValue::Long(self.mlst as i32)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            _ => {
                if let Some(idx) = BIT_NAMES.iter().position(|&n| n == name) {
                    Some(EpicsValue::Char(self.bits[idx]))
                } else {
                    None
                }
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                match value {
                    EpicsValue::Long(v)  => self.val = v as u32,
                    EpicsValue::Short(v) => self.val = v as u32,
                    EpicsValue::Char(v)  => self.val = v as u32,
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                }
                self.val_to_bits();
            }
            "RVAL" => {
                if let EpicsValue::Long(v) = value { self.rval = v; }
                else { return Err(CaError::TypeMismatch("RVAL".into())); }
            }
            "MASK" => {
                if let EpicsValue::Long(v) = value { self.mask = v; }
                else { return Err(CaError::TypeMismatch("MASK".into())); }
            }
            "SHFT" => {
                if let EpicsValue::Short(v) = value { self.shft = v; }
                else { return Err(CaError::TypeMismatch("SHFT".into())); }
            }
            "NOBT" => {
                if let EpicsValue::Short(v) = value { self.nobt = v; }
                else { return Err(CaError::TypeMismatch("NOBT".into())); }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value { self.simm = v; }
                else { return Err(CaError::TypeMismatch("SIMM".into())); }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value { self.siml = v; }
                else { return Err(CaError::TypeMismatch("SIML".into())); }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value { self.siol = v; }
                else { return Err(CaError::TypeMismatch("SIOL".into())); }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value { self.sims = v; }
                else { return Err(CaError::TypeMismatch("SIMS".into())); }
            }
            _ => {
                if let Some(idx) = BIT_NAMES.iter().position(|&n| n == name) {
                    let bit = match value {
                        EpicsValue::Char(v)  => v & 1,
                        EpicsValue::Short(v) => (v & 1) as u8,
                        EpicsValue::Long(v)  => (v & 1) as u8,
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    };
                    self.bits[idx] = bit;
                    self.bits_to_val();
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }
}
