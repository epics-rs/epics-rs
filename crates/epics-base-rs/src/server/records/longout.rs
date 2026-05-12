//! `longout` — integer output record with optional conditional-output
//! gating via the `OOPT` (output-execution-option) field.
//!
//! Manually implements [`Record`] rather than using `#[derive(EpicsRecord)]`
//! so the trait's [`Record::should_output`] hook can be overridden — the
//! derive macro emits only the four mandatory methods and there's no
//! opt-in for behaviour overrides. Once the macro grows a
//! `should_output_fn` knob this file can switch back to the derive form.

use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, Record};
use crate::types::{DbFieldType, EpicsValue};

pub struct LongoutRecord {
    pub val: i32,
    pub egu: String,
    pub hopr: i32,
    pub lopr: i32,
    pub drvh: i32,
    pub drvl: i32,
    pub hihi: i32,
    pub high: i32,
    pub low: i32,
    pub lolo: i32,
    pub hhsv: i16,
    pub hsv: i16,
    pub lsv: i16,
    pub llsv: i16,
    pub hyst: f64,
    pub lalm: f64,
    pub ivoa: i16,
    pub ivov: i32,
    pub adel: f64,
    pub mdel: f64,
    pub alst: f64,
    pub mlst: f64,
    pub omsl: i16,
    pub dol: String,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// Output Execution Option (epics-base 7.0.8):
    ///
    /// | OOPT | Meaning                |
    /// |------|------------------------|
    /// | 0    | Every Time (default)   |
    /// | 1    | On Change (val ≠ pval) |
    /// | 2    | When Zero              |
    /// | 3    | When Non-zero          |
    /// | 4    | Transition to Zero     |
    /// | 5    | Transition to Non-zero |
    ///
    /// Consulted by [`Record::should_output`] before the framework
    /// writes VAL to the OUT link / device. Unknown values default to
    /// `false` (no output) matching C EPICS.
    pub oopt: i16,
    /// Previous VAL — captured after every output cycle so OOPT=1/4/5
    /// (transition modes) can detect changes. Initially zero; loops
    /// after the first successful output.
    pub pval: i32,
}

impl Default for LongoutRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: String::new(),
            hopr: 0,
            lopr: 0,
            drvh: 0, // C defaults both to 0 (equal = no clamping)
            drvl: 0,
            hihi: 0,
            high: 0,
            low: 0,
            lolo: 0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            ivoa: 0,
            ivov: 0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            omsl: 0,
            dol: String::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            oopt: 0,
            pval: 0,
        }
    }
}

impl LongoutRecord {
    pub fn new(val: i32) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// Compute whether the framework should propagate VAL to the OUT
    /// link / device on this process cycle, based on OOPT semantics.
    /// Public so unit tests don't have to spin up the full processing
    /// loop to exercise each menu value.
    pub fn compute_should_output(&self) -> bool {
        match self.oopt {
            0 => true,
            1 => self.val != self.pval,
            2 => self.val == 0,
            3 => self.val != 0,
            4 => self.pval != 0 && self.val == 0,
            5 => self.pval == 0 && self.val != 0,
            _ => false,
        }
    }
}

static LONGOUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "DRVH",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "DRVL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "HIHI",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "HIGH",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "LOW",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "LOLO",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "HHSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LLSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HYST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LALM",
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
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
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
    FieldDesc {
        name: "OOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "PVAL",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
];

impl Record for LongoutRecord {
    fn record_type(&self) -> &'static str {
        "longout"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        LONGOUT_FIELDS
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Long(self.hopr)),
            "LOPR" => Some(EpicsValue::Long(self.lopr)),
            "DRVH" => Some(EpicsValue::Long(self.drvh)),
            "DRVL" => Some(EpicsValue::Long(self.drvl)),
            "HIHI" => Some(EpicsValue::Long(self.hihi)),
            "HIGH" => Some(EpicsValue::Long(self.high)),
            "LOW" => Some(EpicsValue::Long(self.low)),
            "LOLO" => Some(EpicsValue::Long(self.lolo)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Long(self.ivov)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "PVAL" => Some(EpicsValue::Long(self.pval)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.validate_put(name, &value)?;
        match name {
            "VAL" => {
                if let EpicsValue::Long(v) = value {
                    self.val = v;
                }
            }
            "EGU" => {
                if let EpicsValue::String(v) = value {
                    self.egu = v;
                }
            }
            "HOPR" => {
                if let EpicsValue::Long(v) = value {
                    self.hopr = v;
                }
            }
            "LOPR" => {
                if let EpicsValue::Long(v) = value {
                    self.lopr = v;
                }
            }
            "DRVH" => {
                if let EpicsValue::Long(v) = value {
                    self.drvh = v;
                }
            }
            "DRVL" => {
                if let EpicsValue::Long(v) = value {
                    self.drvl = v;
                }
            }
            "HIHI" => {
                if let EpicsValue::Long(v) = value {
                    self.hihi = v;
                }
            }
            "HIGH" => {
                if let EpicsValue::Long(v) = value {
                    self.high = v;
                }
            }
            "LOW" => {
                if let EpicsValue::Long(v) = value {
                    self.low = v;
                }
            }
            "LOLO" => {
                if let EpicsValue::Long(v) = value {
                    self.lolo = v;
                }
            }
            "HHSV" => {
                if let EpicsValue::Short(v) = value {
                    self.hhsv = v;
                }
            }
            "HSV" => {
                if let EpicsValue::Short(v) = value {
                    self.hsv = v;
                }
            }
            "LSV" => {
                if let EpicsValue::Short(v) = value {
                    self.lsv = v;
                }
            }
            "LLSV" => {
                if let EpicsValue::Short(v) = value {
                    self.llsv = v;
                }
            }
            "HYST" => {
                if let EpicsValue::Double(v) = value {
                    self.hyst = v;
                }
            }
            "LALM" => {
                if let EpicsValue::Double(v) = value {
                    self.lalm = v;
                }
            }
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                }
            }
            "IVOV" => {
                if let EpicsValue::Long(v) = value {
                    self.ivov = v;
                }
            }
            "ADEL" => {
                if let EpicsValue::Double(v) = value {
                    self.adel = v;
                }
            }
            "MDEL" => {
                if let EpicsValue::Double(v) = value {
                    self.mdel = v;
                }
            }
            "ALST" => {
                if let EpicsValue::Double(v) = value {
                    self.alst = v;
                }
            }
            "MLST" => {
                if let EpicsValue::Double(v) = value {
                    self.mlst = v;
                }
            }
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                }
            }
            "DOL" => {
                if let EpicsValue::String(v) = value {
                    self.dol = v;
                }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value {
                    self.simm = v;
                }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value {
                    self.siml = v;
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v;
                }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value {
                    self.sims = v;
                }
            }
            "OOPT" => {
                if let EpicsValue::Short(v) = value {
                    self.oopt = v;
                }
            }
            "PVAL" => {
                // Read-only — validate_put already rejected the call.
                return Err(CaError::ReadOnlyField("PVAL".into()));
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        self.on_put(name);
        Ok(())
    }

    /// epics-base 7.0.8: gate the OUT-link / device write on OOPT.
    /// Returns `false` for OOPT modes whose condition isn't met
    /// (On Change / transition / when-zero / when-non-zero). The
    /// framework's processing.rs reads this before issuing the
    /// device write, so a `should_output() == false` cycle skips
    /// the device call entirely.
    fn should_output(&self) -> bool {
        self.compute_should_output()
    }

    /// Latch PVAL after a successful output so OOPT=1/4/5 transition
    /// detection on the next cycle has the right reference point.
    fn on_output_complete(&mut self) {
        self.pval = self.val;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oopt_every_time_default() {
        let r = LongoutRecord::new(42);
        assert!(r.compute_should_output());
    }

    #[test]
    fn oopt_on_change() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 1;
        r.pval = 5;
        r.val = 5;
        assert!(!r.compute_should_output(), "val==pval suppresses output");
        r.val = 7;
        assert!(r.compute_should_output(), "val!=pval permits output");
    }

    #[test]
    fn oopt_when_zero_and_nonzero() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 2; // When Zero
        r.val = 0;
        assert!(r.compute_should_output());
        r.val = 1;
        assert!(!r.compute_should_output());

        r.oopt = 3; // When Non-zero
        r.val = 0;
        assert!(!r.compute_should_output());
        r.val = 5;
        assert!(r.compute_should_output());
    }

    #[test]
    fn oopt_transitions() {
        let mut r = LongoutRecord::new(0);
        r.oopt = 4; // Transition to Zero
        r.pval = 5;
        r.val = 0;
        assert!(r.compute_should_output(), "nonzero→zero transition fires");
        r.pval = 0;
        r.val = 0;
        assert!(!r.compute_should_output(), "zero→zero suppressed");

        r.oopt = 5; // Transition to Non-zero
        r.pval = 0;
        r.val = 5;
        assert!(r.compute_should_output(), "zero→nonzero transition fires");
        r.pval = 5;
        r.val = 5;
        assert!(!r.compute_should_output(), "nonzero→nonzero suppressed");
    }

    #[test]
    fn oopt_unknown_value_suppresses_output() {
        // C EPICS treats unknown OOPT as "don't output" to fail-safe.
        let mut r = LongoutRecord::new(0);
        r.oopt = 99;
        assert!(!r.compute_should_output());
    }

    #[test]
    fn put_get_oopt_roundtrip() {
        let mut r = LongoutRecord::new(0);
        r.put_field("OOPT", EpicsValue::Short(3)).unwrap();
        assert_eq!(r.get_field("OOPT"), Some(EpicsValue::Short(3)));
        assert_eq!(r.oopt, 3);
    }

    #[test]
    fn pval_is_read_only_via_put_field() {
        let mut r = LongoutRecord::new(0);
        let err = r.put_field("PVAL", EpicsValue::Long(42)).unwrap_err();
        assert!(matches!(err, CaError::ReadOnlyField(_)));
    }
}
