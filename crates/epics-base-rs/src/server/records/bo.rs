use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessAction, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Binary output record matching C boRecord behavior.
/// VAL is converted to RVAL using MASK before writing to hardware.
pub struct BoRecord {
    pub val: u16,
    pub rval: i32,
    pub oraw: i32, // old raw value for monitor
    pub rbv: i32,  // readback value
    pub orbv: i32, // old readback value
    pub mask: i32, // hardware mask from device support
    // Strings
    pub znam: String,
    pub onam: String,
    // Alarm
    pub zsv: i16,
    pub osv: i16,
    pub cosv: i16,
    pub lalm: u16, // last alarm value (for COS alarm)
    // Monitor
    pub mlst: u16, // last monitored value
    // Output control
    pub omsl: i16,   // 0=supervisory, 1=closed_loop
    pub dol: String, // desired output location link
    pub high: f64,   // seconds to hold output high (toggle delay)
    // Invalid output
    pub ivoa: i16, // 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: u16, // invalid output value
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
}

impl Default for BoRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            rbv: 0,
            orbv: 0,
            mask: 0,
            znam: String::new(),
            onam: String::new(),
            zsv: 0,
            osv: 0,
            cosv: 0,
            lalm: 0,
            mlst: 0,
            omsl: 0,
            dol: String::new(),
            high: 0.0,
            ivoa: 0,
            ivov: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
        }
    }
}

impl BoRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// Convert VAL to RVAL using MASK (C: convert val to rval)
    fn val_to_rval(&mut self) {
        if self.mask != 0 {
            if self.val == 0 {
                self.rval = 0;
            } else {
                self.rval = self.mask;
            }
        } else {
            self.rval = self.val as i32;
        }
    }
}

/// Try to parse a DOL string as a constant value.
fn dol_as_constant(dol: &str) -> Option<u16> {
    let s = dol.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<u16>().ok()
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Enum,
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
        name: "ZNAM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ONAM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ZSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "COSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Enum,
        read_only: true,
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
        name: "HIGH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Enum,
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

impl Record for BoRecord {
    fn record_type(&self) -> &'static str {
        "bo"
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // DOL constant initialization: normalize to 0/1 (like C: !!ival)
            if let Some(v) = dol_as_constant(&self.dol) {
                self.val = if v != 0 { 1 } else { 0 };
            }

            // Convert val to rval
            self.val_to_rval();

            // Initialize tracking fields
            self.mlst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;
            self.orbv = self.rbv;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // DOL/OMSL: constant DOL handling
        if self.omsl == 1 && !self.dol.is_empty() {
            if let Some(v) = dol_as_constant(&self.dol) {
                self.val = if v != 0 { 1 } else { 0 };
            }
        }

        // Convert val to rval using mask
        self.val_to_rval();

        self.oraw = self.rval;
        self.orbv = self.rbv;

        // HIGH toggle: if val==1 and high>0, schedule reprocess after HIGH seconds
        let mut actions = Vec::new();
        if self.val == 1 && self.high > 0.0 {
            actions.push(ProcessAction::ReprocessAfter(
                std::time::Duration::from_secs_f64(self.high),
            ));
        }

        Ok(ProcessOutcome {
            result: crate::server::record::RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
        })
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "RBV" => Some(EpicsValue::Long(self.rbv)),
            "ORBV" => Some(EpicsValue::Long(self.orbv)),
            "MASK" => Some(EpicsValue::Long(self.mask)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "ZSV" => Some(EpicsValue::Short(self.zsv)),
            "OSV" => Some(EpicsValue::Short(self.osv)),
            "COSV" => Some(EpicsValue::Short(self.cosv)),
            "LALM" => Some(EpicsValue::Enum(self.lalm)),
            "MLST" => Some(EpicsValue::Enum(self.mlst)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Enum(self.ivov)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Enum(v) => {
                    self.val = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.val = v as u16;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.val = v as u16;
                    Ok(())
                }
                // PR/issue #183 — accept ZNAM/ONAM string.
                EpicsValue::String(s) => {
                    if s == self.znam {
                        self.val = 0;
                        Ok(())
                    } else if s == self.onam {
                        self.val = 1;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch(format!(
                            "bo VAL: '{s}' matches neither ZNAM nor ONAM"
                        )))
                    }
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RVAL" => match value {
                EpicsValue::Long(v) => {
                    self.rval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MASK" => match value {
                EpicsValue::Long(v) => {
                    self.mask = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ZNAM" => match value {
                EpicsValue::String(v) => {
                    self.znam = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ONAM" => match value {
                EpicsValue::String(v) => {
                    self.onam = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ZSV" => match value {
                EpicsValue::Short(v) => {
                    self.zsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OSV" => match value {
                EpicsValue::Short(v) => {
                    self.osv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "COSV" => match value {
                EpicsValue::Short(v) => {
                    self.cosv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LALM" => match value {
                EpicsValue::Enum(v) => {
                    self.lalm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MLST" => match value {
                EpicsValue::Enum(v) => {
                    self.mlst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OMSL" => match value {
                EpicsValue::Short(v) => {
                    self.omsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DOL" => match value {
                EpicsValue::String(v) => {
                    self.dol = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HIGH" => match value {
                EpicsValue::Double(v) => {
                    self.high = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IVOV" => match value {
                EpicsValue::Enum(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIML" => match value {
                EpicsValue::String(v) => {
                    self.siml = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIOL" => match value {
                EpicsValue::String(v) => {
                    self.siol = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }
}
