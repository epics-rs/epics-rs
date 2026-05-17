use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

use super::mbbi_direct::BIT_NAMES;

// Multi-bit binary output direct record.
// VAL holds the full unsigned 32-bit value; B0-B1F expose individual bits
// as Char (0/1). C `mbboDirectRecord.c` defines `NUM_BITS 32`.
// On process: VAL is shifted left by SHFT and written as RVAL.
// Writing to any BX field recomputes VAL from individual bits before the
// next process.
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
    pub bits: [u8; 32], // B0-B1F
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
            bits: [0; 32],
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

fn bit_field_descs() -> &'static [FieldDesc] {
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

static MBBO_DIRECT_HEAD_FIELDS: &[FieldDesc] = &[
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
        name: "RBV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ORBV",
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
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DOL",
        dbf_type: DbFieldType::String,
        read_only: false,
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

fn mbbo_direct_fields() -> &'static [FieldDesc] {
    use std::sync::OnceLock;
    static ALL: OnceLock<Vec<FieldDesc>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut v: Vec<FieldDesc> = MBBO_DIRECT_HEAD_FIELDS.to_vec();
        v.extend_from_slice(bit_field_descs());
        v
    })
}

impl Record for MbboDirectRecord {
    fn record_type(&self) -> &'static str {
        "mbboDirect"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        mbbo_direct_fields()
    }

    // C recMbboDirect.c IVOA=set_to_IVOV: val = ivov; rval = ivov.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        // IVOV is Long on mbboDirect; both VAL and RVAL accept Long.
        self.put_field("RVAL", ivov.clone())?;
        self.put_field("VAL", ivov)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `mbboDirectRecord.c::init_record` — MASK from NOBT,
            // NOBT may span 1..32 (NUM_BITS 32).
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as i32;
            }
            self.mlst = self.val;
            self.oraw = self.rval;
            // Don't derive bits from VAL yet — `post_init_finalize_undef`
            // runs after both passes and chooses VAL→bits or bits→VAL
            // based on the framework's UDF flag.
        }
        Ok(())
    }

    /// epics-base PR `dabcf89` (2021): when the record initialises
    /// with no VAL set (UDF=true) but the operator populated B0..B1F
    /// bits in the .db file, fold those bits into VAL and clear UDF.
    /// Otherwise (VAL was set explicitly), derive bits from VAL.
    fn post_init_finalize_undef(&mut self, common_udf: &mut bool) -> CaResult<()> {
        if !*common_udf {
            self.val_to_bits();
        } else {
            let any_bit_set = self.bits.iter().any(|&b| b != 0);
            if any_bit_set {
                self.bits_to_val();
                *common_udf = false;
            }
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            // C `mbboDirectRecord.c::convert` — RVAL = VAL << SHFT only.
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
        // C `mbboDirectRecord.c` — RBV is updated ONLY by device support
        // (the hardware read-back); record support never assigns it.
        // Forcing `RBV = RVAL` here would mask hardware disagreement,
        // so we only roll `orbv` forward and leave `rbv` untouched.
        self.orbv = self.rbv;
        self.oraw = self.rval;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val as i32)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "RBV" => Some(EpicsValue::Long(self.rbv)),
            "ORBV" => Some(EpicsValue::Long(self.orbv)),
            "MASK" => Some(EpicsValue::Long(self.mask)),
            "SHFT" => Some(EpicsValue::Short(self.shft)),
            "NOBT" => Some(EpicsValue::Short(self.nobt)),
            "MLST" => Some(EpicsValue::Long(self.mlst as i32)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Long(self.ivov as i32)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
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
            "RBV" => {
                if let EpicsValue::Long(v) = value {
                    self.rbv = v;
                } else {
                    return Err(CaError::TypeMismatch("RBV".into()));
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
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                } else {
                    return Err(CaError::TypeMismatch("IVOA".into()));
                }
            }
            "IVOV" => {
                if let EpicsValue::Long(v) = value {
                    self.ivov = v as u32;
                } else {
                    return Err(CaError::TypeMismatch("IVOV".into()));
                }
            }
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                } else {
                    return Err(CaError::TypeMismatch("OMSL".into()));
                }
            }
            "DOL" => {
                if let EpicsValue::String(v) = value {
                    self.dol = v;
                } else {
                    return Err(CaError::TypeMismatch("DOL".into()));
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
}
