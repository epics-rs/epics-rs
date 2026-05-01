use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// Long string output record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
pub struct LsoRecord {
    pub val: String,
    pub oval: String,
    pub sizv: u16,
    pub len: u32,
    pub olen: u32,
    pub ivoa: i16,
    pub ivov: String,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
}

impl Default for LsoRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            sizv: 256,
            len: 1,
            olen: 1,
            ivoa: 0,
            ivov: String::new(),
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl LsoRecord {
    pub fn new(val: &str) -> Self {
        let v = val.to_string();
        let len = (v.len() + 1).min(256) as u32;
        Self {
            val: v,
            len,
            ..Default::default()
        }
    }

    fn clamped(&self) -> String {
        let max = (self.sizv as usize).saturating_sub(1).max(0);
        if self.val.len() > max {
            self.val[..max].to_string()
        } else {
            self.val.clone()
        }
    }
}

static LSO_FIELDS: &[FieldDesc] = &[
    FieldDesc { name: "VAL",  dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "OVAL", dbf_type: DbFieldType::Char,  read_only: true  },
    FieldDesc { name: "SIZV", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "LEN",  dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "OLEN", dbf_type: DbFieldType::Long,  read_only: true  },
    FieldDesc { name: "IVOA", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "IVOV", dbf_type: DbFieldType::Char,  read_only: false },
    FieldDesc { name: "OMSL", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "DOL",  dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "SIMM", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "SIML", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "SIOL", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "SIMS", dbf_type: DbFieldType::Short, read_only: false },
];

impl Record for LsoRecord {
    fn record_type(&self) -> &'static str {
        "lso"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        LSO_FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let clamped = self.clamped();
        self.olen = self.len;
        self.len = (clamped.len() + 1) as u32;
        self.oval = self.val.clone();
        Ok(ProcessOutcome::complete())
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.clamped().into_bytes()))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL"  => Some(EpicsValue::CharArray(self.clamped().into_bytes())),
            "OVAL" => Some(EpicsValue::CharArray(self.oval.clone().into_bytes())),
            "SIZV" => Some(EpicsValue::Short(self.sizv as i16)),
            "LEN"  => Some(EpicsValue::Long(self.len as i32)),
            "OLEN" => Some(EpicsValue::Long(self.olen as i32)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::CharArray(self.ivov.clone().into_bytes())),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL"  => Some(EpicsValue::String(self.dol.clone())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                let s = match value {
                    EpicsValue::String(s) => s,
                    EpicsValue::CharArray(bytes) => {
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        String::from_utf8_lossy(&bytes[..end]).into_owned()
                    }
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                };
                let max = (self.sizv as usize).saturating_sub(1).max(0);
                self.val = if s.len() > max { s[..max].to_string() } else { s };
                self.len = (self.val.len() + 1) as u32;
            }
            "SIZV" => {
                if let EpicsValue::Short(v) = value {
                    self.sizv = v.max(1) as u16;
                } else {
                    return Err(CaError::TypeMismatch("SIZV".into()));
                }
            }
            "IVOA" => { if let EpicsValue::Short(v) = value { self.ivoa = v; } }
            "IVOV" => {
                match value {
                    EpicsValue::String(s) => self.ivov = s,
                    EpicsValue::CharArray(bytes) => {
                        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        self.ivov = String::from_utf8_lossy(&bytes[..end]).into_owned();
                    }
                    _ => return Err(CaError::TypeMismatch("IVOV".into())),
                }
            }
            "OMSL" => { if let EpicsValue::Short(v) = value { self.omsl = v; } }
            "DOL"  => { if let EpicsValue::String(v) = value { self.dol = v; } }
            "SIMM" => { if let EpicsValue::Short(v) = value { self.simm = v; } }
            "SIML" => { if let EpicsValue::String(v) = value { self.siml = v; } }
            "SIOL" => { if let EpicsValue::String(v) = value { self.siol = v; } }
            "SIMS" => { if let EpicsValue::Short(v) = value { self.sims = v; } }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }
}
