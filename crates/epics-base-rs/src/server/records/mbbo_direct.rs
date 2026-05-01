use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// Multi-bit binary output direct record.
// VAL holds the full unsigned 16-bit value; B0-BF expose individual bits as Char (0/1).
// On process: VAL is shifted left by SHFT and written as RVAL.
// Writing to any BX field recomputes VAL from individual bits before the next process.
pub struct MbboDirectRecord {
    pub val: u32,
    pub rval: i32,
    pub oraw: i32,
    pub rbv: i32,
    pub orbv: i32,
    pub mask: i32,
    pub shft: i16,
    pub nobt: i16,
    pub mlst: u32,
    pub ivoa: i16,
    pub ivov: u32,
    pub omsl: i16,
    pub dol: String,
    pub bits: [u8; 16], // B0-BF
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    skip_convert: bool,
}

impl Default for MbboDirectRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            rbv: 0,
            orbv: 0,
            mask: 0,
            shft: 0,
            nobt: 0,
            mlst: 0,
            ivoa: 0,
            ivov: 0,
            omsl: 0,
            dol: String::new(),
            bits: [0; 16],
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            skip_convert: false,
        }
    }
}

impl MbboDirectRecord {
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

static MBBO_DIRECT_FIELDS: &[FieldDesc] = &[
    FieldDesc { name: "VAL",  dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "RVAL", dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "ORAW", dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "RBV",  dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "ORBV", dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "MASK", dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "SHFT", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "NOBT", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "MLST", dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "IVOA", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "IVOV", dbf_type: DbFieldType::Long,  read_only: false },
    FieldDesc { name: "OMSL", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "DOL",  dbf_type: DbFieldType::String, read_only: false },
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

impl Record for MbboDirectRecord {
    fn record_type(&self) -> &'static str {
        "mbboDirect"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        MBBO_DIRECT_FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
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
            let mut raw = self.val as i32;
            if self.shft > 0 {
                raw = (raw as u32).wrapping_shl(self.shft as u32) as i32;
            }
            if self.mask != 0 {
                raw &= self.mask;
            }
            self.rval = raw;
        }
        self.skip_convert = false;
        self.orbv = self.rbv;
        self.rbv = self.rval;
        self.oraw = self.rval;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL"  => Some(EpicsValue::Long(self.val as i32)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "RBV"  => Some(EpicsValue::Long(self.rbv)),
            "ORBV" => Some(EpicsValue::Long(self.orbv)),
            "MASK" => Some(EpicsValue::Long(self.mask)),
            "SHFT" => Some(EpicsValue::Short(self.shft)),
            "NOBT" => Some(EpicsValue::Short(self.nobt)),
            "MLST" => Some(EpicsValue::Long(self.mlst as i32)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Long(self.ivov as i32)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL"  => Some(EpicsValue::String(self.dol.clone())),
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
            "RVAL" => { if let EpicsValue::Long(v) = value { self.rval = v; } }
            "MASK" => { if let EpicsValue::Long(v) = value { self.mask = v; } }
            "SHFT" => { if let EpicsValue::Short(v) = value { self.shft = v; } }
            "NOBT" => { if let EpicsValue::Short(v) = value { self.nobt = v; } }
            "IVOA" => { if let EpicsValue::Short(v) = value { self.ivoa = v; } }
            "IVOV" => { if let EpicsValue::Long(v) = value { self.ivov = v as u32; } }
            "OMSL" => { if let EpicsValue::Short(v) = value { self.omsl = v; } }
            "DOL"  => { if let EpicsValue::String(v) = value { self.dol = v; } }
            "SIMM" => { if let EpicsValue::Short(v) = value { self.simm = v; } }
            "SIML" => { if let EpicsValue::String(v) = value { self.siml = v; } }
            "SIOL" => { if let EpicsValue::String(v) = value { self.siol = v; } }
            "SIMS" => { if let EpicsValue::Short(v) = value { self.sims = v; } }
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
