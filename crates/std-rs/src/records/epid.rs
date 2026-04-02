use std::any::Any;
use std::time::Instant;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Feedback mode for the epid record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i16)]
pub enum FeedbackMode {
    #[default]
    Pid = 0,
    MaxMin = 1,
}

impl From<i16> for FeedbackMode {
    fn from(v: i16) -> Self {
        match v {
            1 => FeedbackMode::MaxMin,
            _ => FeedbackMode::Pid,
        }
    }
}

/// Feedback on/off state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i16)]
pub enum FeedbackState {
    #[default]
    Off = 0,
    On = 1,
}

impl From<i16> for FeedbackState {
    fn from(v: i16) -> Self {
        match v {
            1 => FeedbackState::On,
            _ => FeedbackState::Off,
        }
    }
}

/// Extended PID feedback control record.
///
/// Ported from EPICS std module `epidRecord.c`.
/// Supports PID and Max/Min feedback modes with anti-windup,
/// bumpless turn-on, output deadband, and hysteresis-based alarms.
pub struct EpidRecord {
    // --- PID control ---
    /// Setpoint (VAL)
    pub val: f64,
    /// Setpoint mode: 0=supervisory, 1=closed_loop (SMSL)
    pub smsl: i16,
    /// Setpoint input link (STPL) — resolved by framework
    pub stpl: String,
    /// Controlled value input link (INP) — resolved by framework
    pub inp: String,
    /// Output link (OUTL) — resolved by framework
    pub outl: String,
    /// Readback trigger link (TRIG)
    pub trig: String,
    /// Trigger value (TVAL)
    pub tval: f64,
    /// Controlled value (CVAL), read-only
    pub cval: f64,
    /// Previous controlled value (CVLP), read-only
    pub cvlp: f64,
    /// Output value (OVAL), read-only
    pub oval: f64,
    /// Previous output value (OVLP), read-only
    pub ovlp: f64,
    /// Proportional gain (KP)
    pub kp: f64,
    /// Integral gain — repeats per second (KI)
    pub ki: f64,
    /// Derivative gain (KD)
    pub kd: f64,
    /// Proportional component (P), read-only
    pub p: f64,
    /// Previous P (PP), read-only
    pub pp: f64,
    /// Integral component (I), writable for bumpless init
    pub i: f64,
    /// Previous I (IP)
    pub ip: f64,
    /// Derivative component (D), read-only
    pub d: f64,
    /// Previous D (DP), read-only
    pub dp: f64,
    /// Error = setpoint - controlled value (ERR), read-only
    pub err: f64,
    /// Previous error (ERRP), read-only
    pub errp: f64,
    /// Delta time in seconds (DT), writable for fast mode
    pub dt: f64,
    /// Previous delta time (DTP)
    pub dtp: f64,
    /// Minimum delta time between calculations (MDT)
    pub mdt: f64,
    /// Feedback mode: PID or MaxMin (FMOD)
    pub fmod: i16,
    /// Feedback on/off (FBON)
    pub fbon: i16,
    /// Previous feedback on/off (FBOP)
    pub fbop: i16,
    /// Output deadband (ODEL)
    pub odel: f64,

    // --- Display ---
    /// Display precision (PREC)
    pub prec: i16,
    /// Engineering units (EGU)
    pub egu: String,
    /// High operating range (HOPR)
    pub hopr: f64,
    /// Low operating range (LOPR)
    pub lopr: f64,
    /// High drive limit (DRVH)
    pub drvh: f64,
    /// Low drive limit (DRVL)
    pub drvl: f64,

    // --- Alarm ---
    /// Hihi deviation limit (HIHI)
    pub hihi: f64,
    /// Lolo deviation limit (LOLO)
    pub lolo: f64,
    /// High deviation limit (HIGH)
    pub high: f64,
    /// Low deviation limit (LOW)
    pub low: f64,
    /// Hihi severity (HHSV)
    pub hhsv: i16,
    /// Lolo severity (LLSV)
    pub llsv: i16,
    /// High severity (HSV)
    pub hsv: i16,
    /// Low severity (LSV)
    pub lsv: i16,
    /// Alarm deadband / hysteresis (HYST)
    pub hyst: f64,
    /// Last value alarmed (LALM), read-only
    pub lalm: f64,

    // --- Monitor deadband ---
    /// Archive deadband (ADEL)
    pub adel: f64,
    /// Monitor deadband (MDEL)
    pub mdel: f64,
    /// Last value archived (ALST), read-only
    pub alst: f64,
    /// Last value monitored (MLST), read-only
    pub mlst: f64,

    // --- Internal time tracking ---
    /// Current time (CT) — used for delta-T computation
    pub(crate) ct: Instant,
    /// Previous time (CTP) — tracked for monitor change detection
    #[allow(dead_code)]
    pub(crate) ctp: Instant,

    // --- Internal flags ---
    /// Set by the framework (via set_device_did_compute) to indicate
    /// device support's read() already performed the PID computation.
    /// process() checks this to avoid running the built-in PID a second time.
    device_did_compute: bool,
}

impl Default for EpidRecord {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            val: 0.0,
            smsl: 0,
            stpl: String::new(),
            inp: String::new(),
            outl: String::new(),
            trig: String::new(),
            tval: 0.0,
            cval: 0.0,
            cvlp: 0.0,
            oval: 0.0,
            ovlp: 0.0,
            kp: 0.0,
            ki: 0.0,
            kd: 0.0,
            p: 0.0,
            pp: 0.0,
            i: 0.0,
            ip: 0.0,
            d: 0.0,
            dp: 0.0,
            err: 0.0,
            errp: 0.0,
            dt: 0.0,
            dtp: 0.0,
            mdt: 0.0,
            fmod: 0,
            fbon: 0,
            fbop: 0,
            odel: 0.0,
            prec: 0,
            egu: String::new(),
            hopr: 0.0,
            lopr: 0.0,
            drvh: 0.0,
            drvl: 0.0,
            hihi: 0.0,
            lolo: 0.0,
            high: 0.0,
            low: 0.0,
            hhsv: 0,
            llsv: 0,
            hsv: 0,
            lsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            ct: now,
            ctp: now,
            device_did_compute: false,
        }
    }
}

impl EpidRecord {
    /// Check alarms using hysteresis-based threshold comparison on VAL.
    /// Ported from epidRecord.c `checkAlarms()`.
    pub fn check_alarms(&mut self) -> Option<(u16, u16)> {
        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        // HIHI alarm
        if self.hhsv != 0 {
            if val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst) {
                self.lalm = self.hihi;
                return Some((3, self.hhsv as u16)); // HIHI_ALARM
            }
        }

        // LOLO alarm
        if self.llsv != 0 {
            if val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst) {
                self.lalm = self.lolo;
                return Some((4, self.llsv as u16)); // LOLO_ALARM
            }
        }

        // HIGH alarm
        if self.hsv != 0 {
            if val >= self.high || (lalm == self.high && val >= self.high - hyst) {
                self.lalm = self.high;
                return Some((1, self.hsv as u16)); // HIGH_ALARM
            }
        }

        // LOW alarm
        if self.lsv != 0 {
            if val <= self.low || (lalm == self.low && val <= self.low + hyst) {
                self.lalm = self.low;
                return Some((2, self.lsv as u16)); // LOW_ALARM
            }
        }

        // No alarm
        self.lalm = val;
        None
    }

    /// Update monitor tracking fields. Returns list of fields that changed.
    /// Ported from epidRecord.c `monitor()`.
    pub fn update_monitors(&mut self) {
        // Update previous-value fields for change detection
        self.ovlp = self.oval;
        self.pp = self.p;
        self.ip = self.i;
        self.dp = self.d;
        self.dtp = self.dt;
        self.errp = self.err;
        self.cvlp = self.cval;

        // VAL deadband tracking
        if self.mdel == 0.0 || (self.mlst - self.val).abs() > self.mdel {
            self.mlst = self.val;
        }
        if self.adel == 0.0 || (self.alst - self.val).abs() > self.adel {
            self.alst = self.val;
        }
    }
}

static FIELDS: &[FieldDesc] = &[
    // PID control
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "STPL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "TRIG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "TVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "CVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "CVLP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "OVLP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "KP",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "KI",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "KD",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "P",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "I",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "IP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "D",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "DP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ERR",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ERRP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "DT",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DTP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MDT",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "FMOD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FBON",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FBOP",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "ODEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Display
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
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
    // Alarm
    FieldDesc {
        name: "HIHI",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HIGH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOW",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HHSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LLSV",
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
        name: "HYST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    // Monitor deadband
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
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

impl Record for EpidRecord {
    fn record_type(&self) -> &'static str {
        "epid"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // In the C code, process() always calls pdset->do_pid() — a custom
        // device support function unique to the epid record. In Rust, the
        // framework has a generic DeviceSupport trait with read()/write()
        // and no custom function pointers.
        //
        // For non-"Soft Channel" DTYPs (e.g. "Fast Epid"), the framework
        // calls DeviceSupport::read() BEFORE process(). That read() runs
        // the driver-specific PID and sets pid_done = true.
        //
        // For "Soft Channel" or no device support, the framework skips
        // read(), so pid_done stays false and process() runs the built-in
        // PID here.
        if !self.device_did_compute {
            crate::device_support::epid_soft::EpidSoftDeviceSupport::do_pid(self);
        }
        self.device_did_compute = false; // Reset for next cycle

        self.check_alarms();
        self.update_monitors();

        // Device support actions are now merged by the framework
        let actions = Vec::new();
        Ok(ProcessOutcome::complete_with(actions))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SMSL" => Some(EpicsValue::Short(self.smsl)),
            "STPL" => Some(EpicsValue::String(self.stpl.clone())),
            "INP" => Some(EpicsValue::String(self.inp.clone())),
            "OUTL" => Some(EpicsValue::String(self.outl.clone())),
            "TRIG" => Some(EpicsValue::String(self.trig.clone())),
            "TVAL" => Some(EpicsValue::Double(self.tval)),
            "CVAL" => Some(EpicsValue::Double(self.cval)),
            "CVLP" => Some(EpicsValue::Double(self.cvlp)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OVLP" => Some(EpicsValue::Double(self.ovlp)),
            "KP" => Some(EpicsValue::Double(self.kp)),
            "KI" => Some(EpicsValue::Double(self.ki)),
            "KD" => Some(EpicsValue::Double(self.kd)),
            "P" => Some(EpicsValue::Double(self.p)),
            "PP" => Some(EpicsValue::Double(self.pp)),
            "I" => Some(EpicsValue::Double(self.i)),
            "IP" => Some(EpicsValue::Double(self.ip)),
            "D" => Some(EpicsValue::Double(self.d)),
            "DP" => Some(EpicsValue::Double(self.dp)),
            "ERR" => Some(EpicsValue::Double(self.err)),
            "ERRP" => Some(EpicsValue::Double(self.errp)),
            "DT" => Some(EpicsValue::Double(self.dt)),
            "DTP" => Some(EpicsValue::Double(self.dtp)),
            "MDT" => Some(EpicsValue::Double(self.mdt)),
            "FMOD" => Some(EpicsValue::Short(self.fmod)),
            "FBON" => Some(EpicsValue::Short(self.fbon)),
            "FBOP" => Some(EpicsValue::Short(self.fbop)),
            "ODEL" => Some(EpicsValue::Double(self.odel)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "DRVH" => Some(EpicsValue::Double(self.drvh)),
            "DRVL" => Some(EpicsValue::Double(self.drvl)),
            "HIHI" => Some(EpicsValue::Double(self.hihi)),
            "LOLO" => Some(EpicsValue::Double(self.lolo)),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "LOW" => Some(EpicsValue::Double(self.low)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SMSL" => match value {
                EpicsValue::Short(v) => {
                    self.smsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "STPL" => match value {
                EpicsValue::String(v) => {
                    self.stpl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "INP" => match value {
                EpicsValue::String(v) => {
                    self.inp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OUTL" => match value {
                EpicsValue::String(v) => {
                    self.outl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TRIG" => match value {
                EpicsValue::String(v) => {
                    self.trig = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TVAL" => match value {
                EpicsValue::Double(v) => {
                    self.tval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "KP" => match value {
                EpicsValue::Double(v) => {
                    self.kp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "KI" => match value {
                EpicsValue::Double(v) => {
                    self.ki = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "KD" => match value {
                EpicsValue::Double(v) => {
                    self.kd = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "I" => match value {
                EpicsValue::Double(v) => {
                    self.i = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "IP" => match value {
                EpicsValue::Double(v) => {
                    self.ip = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DT" => match value {
                EpicsValue::Double(v) => {
                    self.dt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDT" => match value {
                EpicsValue::Double(v) => {
                    self.mdt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "FMOD" => match value {
                EpicsValue::Short(v) => {
                    self.fmod = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "FBON" => match value {
                EpicsValue::Short(v) => {
                    self.fbon = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ODEL" => match value {
                EpicsValue::Double(v) => {
                    self.odel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGU" => match value {
                EpicsValue::String(v) => {
                    self.egu = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HOPR" => match value {
                EpicsValue::Double(v) => {
                    self.hopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOPR" => match value {
                EpicsValue::Double(v) => {
                    self.lopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVH" => match value {
                EpicsValue::Double(v) => {
                    self.drvh = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVL" => match value {
                EpicsValue::Double(v) => {
                    self.drvl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HIHI" => match value {
                EpicsValue::Double(v) => {
                    self.hihi = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOLO" => match value {
                EpicsValue::Double(v) => {
                    self.lolo = v;
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
            "LOW" => match value {
                EpicsValue::Double(v) => {
                    self.low = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HHSV" => match value {
                EpicsValue::Short(v) => {
                    self.hhsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LLSV" => match value {
                EpicsValue::Short(v) => {
                    self.llsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HSV" => match value {
                EpicsValue::Short(v) => {
                    self.hsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LSV" => match value {
                EpicsValue::Short(v) => {
                    self.lsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HYST" => match value {
                EpicsValue::Double(v) => {
                    self.hyst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ADEL" => match value {
                EpicsValue::Double(v) => {
                    self.adel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDEL" => match value {
                EpicsValue::Double(v) => {
                    self.mdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // Read-only fields
            "CVAL" | "CVLP" | "OVAL" | "OVLP" | "P" | "PP" | "D" | "DP" | "ERR" | "ERRP"
            | "DTP" | "FBOP" | "LALM" | "ALST" | "MLST" => Err(CaError::ReadOnlyField(name.into())),
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }

    fn set_device_did_compute(&mut self, did_compute: bool) {
        self.device_did_compute = did_compute;
    }

    fn put_field_internal(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> epics_base_rs::error::CaResult<()> {
        // Bypass read-only checks for framework-internal writes (ReadDbLink).
        // This allows the framework to write to CVAL, OVAL, etc. from link resolution.
        match name {
            "CVAL" => match value {
                EpicsValue::Double(v) => {
                    self.cval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "P" => match value {
                EpicsValue::Double(v) => {
                    self.p = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "D" => match value {
                EpicsValue::Double(v) => {
                    self.d = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ERR" => match value {
                EpicsValue::Double(v) => {
                    self.err = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => self.put_field(name, value),
        }
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        // INP -> CVAL is always resolved.
        // STPL -> VAL is only resolved when SMSL == closed_loop (1).
        // In supervisory mode (SMSL=0), the operator sets VAL directly
        // and STPL must not overwrite it.
        if self.smsl == 1 {
            // closed_loop: fetch setpoint from STPL into VAL
            static WITH_STPL: &[(&str, &str)] = &[("STPL", "VAL"), ("INP", "CVAL")];
            WITH_STPL
        } else {
            // supervisory: VAL is set by operator, don't fetch STPL
            static WITHOUT_STPL: &[(&str, &str)] = &[("INP", "CVAL")];
            WITHOUT_STPL
        }
    }

    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        // OUTL -> OVAL (output link)
        static LINKS: &[(&str, &str)] = &[("OUTL", "OVAL")];
        LINKS
    }
}
