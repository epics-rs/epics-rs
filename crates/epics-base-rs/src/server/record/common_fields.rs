use std::time::SystemTime;

use super::alarm::{AlarmSeverity, AnalogAlarmConfig};
use super::scan::ScanType;

/// Common fields shared by all records.
#[derive(Clone, Debug)]
pub struct CommonFields {
    // Alarm state (current/result)
    pub sevr: AlarmSeverity,
    pub stat: u16,
    /// Alarm message string (current). Mirrors epics-base PR #568:
    /// records may attach a human-readable explanation alongside
    /// `stat`/`sevr`. Empty means "no message". Transferred from
    /// `namsg` by `rec_gbl_reset_alarms`.
    pub amsg: String,
    // New alarm state (pending, transferred by rec_gbl_reset_alarms)
    pub nsev: AlarmSeverity,
    pub nsta: u16,
    /// Pending alarm message — set during process(), transferred to
    /// `amsg` by `rec_gbl_reset_alarms` (epics-base PR #566).
    pub namsg: String,
    // Alarm acknowledgement
    pub acks: AlarmSeverity,
    pub ackt: bool,
    pub udf: bool,
    pub udfs: AlarmSeverity,
    // Scan
    pub scan: ScanType,
    pub sscn: ScanType,
    pub pini: bool,
    pub tpro: bool,
    pub bkpt: u8,
    // Links (raw strings)
    pub flnk: String,
    pub inp: String,
    pub out: String,
    // Device
    pub dtyp: String,
    // Timestamp
    pub time: SystemTime,
    pub tse: i16,
    pub tsel: String,
    /// Time-tag — C `dbCommon.dbd.pod` `field(UTAG,DBF_UINT64)`. A
    /// 64-bit user/hardware tag set alongside `time` by
    /// `recGblGetTimeStampSimm` via `dbGetTimeStampTag`. Zero when no
    /// time-tag source is configured.
    pub utag: u64,
    // Analog alarm config (Some for analog record types)
    pub analog_alarm: Option<AnalogAlarmConfig>,
    // Access security group
    pub asg: String,
    /// Access security level. Round-33A (R33-G4): C `dbCommon.ASL`
    /// (0 or 1, default 0). Compared against `RULE(N, …)` levels
    /// in [`crate::server::access_security::check_access_method`] —
    /// a rule with `RULE(M, …)` only applies when `ASL ≤ M`. The
    /// pre-Round-33A code hard-coded ASL=0 at every ACF call site
    /// (CA tcp.rs, PVA native_source GET/PUT/MONITOR), so every
    /// `RULE(N>0, WRITE)` was always considered "applicable" and
    /// the per-record ASL gate was silently inert.
    pub asl: u8,
    // Description (moved from individual records)
    pub desc: String,
    // Phase/priority/event
    pub phas: i16,
    /// Event name for `SCAN="Event"` records. C `dbCommon.dbd.pod`:
    /// `field(EVNT,DBF_STRING) { size(40) }` — since EPICS 7 this is
    /// an event *name* (resolved by `eventNameToHandle`), not a
    /// numeric subscript. A numeric string ("5") still works for
    /// backward compatibility. Empty means "no event".
    pub evnt: String,
    pub prio: i16,
    // Disable support
    pub disv: i16,
    pub disa: i16,
    pub sdis: String,
    pub diss: AlarmSeverity,
    // Alarm hysteresis (analog records)
    pub hyst: f64,
    // Lock count (re-entrance counter)
    pub lcnt: i16,
    // Disable putfield from CA (default false)
    pub disp: bool,
    // Process control
    pub putf: bool,
    pub rpro: bool,
    // Fallback monitor/archive last-sent values for records without MLST/ALST fields
    pub mlst: Option<f64>,
    pub alst: Option<f64>,
}

impl CommonFields {
    /// Build a [`ProcessContext`](super::record_trait::ProcessContext)
    /// snapshot of the framework-owned state a record's `process()` or
    /// device support's `read()` needs to observe during the cycle.
    pub fn process_context(&self) -> super::record_trait::ProcessContext {
        super::record_trait::ProcessContext {
            udf: self.udf,
            udfs: self.udfs,
            phas: self.phas,
            tse: self.tse,
            tsel: self.tsel.clone(),
            dtyp: self.dtyp.clone(),
        }
    }
}

impl Default for CommonFields {
    fn default() -> Self {
        Self {
            sevr: AlarmSeverity::NoAlarm,
            stat: 0,
            amsg: String::new(),
            nsev: AlarmSeverity::NoAlarm,
            nsta: 0,
            namsg: String::new(),
            acks: AlarmSeverity::NoAlarm,
            ackt: true,
            udf: true,
            udfs: AlarmSeverity::Invalid,
            scan: ScanType::Passive,
            sscn: ScanType::Passive,
            pini: false,
            tpro: false,
            bkpt: 0,
            flnk: String::new(),
            inp: String::new(),
            out: String::new(),
            dtyp: String::new(),
            time: SystemTime::UNIX_EPOCH,
            tse: 0,
            tsel: String::new(),
            utag: 0,
            analog_alarm: None,
            asg: "DEFAULT".to_string(),
            asl: 0,
            desc: String::new(),
            phas: 0,
            evnt: String::new(),
            prio: 0,
            disv: 1,
            disa: 0,
            sdis: String::new(),
            diss: AlarmSeverity::NoAlarm,
            hyst: 0.0,
            lcnt: 0,
            disp: false,
            putf: false,
            rpro: false,
            mlst: None,
            alst: None,
        }
    }
}
