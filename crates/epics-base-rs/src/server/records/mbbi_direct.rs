use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// Multi-bit binary input direct record.
// VAL holds the full unsigned 32-bit value; B0-B1F expose individual bits
// as Char (0/1). C `mbbiDirectRecord.c` defines `NUM_BITS 32`, so the
// bit-field interface spans B0..B1F (32 fields).
// On process: RVAL is shifted right by SHFT and stored in VAL and bit fields.
pub struct MbbiDirectRecord {
    pub val: u32,
    pub rval: i32,
    pub oraw: i32,
    pub mask: i32,
    pub shft: i16,
    pub nobt: i16,
    pub mlst: u32,
    pub bits: [u8; 32], // B0-B1F
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
            bits: [0; 32],
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
        for i in 0..32 {
            self.bits[i] = ((self.val >> i) & 1) as u8;
        }
    }

    fn bits_to_val(&mut self) {
        self.val = 0;
        for i in 0..32 {
            self.val |= (self.bits[i] as u32 & 1) << i;
        }
    }
}

/// Bit field names B0..B1F — 32 entries, matching C `NUM_BITS 32`.
pub(crate) const BIT_NAMES: [&str; 32] = [
    "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "BA", "BB", "BC", "BD", "BE", "BF",
    "B10", "B11", "B12", "B13", "B14", "B15", "B16", "B17", "B18", "B19", "B1A", "B1B", "B1C",
    "B1D", "B1E", "B1F",
];

fn bit_field_descs() -> &'static [FieldDesc] {
    // Const-evaluated table of the 32 bit fields.
    macro_rules! bf {
        ($name:literal) => {
            FieldDesc {
                name: $name,
                dbf_type: DbFieldType::Char,
                read_only: false,
            }
        };
    }
    static BITS: [FieldDesc; 32] = [
        bf!("B0"),
        bf!("B1"),
        bf!("B2"),
        bf!("B3"),
        bf!("B4"),
        bf!("B5"),
        bf!("B6"),
        bf!("B7"),
        bf!("B8"),
        bf!("B9"),
        bf!("BA"),
        bf!("BB"),
        bf!("BC"),
        bf!("BD"),
        bf!("BE"),
        bf!("BF"),
        bf!("B10"),
        bf!("B11"),
        bf!("B12"),
        bf!("B13"),
        bf!("B14"),
        bf!("B15"),
        bf!("B16"),
        bf!("B17"),
        bf!("B18"),
        bf!("B19"),
        bf!("B1A"),
        bf!("B1B"),
        bf!("B1C"),
        bf!("B1D"),
        bf!("B1E"),
        bf!("B1F"),
    ];
    &BITS
}

static MBBI_DIRECT_HEAD_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ORAW",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "MASK",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "SHFT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "NOBT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "SIMM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SIML",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIMS",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

/// Full field table: the 11 scalar fields followed by the 32 bit fields.
fn mbbi_direct_fields() -> &'static [FieldDesc] {
    use std::sync::OnceLock;
    static ALL: OnceLock<Vec<FieldDesc>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut v: Vec<FieldDesc> = MBBI_DIRECT_HEAD_FIELDS.to_vec();
        v.extend_from_slice(bit_field_descs());
        v
    })
}

impl Record for MbbiDirectRecord {
    fn record_type(&self) -> &'static str {
        "mbbiDirect"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        mbbi_direct_fields()
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `mbbiDirectRecord.c::init_record` — MASK from NOBT,
            // NOBT may span 1..32 (NUM_BITS 32).
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
            "VAL" => Some(EpicsValue::Long(self.val as i32)),
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
            _ => BIT_NAMES
                .iter()
                .position(|&n| n == name)
                .map(|idx| EpicsValue::Char(self.bits[idx])),
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                match value {
                    EpicsValue::Long(v) => self.val = v as u32,
                    EpicsValue::Short(v) => self.val = v as u32,
                    EpicsValue::Char(v) => self.val = v as u32,
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                }
                self.val_to_bits();
            }
            "RVAL" => {
                if let EpicsValue::Long(v) = value {
                    self.rval = v;
                } else {
                    return Err(CaError::TypeMismatch("RVAL".into()));
                }
            }
            "MASK" => {
                if let EpicsValue::Long(v) = value {
                    self.mask = v;
                } else {
                    return Err(CaError::TypeMismatch("MASK".into()));
                }
            }
            "SHFT" => {
                if let EpicsValue::Short(v) = value {
                    self.shft = v;
                } else {
                    return Err(CaError::TypeMismatch("SHFT".into()));
                }
            }
            "NOBT" => {
                if let EpicsValue::Short(v) = value {
                    self.nobt = v;
                } else {
                    return Err(CaError::TypeMismatch("NOBT".into()));
                }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value {
                    self.simm = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMM".into()));
                }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value {
                    self.siml = v;
                } else {
                    return Err(CaError::TypeMismatch("SIML".into()));
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v;
                } else {
                    return Err(CaError::TypeMismatch("SIOL".into()));
                }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value {
                    self.sims = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMS".into()));
                }
            }
            _ => {
                if let Some(idx) = BIT_NAMES.iter().position(|&n| n == name) {
                    let bit = match value {
                        EpicsValue::Char(v) => v & 1,
                        EpicsValue::Short(v) => (v & 1) as u8,
                        EpicsValue::Long(v) => (v & 1) as u8,
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

    /// `mbbiDirect` has an `RVAL → VAL` `convert()` step. A `Soft
    /// Channel` `mbbiDirect` must skip it — C `devMbbiDirectSoft.c`
    /// `read_mbbiDirect` returns 2.
    fn soft_channel_skips_convert(&self) -> bool {
        true
    }
}
