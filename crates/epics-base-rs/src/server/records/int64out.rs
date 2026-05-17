use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// int64out: 64-bit integer output.
// CA limitation: served as DBR_DOUBLE over Channel Access (precision loss for |val|>2^53).
// Native i64 storage is lossless; precision is only lost at the CA wire boundary.
//
// Alarm threshold fields (HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV) are intentionally absent
// from the field list so they route to RecordInstance::common.analog_alarm via
// put_common_field, matching the path used by longout/ao.
//
// Manually implements [`Record`] (rather than `#[derive(EpicsRecord)]`) so the
// `process()` hook can clamp VAL to the DRVH/DRVL drive-limit window — C parity
// with `int64outRecord.c::convert` (see [`Record::process`] below).
pub struct Int64outRecord {
    pub val: i64,
    pub egu: String,
    pub hopr: f64,
    pub lopr: f64,
    pub drvh: f64,
    pub drvl: f64,
    pub hyst: f64,
    pub lalm: f64,
    pub ivoa: i16,
    pub ivov: f64,
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
}

impl Default for Int64outRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: String::new(),
            hopr: 0.0,
            lopr: 0.0,
            drvh: 0.0,
            drvl: 0.0,
            hyst: 0.0,
            lalm: 0.0,
            ivoa: 0,
            ivov: 0.0,
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
        }
    }
}

impl Int64outRecord {
    pub fn new(val: i64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}

static INT64OUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Int64,
        read_only: false,
    },
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVL",
        dbf_type: DbFieldType::Double,
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
        dbf_type: DbFieldType::Double,
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
];

impl Record for Int64outRecord {
    fn record_type(&self) -> &'static str {
        "int64out"
    }

    /// C `int64outRecord.c::convert` (lines 418-423): clamp VAL into the
    /// drive-limit window `[DRVL, DRVH]` every process cycle, but only
    /// when `DRVH > DRVL` (equal limits = no clamping). DRVH/DRVL are
    /// DBF_DOUBLE in int64out's .dbd; the comparison and clamp are done
    /// in i64 space (the C `convert` clamps the `epicsInt64 value`
    /// against `prec->drvh`/`prec->drvl`, which the C struct also stores
    /// as DBF_INT64 — here the limits are doubles, so they are rounded
    /// to i64 with saturation before the clamp, matching the integer
    /// semantics of the C clamp).
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.drvh > self.drvl {
            let hi = double_to_i64_saturating(self.drvh);
            let lo = double_to_i64_saturating(self.drvl);
            if self.val > hi {
                self.val = hi;
            } else if self.val < lo {
                self.val = lo;
            }
        }
        Ok(ProcessOutcome::complete())
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        INT64OUT_FIELDS
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Int64(self.val)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "DRVH" => Some(EpicsValue::Double(self.drvh)),
            "DRVL" => Some(EpicsValue::Double(self.drvl)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
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
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.validate_put(name, &value)?;
        match name {
            "VAL" => {
                if let EpicsValue::Int64(v) = value {
                    self.val = v;
                } else {
                    return Err(CaError::TypeMismatch("VAL".into()));
                }
            }
            "EGU" => {
                if let EpicsValue::String(v) = value {
                    self.egu = v;
                } else {
                    return Err(CaError::TypeMismatch("EGU".into()));
                }
            }
            "HOPR" => {
                self.hopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HOPR".into()))?;
            }
            "LOPR" => {
                self.lopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOPR".into()))?;
            }
            "DRVH" => {
                self.drvh = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("DRVH".into()))?;
            }
            "DRVL" => {
                self.drvl = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("DRVL".into()))?;
            }
            "HYST" => {
                self.hyst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HYST".into()))?;
            }
            "LALM" => {
                self.lalm = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LALM".into()))?;
            }
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                } else {
                    return Err(CaError::TypeMismatch("IVOA".into()));
                }
            }
            "IVOV" => {
                self.ivov = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("IVOV".into()))?;
            }
            "ADEL" => {
                self.adel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ADEL".into()))?;
            }
            "MDEL" => {
                self.mdel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MDEL".into()))?;
            }
            "ALST" => {
                self.alst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ALST".into()))?;
            }
            "MLST" => {
                self.mlst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MLST".into()))?;
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
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        self.on_put(name);
        Ok(())
    }
}

/// Round a DBF_DOUBLE drive limit to i64 with saturation. NaN maps to 0,
/// out-of-range values saturate to i64::MIN/MAX. Used so the int64out
/// drive-limit clamp operates in the same integer space as the C
/// `int64outRecord.c::convert` clamp.
fn double_to_i64_saturating(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else if v >= i64::MAX as f64 {
        i64::MAX
    } else if v <= i64::MIN as f64 {
        i64::MIN
    } else {
        v as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_val_into_drive_window() {
        let mut r = Int64outRecord::new(0);
        r.drvl = -10.0;
        r.drvh = 10.0;
        r.val = 50;
        r.process().unwrap();
        assert_eq!(r.val, 10, "above DRVH clamps down to DRVH");
        r.val = -50;
        r.process().unwrap();
        assert_eq!(r.val, -10, "below DRVL clamps up to DRVL");
        r.val = 3;
        r.process().unwrap();
        assert_eq!(r.val, 3, "in-window value untouched");
    }

    #[test]
    fn equal_drive_limits_disable_clamp() {
        let mut r = Int64outRecord::new(0);
        r.drvl = 0.0;
        r.drvh = 0.0; // DRVH not > DRVL
        r.val = 99999;
        r.process().unwrap();
        assert_eq!(r.val, 99999, "DRVH == DRVL: no clamping (C parity)");
    }

    #[test]
    fn put_get_val_roundtrip() {
        let mut r = Int64outRecord::new(0);
        r.put_field("VAL", EpicsValue::Int64(1234567890123)).unwrap();
        assert_eq!(r.get_field("VAL"), Some(EpicsValue::Int64(1234567890123)));
    }
}
