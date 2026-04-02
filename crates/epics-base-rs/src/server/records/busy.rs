use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// --- Busy-specific types (inlined from busy-rs/types.rs) ---

/// Output Mode Select
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Omsl {
    #[default]
    Supervisory = 0,
    ClosedLoop = 1,
}

impl From<i16> for Omsl {
    fn from(v: i16) -> Self {
        match v {
            1 => Self::ClosedLoop,
            _ => Self::Supervisory,
        }
    }
}

impl From<Omsl> for i16 {
    fn from(v: Omsl) -> Self {
        v as i16
    }
}

/// Invalid Output Action
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ivoa {
    #[default]
    ContinueNormally = 0,
    DontDriveOutputs = 1,
    SetOutputToIvov = 2,
}

impl From<i16> for Ivoa {
    fn from(v: i16) -> Self {
        match v {
            1 => Self::DontDriveOutputs,
            2 => Self::SetOutputToIvov,
            _ => Self::ContinueNormally,
        }
    }
}

impl From<Ivoa> for i16 {
    fn from(v: Ivoa) -> Self {
        v as i16
    }
}

/// Alarm severity for ZSV/OSV/COSV fields
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlarmSevr {
    #[default]
    None = 0,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl From<i16> for AlarmSevr {
    fn from(v: i16) -> Self {
        match v {
            1 => Self::Minor,
            2 => Self::Major,
            3 => Self::Invalid,
            _ => Self::None,
        }
    }
}

impl From<AlarmSevr> for i16 {
    fn from(v: AlarmSevr) -> Self {
        v as i16
    }
}

impl AlarmSevr {
    pub fn to_base(self) -> crate::server::record::AlarmSeverity {
        match self {
            Self::None => crate::server::record::AlarmSeverity::NoAlarm,
            Self::Minor => crate::server::record::AlarmSeverity::Minor,
            Self::Major => crate::server::record::AlarmSeverity::Major,
            Self::Invalid => crate::server::record::AlarmSeverity::Invalid,
        }
    }
}

/// EPICS busy record implementation.
///
/// A busy record is a binary output variant that tracks asynchronous operation
/// state. VAL=1 means busy, VAL=0 means done. Forward links fire only when
/// `val == 0 || oval == 0`, suppressing FLNK during sustained busy state (1→1).
#[derive(Debug, Clone)]
pub struct BusyRecord {
    // Primary value
    pub val: u16,
    pub oval: u16,
    // Enum labels
    pub znam: String,
    pub onam: String,
    // Timing
    pub high: f64,
    // Alarms
    pub zsv: AlarmSevr,
    pub osv: AlarmSevr,
    pub cosv: AlarmSevr,
    pub lalm: u16,
    // Invalid output
    pub ivoa: Ivoa,
    pub ivov: u16,
    // Output control
    pub omsl: Omsl,
    pub dol: String,
    // Monitoring
    pub mlst: u16,
    // Raw value (Phase B)
    pub rval: u32,
    pub oraw: u32,
    pub mask: u32,
    pub rbv: u32,
    pub orbv: u32,
    // HIGH timer state
    _high_active: bool,
    // Internal alarm state (set during process, used for IVOA check)
    nsev: AlarmSevr,
}

impl Default for BusyRecord {
    fn default() -> Self {
        Self {
            val: 0,
            oval: 0,
            znam: "Done".to_string(),
            onam: "Busy".to_string(),
            high: 0.0,
            zsv: AlarmSevr::None,
            osv: AlarmSevr::None,
            cosv: AlarmSevr::None,
            lalm: 0,
            ivoa: Ivoa::ContinueNormally,
            ivov: 0,
            omsl: Omsl::Supervisory,
            dol: String::new(),
            mlst: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            rbv: 0,
            orbv: 0,
            _high_active: false,
            nsev: AlarmSevr::None,
        }
    }
}

impl BusyRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convert VAL to RVAL using mask.
    fn convert_val_to_rval(&mut self) {
        if self.mask != 0 {
            self.rval = if self.val == 0 { 0 } else { self.mask };
        } else {
            self.rval = self.val as u32;
        }
    }

    /// Check alarms: UDF (handled by framework), STATE, COS.
    /// Sets internal nsev for IVOA check.
    fn check_alarms(&mut self) {
        let mut max_sev = AlarmSevr::None;

        // State alarm: val==0 → zsv, val!=0 → osv
        let state_sev = if self.val == 0 { self.zsv } else { self.osv };
        if (state_sev as u16) > (max_sev as u16) {
            max_sev = state_sev;
        }

        // COS alarm: val changed from lalm
        if self.val != self.lalm {
            if (self.cosv as u16) > (max_sev as u16) {
                max_sev = self.cosv;
            }
            self.lalm = self.val;
        }

        self.nsev = max_sev;
    }

    /// Update monitoring fields.
    fn monitor(&mut self) {
        if self.mlst != self.val {
            self.mlst = self.val;
        }
        self.oraw = self.rval;
        self.orbv = self.rbv;
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Enum,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Enum,
        read_only: true,
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
        name: "HIGH",
        dbf_type: DbFieldType::Double,
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
        name: "MLST",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: true,
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
        name: "RBV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ORBV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
];

impl Record for BusyRecord {
    fn record_type(&self) -> &'static str {
        "busy"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Step 1: DOL reading handled by framework (OMSL=ClosedLoop)

        // Step 2: VAL → RVAL conversion
        self.convert_val_to_rval();

        // Step 3: Save current VAL before write (for FLNK decision)
        self.oval = self.val;

        // Step 4: Check alarms
        self.check_alarms();

        // Step 5: IVOA handling
        // The framework handles IVOA for known record types, but "busy" is external.
        // We handle IVOA ourselves: if alarm severity is INVALID, apply IVOA policy.
        if self.nsev == AlarmSevr::Invalid {
            match self.ivoa {
                Ivoa::ContinueNormally => {
                    // proceed normally — write will happen via framework
                }
                Ivoa::DontDriveOutputs => {
                    // We can't prevent framework OUT write from here,
                    // but the framework's IVOA check won't fire for "busy".
                    // For now, this is a best-effort: the framework still writes OUT.
                    // Full IVOA support requires framework integration.
                }
                Ivoa::SetOutputToIvov => {
                    self.val = self.ivov;
                    self.convert_val_to_rval();
                }
            }
        }

        // Step 6: HIGH timer (Phase C — skip for now)

        // Step 7: Monitor
        self.monitor();

        // Step 8: FLNK handled by should_fire_forward_link()
        Ok(ProcessOutcome::complete())
    }

    fn should_fire_forward_link(&self) -> bool {
        self.val == 0 || self.oval == 0
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn is_put_complete(&self) -> bool {
        self.val == 0
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "OVAL" => Some(EpicsValue::Enum(self.oval)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "ZSV" => Some(EpicsValue::Short(self.zsv.into())),
            "OSV" => Some(EpicsValue::Short(self.osv.into())),
            "COSV" => Some(EpicsValue::Short(self.cosv.into())),
            "LALM" => Some(EpicsValue::Enum(self.lalm)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa.into())),
            "IVOV" => Some(EpicsValue::Enum(self.ivov)),
            "OMSL" => Some(EpicsValue::Short(self.omsl.into())),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
            "MLST" => Some(EpicsValue::Enum(self.mlst)),
            "RVAL" => Some(EpicsValue::Long(self.rval as i32)),
            "ORAW" => Some(EpicsValue::Long(self.oraw as i32)),
            "MASK" => Some(EpicsValue::Long(self.mask as i32)),
            "RBV" => Some(EpicsValue::Long(self.rbv as i32)),
            "ORBV" => Some(EpicsValue::Long(self.orbv as i32)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = match value {
                    EpicsValue::Enum(v) => v,
                    EpicsValue::Short(v) => v as u16,
                    EpicsValue::Long(v) => v as u16,
                    EpicsValue::Double(v) => v as u16,
                    EpicsValue::String(ref s) => {
                        if s.eq_ignore_ascii_case(&self.znam) {
                            0
                        } else if s.eq_ignore_ascii_case(&self.onam) {
                            1
                        } else {
                            s.parse::<u16>().unwrap_or(0)
                        }
                    }
                    _ => return Err(CaError::TypeMismatch(name.to_string())),
                };
                Ok(())
            }
            "ZNAM" => {
                if let EpicsValue::String(s) = value {
                    self.znam = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "ONAM" => {
                if let EpicsValue::String(s) = value {
                    self.onam = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "HIGH" => {
                if let EpicsValue::Double(v) = value {
                    self.high = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "ZSV" => {
                if let EpicsValue::Short(v) = value {
                    self.zsv = AlarmSevr::from(v);
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "OSV" => {
                if let EpicsValue::Short(v) = value {
                    self.osv = AlarmSevr::from(v);
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "COSV" => {
                if let EpicsValue::Short(v) = value {
                    self.cosv = AlarmSevr::from(v);
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = Ivoa::from(v);
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "IVOV" => {
                self.ivov = match value {
                    EpicsValue::Enum(v) => v,
                    EpicsValue::Short(v) => v as u16,
                    _ => return Err(CaError::TypeMismatch(name.to_string())),
                };
                Ok(())
            }
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = Omsl::from(v);
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "DOL" => {
                if let EpicsValue::String(s) = value {
                    self.dol = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch(name.to_string()))
                }
            }
            "MASK" => {
                self.mask = match value {
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch(name.to_string())),
                };
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        let rec = BusyRecord::default();
        assert_eq!(rec.val, 0);
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.znam, "Done");
        assert_eq!(rec.onam, "Busy");
        assert_eq!(rec.high, 0.0);
        assert_eq!(rec.zsv, AlarmSevr::None);
        assert_eq!(rec.osv, AlarmSevr::None);
        assert_eq!(rec.cosv, AlarmSevr::None);
        assert_eq!(rec.ivoa, Ivoa::ContinueNormally);
        assert_eq!(rec.omsl, Omsl::Supervisory);
        assert_eq!(rec.mlst, 0);
        assert_eq!(rec.mask, 0);
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_record_type() {
        let rec = BusyRecord::new();
        assert_eq!(rec.record_type(), "busy");
    }

    #[test]
    fn test_can_device_write() {
        let rec = BusyRecord::new();
        assert!(rec.can_device_write());
    }

    #[test]
    fn test_get_put_field_val() {
        let mut rec = BusyRecord::new();
        rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.val, 1);

        rec.put_field("VAL", EpicsValue::Short(0)).unwrap();
        assert_eq!(rec.val, 0);

        rec.put_field("VAL", EpicsValue::Double(1.0)).unwrap();
        assert_eq!(rec.val, 1);
    }

    #[test]
    fn test_get_put_field_roundtrip() {
        let mut rec = BusyRecord::new();

        // String fields
        rec.put_field("ZNAM", EpicsValue::String("Idle".to_string()))
            .unwrap();
        assert_eq!(
            rec.get_field("ZNAM"),
            Some(EpicsValue::String("Idle".to_string()))
        );

        rec.put_field("ONAM", EpicsValue::String("Active".to_string()))
            .unwrap();
        assert_eq!(
            rec.get_field("ONAM"),
            Some(EpicsValue::String("Active".to_string()))
        );

        // Double field
        rec.put_field("HIGH", EpicsValue::Double(2.5)).unwrap();
        assert_eq!(rec.get_field("HIGH"), Some(EpicsValue::Double(2.5)));

        // Short fields (enums)
        rec.put_field("ZSV", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("ZSV"), Some(EpicsValue::Short(1)));

        rec.put_field("OSV", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("OSV"), Some(EpicsValue::Short(2)));

        rec.put_field("COSV", EpicsValue::Short(3)).unwrap();
        assert_eq!(rec.get_field("COSV"), Some(EpicsValue::Short(3)));

        rec.put_field("IVOA", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("IVOA"), Some(EpicsValue::Short(1)));

        rec.put_field("IVOV", EpicsValue::Enum(1)).unwrap();
        assert_eq!(rec.get_field("IVOV"), Some(EpicsValue::Enum(1)));

        rec.put_field("OMSL", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(1)));

        rec.put_field("DOL", EpicsValue::String("some:link".to_string()))
            .unwrap();
        assert_eq!(
            rec.get_field("DOL"),
            Some(EpicsValue::String("some:link".to_string()))
        );

        rec.put_field("MASK", EpicsValue::Long(0xFF)).unwrap();
        assert_eq!(rec.get_field("MASK"), Some(EpicsValue::Long(0xFF)));
    }

    #[test]
    fn test_enum_str() {
        let mut rec = BusyRecord::new();

        // String "Done" → val=0
        rec.put_field("VAL", EpicsValue::String("Done".to_string()))
            .unwrap();
        assert_eq!(rec.val, 0);

        // String "Busy" → val=1
        rec.put_field("VAL", EpicsValue::String("Busy".to_string()))
            .unwrap();
        assert_eq!(rec.val, 1);

        // Case insensitive
        rec.put_field("VAL", EpicsValue::String("done".to_string()))
            .unwrap();
        assert_eq!(rec.val, 0);

        rec.put_field("VAL", EpicsValue::String("busy".to_string()))
            .unwrap();
        assert_eq!(rec.val, 1);

        // Custom ZNAM/ONAM
        rec.znam = "Off".to_string();
        rec.onam = "On".to_string();
        rec.put_field("VAL", EpicsValue::String("Off".to_string()))
            .unwrap();
        assert_eq!(rec.val, 0);
        rec.put_field("VAL", EpicsValue::String("On".to_string()))
            .unwrap();
        assert_eq!(rec.val, 1);
    }

    // --- process() tests ---

    #[test]
    fn test_process_updates_oval() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.oval, 1);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.oval, 0);
    }

    #[test]
    fn test_mask_conversion() {
        let mut rec = BusyRecord::new();
        rec.mask = 0xFF;
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.rval, 0xFF);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_mask_zero_passthrough() {
        let mut rec = BusyRecord::new();
        rec.mask = 0;
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.rval, 1);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_state_alarm_zsv() {
        let mut rec = BusyRecord::new();
        rec.zsv = AlarmSevr::Minor;
        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.nsev, AlarmSevr::Minor);
    }

    #[test]
    fn test_state_alarm_osv() {
        let mut rec = BusyRecord::new();
        rec.osv = AlarmSevr::Major;
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.nsev, AlarmSevr::Major);
    }

    #[test]
    fn test_cos_alarm() {
        let mut rec = BusyRecord::new();
        rec.cosv = AlarmSevr::Minor;
        rec.lalm = 0;
        rec.val = 1; // changed from lalm=0
        rec.process().unwrap();
        // COS alarm should fire and update lalm
        assert_eq!(rec.lalm, 1);

        // Process again with same val — no COS change
        rec.process().unwrap();
        assert_eq!(rec.lalm, 1); // unchanged
    }

    #[test]
    fn test_cos_alarm_severity() {
        let mut rec = BusyRecord::new();
        rec.cosv = AlarmSevr::Major;
        rec.osv = AlarmSevr::Minor;
        rec.lalm = 0;
        rec.val = 1;
        rec.process().unwrap();
        // COS (Major) > OSV (Minor), so nsev should be Major
        assert_eq!(rec.nsev, AlarmSevr::Major);
    }

    #[test]
    fn test_monitor_mlst() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.mlst, 1);

        // Same val — mlst stays
        rec.process().unwrap();
        assert_eq!(rec.mlst, 1);

        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.mlst, 0);
    }

    // --- FLNK semantics tests ---

    #[test]
    fn test_flnk_0_to_1() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.oval = 0;
        assert!(rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_1_to_1() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.oval = 1;
        assert!(!rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_1_to_0() {
        let mut rec = BusyRecord::new();
        rec.val = 0;
        rec.oval = 1;
        assert!(rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_0_to_0() {
        let mut rec = BusyRecord::new();
        rec.val = 0;
        rec.oval = 0;
        assert!(rec.should_fire_forward_link());
    }

    // --- FLNK after process() ---

    #[test]
    fn test_flnk_after_process_busy_start() {
        let mut rec = BusyRecord::new();
        rec.val = 1;
        rec.process().unwrap();
        // After process: val=1, oval=1 (set during process)
        // But FLNK decision in C code uses oval saved *before* write.
        // In our impl, oval is set to val at process start, so oval=1.
        // 0→1 transition: we need to check the val/oval after process.
        // oval was set to val (1) during process, so val=1, oval=1 → false.
        // Wait — the C code saves oval=val BEFORE write, meaning before device
        // support might change val. In our pure record process, val doesn't change
        // during write. So for a simple 0→1 put: val=1, oval=1 after process.
        // FLNK = val==0 || oval==0 → false.
        //
        // But in C code line 271: if val==0 || oval==0 → fire FLNK.
        // For the transition 0→1:
        //   Before process: val=1 (just put), oval=0 (from last process)
        //   Process starts: oval = val = 1
        //   After process: val=1, oval=1 → FLNK = false
        //
        // Hmm, but the plan says 0→1 should fire FLNK (oval=0).
        // The key insight: oval is NOT set in the current process, it was set
        // in the PREVIOUS process cycle. Let me re-read the C code...
        //
        // Actually re-reading C code line 220: prec->oval = prec->val
        // This saves the current val into oval. So when we PUT val=1:
        //   process(): oval = val = 1
        //   FLNK check: val=1, oval=1 → false
        //
        // But the FIRST time val transitions 0→1, what was oval before?
        // It was 0 from the previous process (or default).
        // Wait — line 220 sets oval = val at the START of each process.
        // So oval always equals val at FLNK check time... unless async
        // device support changes val after oval is saved (line 220 is before write).
        //
        // For the synchronous case (no async device support), the plan's table
        // describes the state ENTERING process, not after. The actual FLNK check
        // uses the values AT CHECK TIME:
        //   val=1 (unchanged), oval=1 (just saved) → false
        //
        // This means for synchronous device support, FLNK only fires when val==0.
        // The oval==0 case handles async: device support sets val=1 while
        // oval was saved as 0.
        //
        // For our tests, just verify the process() behavior directly.
        assert_eq!(rec.val, 1);
        assert_eq!(rec.oval, 1);
        // val=1, oval=1 → FLNK = false (correct for sync)
        assert!(!rec.should_fire_forward_link());
    }

    #[test]
    fn test_flnk_after_process_done() {
        let mut rec = BusyRecord::new();
        // Simulate: was busy, now done
        rec.val = 0;
        rec.oval = 1; // from previous process where val was 1
        rec.process().unwrap();
        // After process: oval = val = 0
        assert_eq!(rec.val, 0);
        assert_eq!(rec.oval, 0);
        // val=0 → FLNK fires
        assert!(rec.should_fire_forward_link());
    }

    // --- IVOA tests ---

    #[test]
    fn test_ivoa_continue() {
        let mut rec = BusyRecord::new();
        rec.ivoa = Ivoa::ContinueNormally;
        rec.zsv = AlarmSevr::Invalid;
        rec.val = 0;
        rec.process().unwrap();
        // Should process normally
        assert_eq!(rec.val, 0);
    }

    #[test]
    fn test_ivoa_dont_drive() {
        let mut rec = BusyRecord::new();
        rec.ivoa = Ivoa::DontDriveOutputs;
        rec.zsv = AlarmSevr::Invalid;
        rec.val = 0;
        rec.process().unwrap();
        // Val unchanged (best-effort, framework OUT write not blocked from record)
        assert_eq!(rec.val, 0);
    }

    #[test]
    fn test_ivoa_set_ivov() {
        let mut rec = BusyRecord::new();
        rec.ivoa = Ivoa::SetOutputToIvov;
        rec.ivov = 0;
        rec.osv = AlarmSevr::Invalid; // val=1 → Invalid alarm
        rec.val = 1;
        rec.process().unwrap();
        // IVOA=SetOutputToIvov + Invalid alarm → val set to ivov=0
        assert_eq!(rec.val, 0);
        assert_eq!(rec.rval, 0);
    }

    #[test]
    fn test_ivoa_no_effect_without_invalid() {
        let mut rec = BusyRecord::new();
        rec.ivoa = Ivoa::SetOutputToIvov;
        rec.ivov = 0;
        rec.osv = AlarmSevr::Minor; // Not Invalid
        rec.val = 1;
        rec.process().unwrap();
        // No IVOA effect since alarm is not Invalid
        assert_eq!(rec.val, 1);
    }

    // --- State transition cycle ---

    #[test]
    fn test_state_transition_cycle() {
        let mut rec = BusyRecord::new();

        // Start idle
        assert_eq!(rec.val, 0);
        rec.process().unwrap();
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.mlst, 0);

        // Go busy
        rec.val = 1;
        rec.process().unwrap();
        assert_eq!(rec.oval, 1);
        assert_eq!(rec.mlst, 1);
        assert_eq!(rec.rval, 1);

        // Stay busy (re-process)
        rec.process().unwrap();
        assert_eq!(rec.oval, 1);
        assert!(!rec.should_fire_forward_link());

        // Go done
        rec.val = 0;
        rec.process().unwrap();
        assert_eq!(rec.oval, 0);
        assert_eq!(rec.mlst, 0);
        assert_eq!(rec.rval, 0);
        assert!(rec.should_fire_forward_link());
    }
}
