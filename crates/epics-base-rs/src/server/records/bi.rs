use std::time::SystemTime;

use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// AFTC low-pass alarm-severity filter shared by `bi` and `mbbi`.
///
/// Mirrors C `biRecord.c::checkAlarms` / `mbbiRecord.c::checkAlarms`
/// (epics-base PR #817): when `aftc > 0` the raw state severity is
/// run through an exponential filter so a momentary state change does
/// not raise an alarm until the signal has been in the alarm range for
/// roughly `aftc` seconds. Returns `(filtered_alarm, new_afvl)`.
pub(crate) fn aftc_filter(
    raw_alarm: u16,
    aftc: f64,
    afvl_in: f64,
    time_last: SystemTime,
    time_now: SystemTime,
) -> (u16, f64) {
    const THRESHOLD: f64 = 0.6321; // 1 - 1/e
    if aftc <= 0.0 {
        return (raw_alarm, 0.0);
    }
    if afvl_in == 0.0 {
        // Initial sample: seed the accumulator without filtering.
        return (raw_alarm, raw_alarm as f64);
    }
    let dt = time_now
        .duration_since(time_last)
        .unwrap_or_default()
        .as_secs_f64();
    let alpha = aftc / (dt + aftc);
    let mut afvl = alpha * afvl_in
        + if afvl_in > 0.0 {
            1.0 - alpha
        } else {
            alpha - 1.0
        } * (raw_alarm as f64);
    if afvl - afvl.floor() > THRESHOLD {
        afvl = -afvl;
    }
    let alarm = afvl.floor().abs() as u16;
    (alarm, afvl)
}

/// Binary input record matching C biRecord behavior.
/// RVAL from device support is converted to VAL (0 or 1).
pub struct BiRecord {
    pub val: u16,
    pub rval: i32,
    pub oraw: i32, // old raw value for monitor
    pub mask: i32, // hardware mask from device support
    // Strings
    pub znam: String,
    pub onam: String,
    // Alarm
    pub zsv: i16,
    pub osv: i16,
    pub cosv: i16,
    /// Alarm filter time constant (seconds). 0 = filter disabled.
    /// Mirrors epics-base PR #817: a low-pass filter on alarm severity
    /// that delays reporting until the signal has been in the alarm
    /// range for AFTC seconds.
    pub aftc: f64,
    /// Alarm filter accumulator. Carries low-pass filter state between
    /// process cycles; 0 means "initial sample, no prior filter state".
    pub afvl: f64,
    pub lalm: u16, // last alarm value (for COS alarm)
    // Monitor
    pub mlst: u16, // last monitored value
    // Simulation
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    // Internal: skip RVAL->VAL when soft INP set VAL directly
    skip_convert: bool,
}

impl Default for BiRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            znam: String::new(),
            onam: String::new(),
            zsv: 0,
            osv: 0,
            cosv: 0,
            aftc: 0.0,
            afvl: 0.0,
            lalm: 0,
            mlst: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            skip_convert: false,
        }
    }
}

impl BiRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
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
        name: "AFTC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "AFVL",
        dbf_type: DbFieldType::Double,
        read_only: true,
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

impl Record for BiRecord {
    fn record_type(&self) -> &'static str {
        "bi"
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // Initialize tracking fields from current val
            self.mlst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Skip RVAL->VAL conversion when soft INP already set VAL (C: status==2)
        if !self.skip_convert {
            if self.rval == 0 {
                self.val = 0;
            } else {
                self.val = 1;
            }
        }
        self.skip_convert = false; // reset for next cycle

        self.oraw = self.rval;
        Ok(ProcessOutcome::complete())
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.skip_convert = did_compute;
    }

    /// C `biRecord.c::checkAlarms` — UDF alarm, STATE alarm (ZSV/OSV
    /// with the AFTC low-pass filter) and COS alarm (COSV). C
    /// `checkAlarms:232-235` raises `UDF_ALARM/udfs` and returns early
    /// when `udf` is set; we mirror that here (raising UDF is
    /// idempotent with the framework's own `rec_gbl_check_udf`, which
    /// also runs on the process path).
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::record::AlarmSeverity;
        use crate::server::recgbl::{self, alarm_status};

        if common.udf {
            recgbl::rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, common.udfs);
            return;
        }
        let val = self.val;
        if val > 1 {
            return;
        }
        let state_sev = if val == 0 { self.zsv } else { self.osv };
        // AFTC low-pass filter on the state severity (PR #817).
        let (filtered, new_afvl) = aftc_filter(
            state_sev as u16,
            self.aftc,
            self.afvl,
            common.time,
            crate::runtime::general_time::get_current(),
        );
        self.afvl = new_afvl;
        let sev = AlarmSeverity::from_u16(filtered);
        if sev != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, sev);
        }
        // COS alarm — fires only when VAL changed from LALM.
        if val != self.lalm {
            let cos_sev = AlarmSeverity::from_u16(self.cosv as u16);
            if cos_sev != AlarmSeverity::NoAlarm {
                recgbl::rec_gbl_set_sevr(common, alarm_status::COS_ALARM, cos_sev);
            }
            self.lalm = val;
        }
    }

    fn accepts_raw_soft_input(&self) -> bool {
        true
    }

    /// `DTYP="Raw Soft Channel"` reads the link value into `RVAL` and
    /// applies `MASK` (epics-base f2fe9d12, devBiSoftRaw): a non-zero
    /// MASK gates which bits of the source contribute to the
    /// subsequent RVAL→VAL conversion.
    fn apply_raw_input(&mut self, value: EpicsValue) -> CaResult<()> {
        let rval = value.to_f64().map(|f| f as i32).ok_or_else(|| {
            CaError::TypeMismatch("bi Raw Soft Channel: INP value not numeric".into())
        })?;
        self.rval = rval;
        if self.mask != 0 {
            self.rval &= self.mask;
        }
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "ORAW" => Some(EpicsValue::Long(self.oraw)),
            "MASK" => Some(EpicsValue::Long(self.mask)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "ZSV" => Some(EpicsValue::Short(self.zsv)),
            "OSV" => Some(EpicsValue::Short(self.osv)),
            "COSV" => Some(EpicsValue::Short(self.cosv)),
            "AFTC" => Some(EpicsValue::Double(self.aftc)),
            "AFVL" => Some(EpicsValue::Double(self.afvl)),
            "LALM" => Some(EpicsValue::Enum(self.lalm)),
            "MLST" => Some(EpicsValue::Enum(self.mlst)),
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
                // epics-base PR/issue #183 — DBF_MENU ↔ DBF_STRING.
                // Accept the ZNAM/ONAM string and convert to the
                // enum index. Mirrors the upstream fix that lets a
                // bi VAL be written from a string-typed source link.
                EpicsValue::String(s) => {
                    if s == self.znam {
                        self.val = 0;
                        Ok(())
                    } else if s == self.onam {
                        self.val = 1;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch(format!(
                            "bi VAL: '{s}' matches neither ZNAM nor ONAM"
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
            "AFTC" => match value {
                EpicsValue::Double(v) => {
                    self.aftc = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.aftc = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch(name.into()))
                    }
                }
            },
            "AFVL" => match value {
                EpicsValue::Double(v) => {
                    self.afvl = v;
                    Ok(())
                }
                v => {
                    if let Some(f) = v.to_f64() {
                        self.afvl = f;
                        Ok(())
                    } else {
                        Err(CaError::TypeMismatch(name.into()))
                    }
                }
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
