use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::SystemTime;

use crate::runtime::sync::mpsc;

use crate::error::{CaError, CaResult};
use crate::server::pv::{MonitorEvent, Subscriber};
use crate::types::{DbFieldType, EpicsValue};

/// Metadata describing a single field in a record.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    pub name: &'static str,
    pub dbf_type: DbFieldType,
    pub read_only: bool,
}

/// Alarm severity levels matching EPICS base.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u16)]
pub enum AlarmSeverity {
    #[default]
    NoAlarm = 0,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl AlarmSeverity {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::NoAlarm,
            1 => Self::Minor,
            2 => Self::Major,
            3 => Self::Invalid,
            _ => Self::Invalid,
        }
    }
}

/// Scan types matching EPICS base SCAN field menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
#[repr(u16)]
pub enum ScanType {
    #[default]
    Passive = 0,
    Event = 1,
    IoIntr = 2,
    Sec10 = 3,
    Sec5 = 4,
    Sec2 = 5,
    Sec1 = 6,
    Sec05 = 7,
    Sec02 = 8,
    Sec01 = 9,
}

impl ScanType {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Passive,
            1 => Self::Event,
            2 => Self::IoIntr,
            3 => Self::Sec10,
            4 => Self::Sec5,
            5 => Self::Sec2,
            6 => Self::Sec1,
            7 => Self::Sec05,
            8 => Self::Sec02,
            9 => Self::Sec01,
            _ => Self::Passive,
        }
    }

    pub fn from_str(s: &str) -> CaResult<Self> {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "passive" => Ok(Self::Passive),
            "event" => Ok(Self::Event),
            "i/o intr" | "iointr" => Ok(Self::IoIntr),
            "10 second" => Ok(Self::Sec10),
            "5 second" => Ok(Self::Sec5),
            "2 second" => Ok(Self::Sec2),
            "1 second" => Ok(Self::Sec1),
            ".5 second" | "0.5 second" => Ok(Self::Sec05),
            ".2 second" | "0.2 second" => Ok(Self::Sec02),
            ".1 second" | "0.1 second" => Ok(Self::Sec01),
            other => {
                if let Ok(v) = other.parse::<u16>() {
                    Ok(Self::from_u16(v))
                } else {
                    Err(CaError::InvalidValue(format!("unknown scan type: '{s}'")))
                }
            }
        }
    }

    /// Return the interval duration for periodic scan types.
    pub fn interval(&self) -> Option<std::time::Duration> {
        match self {
            Self::Sec10 => Some(std::time::Duration::from_secs(10)),
            Self::Sec5 => Some(std::time::Duration::from_secs(5)),
            Self::Sec2 => Some(std::time::Duration::from_secs(2)),
            Self::Sec1 => Some(std::time::Duration::from_secs(1)),
            Self::Sec05 => Some(std::time::Duration::from_millis(500)),
            Self::Sec02 => Some(std::time::Duration::from_millis(200)),
            Self::Sec01 => Some(std::time::Duration::from_millis(100)),
            _ => None,
        }
    }
}

impl std::fmt::Display for ScanType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passive => write!(f, "Passive"),
            Self::Event => write!(f, "Event"),
            Self::IoIntr => write!(f, "I/O Intr"),
            Self::Sec10 => write!(f, "10 second"),
            Self::Sec5 => write!(f, "5 second"),
            Self::Sec2 => write!(f, "2 second"),
            Self::Sec1 => write!(f, "1 second"),
            Self::Sec05 => write!(f, ".5 second"),
            Self::Sec02 => write!(f, ".2 second"),
            Self::Sec01 => write!(f, ".1 second"),
        }
    }
}

/// Link processing policy for input/output links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkProcessPolicy {
    NoProcess,
    #[default]
    ProcessPassive,
    /// CP: subscribe to source; when source changes, process this record.
    ChannelProcess,
}

/// Analog alarm configuration — only for ai/ao/longin/longout.
#[derive(Clone, Debug)]
pub struct AnalogAlarmConfig {
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: AlarmSeverity,
    pub hsv: AlarmSeverity,
    pub lsv: AlarmSeverity,
    pub llsv: AlarmSeverity,
}

impl Default for AnalogAlarmConfig {
    fn default() -> Self {
        Self {
            hihi: 0.0,
            high: 0.0,
            low: 0.0,
            lolo: 0.0,
            hhsv: AlarmSeverity::NoAlarm,
            hsv: AlarmSeverity::NoAlarm,
            lsv: AlarmSeverity::NoAlarm,
            llsv: AlarmSeverity::NoAlarm,
        }
    }
}

/// Common fields shared by all records.
#[derive(Clone, Debug)]
pub struct CommonFields {
    // Alarm state (current/result)
    pub sevr: AlarmSeverity,
    pub stat: u16,
    // New alarm state (pending, transferred by rec_gbl_reset_alarms)
    pub nsev: AlarmSeverity,
    pub nsta: u16,
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
    // Analog alarm config (Some for analog record types)
    pub analog_alarm: Option<AnalogAlarmConfig>,
    // Access security group
    pub asg: String,
    // Description (moved from individual records)
    pub desc: String,
    // Phase/priority/event
    pub phas: i16,
    pub evnt: i16,
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
}

impl Default for CommonFields {
    fn default() -> Self {
        Self {
            sevr: AlarmSeverity::NoAlarm,
            stat: 0,
            nsev: AlarmSeverity::NoAlarm,
            nsta: 0,
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
            analog_alarm: None,
            asg: "DEFAULT".to_string(),
            desc: String::new(),
            phas: 0,
            evnt: 0,
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
        }
    }
}

/// Parsed link address pointing to another record's field.
#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
}

/// Parsed link — distinguishes constants, DB links, CA/PVA links, and empty.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedLink {
    None,
    Constant(String),
    Db(DbLink),
    Ca(String),
    Pva(String),
}

/// A database link to another record's field.
#[derive(Clone, Debug, PartialEq)]
pub struct DbLink {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
}

impl ParsedLink {
    /// Extract the constant as an EpicsValue (Double if numeric, else String).
    pub fn constant_value(&self) -> Option<EpicsValue> {
        if let ParsedLink::Constant(s) = self {
            if let Ok(v) = s.parse::<f64>() {
                Some(EpicsValue::Double(v))
            } else {
                Some(EpicsValue::String(s.clone()))
            }
        } else {
            None
        }
    }

    pub fn is_db(&self) -> bool {
        matches!(self, ParsedLink::Db(_))
    }
}

/// Parse a link string into a ParsedLink (v2 — distinguishes constants from DB links).
pub fn parse_link_v2(s: &str) -> ParsedLink {
    let s = s.trim();
    if s.is_empty() {
        return ParsedLink::None;
    }

    // CA/PVA protocol links
    if let Some(rest) = s.strip_prefix("ca://") {
        return ParsedLink::Ca(rest.to_string());
    }
    if let Some(rest) = s.strip_prefix("pva://") {
        return ParsedLink::Pva(rest.to_string());
    }

    // Strip trailing link attributes: PP, NPP, CP, CPP, MS, NMS, MSS, MSI
    // They can appear in any order: "REC.FIELD NPP NMS", "REC CP", etc.
    let mut policy = LinkProcessPolicy::ProcessPassive;
    let mut link_part = s;
    loop {
        let trimmed = link_part.trim_end();
        if let Some(rest) = trimmed.strip_suffix(" NMS").or_else(|| trimmed.strip_suffix(" MS"))
            .or_else(|| trimmed.strip_suffix(" MSS")).or_else(|| trimmed.strip_suffix(" MSI"))
        {
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" NPP") {
            policy = LinkProcessPolicy::NoProcess;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" CP").or_else(|| trimmed.strip_suffix(" CPP")) {
            policy = LinkProcessPolicy::ChannelProcess;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" PP") {
            policy = LinkProcessPolicy::ProcessPassive;
            link_part = rest;
            continue;
        }
        link_part = trimmed;
        break;
    }

    // Numeric constant
    if link_part.parse::<f64>().is_ok() {
        return ParsedLink::Constant(link_part.to_string());
    }

    // Quoted string constant
    if link_part.starts_with('"') && link_part.ends_with('"') && link_part.len() >= 2 {
        return ParsedLink::Constant(link_part[1..link_part.len()-1].to_string());
    }

    // DB link: try rsplit on '.', validate field part is uppercase alpha 1-4 chars
    if let Some((rec, field)) = link_part.rsplit_once('.') {
        let field_upper = field.to_ascii_uppercase();
        let is_valid_field = !field_upper.is_empty()
            && field_upper.len() <= 4
            && field_upper.chars().all(|c| c.is_ascii_uppercase());
        if is_valid_field {
            return ParsedLink::Db(DbLink {
                record: rec.to_string(),
                field: field_upper,
                policy,
            });
        }
    }

    // No dot or invalid field part → DB link with default field VAL
    ParsedLink::Db(DbLink {
        record: link_part.to_string(),
        field: "VAL".to_string(),
        policy,
    })
}

/// Parse a link string into a LinkAddress (legacy wrapper around parse_link_v2).
/// Formats: "REC.FIELD", "REC", "REC.FIELD PP", "REC.FIELD NPP", "" → None
pub fn parse_link(s: &str) -> Option<LinkAddress> {
    match parse_link_v2(s) {
        ParsedLink::Db(db) => Some(LinkAddress {
            record: db.record,
            field: db.field,
            policy: db.policy,
        }),
        _ => None,
    }
}

/// Result of a record's process() call.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordProcessResult {
    /// Processing completed synchronously this cycle.
    Complete,
    /// Processing started but not yet complete (PACT stays set).
    AsyncPending,
    /// Async pending, but notify these intermediate field changes immediately.
    /// Used by motor records to flush DMOV=0 before the move completes.
    AsyncPendingNotify(Vec<(String, EpicsValue)>),
}

/// Result of setting a common field, indicating what scan index updates are needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonFieldPutResult {
    NoChange,
    ScanChanged { old_scan: ScanType, new_scan: ScanType, phas: i16 },
    PhasChanged { scan: ScanType, old_phas: i16, new_phas: i16 },
}

/// Snapshot of changes from a process cycle, used for notify outside lock.
pub struct ProcessSnapshot {
    pub changed_fields: Vec<(String, EpicsValue)>,
    /// Event mask computed for this cycle.
    pub event_mask: crate::server::recgbl::EventMask,
}

/// Trait that all EPICS record types must implement.
pub trait Record: Send + Sync + 'static {
    /// Return the record type name (e.g., "ai", "ao", "bi").
    fn record_type(&self) -> &'static str;

    /// Process the record (scan/compute cycle).
    fn process(&mut self) -> CaResult<RecordProcessResult> {
        Ok(RecordProcessResult::Complete)
    }

    /// Get a field value by name.
    fn get_field(&self, name: &str) -> Option<EpicsValue>;

    /// Set a field value by name.
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()>;

    /// Return the list of field descriptors.
    fn field_list(&self) -> &'static [FieldDesc];

    /// Validate a put before it is applied. Return Err to reject.
    fn validate_put(&self, _field: &str, _value: &EpicsValue) -> CaResult<()> {
        Ok(())
    }

    /// Hook called after a successful put_field.
    fn on_put(&mut self, _field: &str) {}

    /// Primary field name (default "VAL"). Override for waveform etc.
    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// Get the primary value.
    fn val(&self) -> Option<EpicsValue> {
        self.get_field(self.primary_field())
    }

    /// Set the primary value.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        self.put_field(self.primary_field(), value)
    }

    /// Whether this record type supports device write (output records only).
    fn can_device_write(&self) -> bool {
        matches!(self.record_type(), "ao" | "bo" | "longout" | "mbbo" | "stringout")
    }

    /// Whether this record should fire its forward link after processing.
    fn should_fire_forward_link(&self) -> bool {
        true
    }

    /// Whether this record's OUT link should be written after processing.
    /// Defaults to true. Override in calcout to implement OOPT conditional output.
    fn should_output(&self) -> bool {
        true
    }

    /// Initialize record (pass 0: field defaults; pass 1: dependent init).
    fn init_record(&mut self, _pass: u8) -> CaResult<()> {
        Ok(())
    }

    /// Called before/after a field put for side-effect processing.
    fn special(&mut self, _field: &str, _after: bool) -> CaResult<()> {
        Ok(())
    }

    /// Downcast to concrete type for device support init injection.
    /// Override in record types that need device support to inject state (e.g., MotorRecord).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    /// Whether processing this record should clear UDF.
    /// Override to return false for record types that don't produce a valid value every cycle.
    fn clears_udf(&self) -> bool {
        true
    }

    /// Return multi-input link field pairs: (link_field, value_field).
    /// Override in calc, calcout, sel, sub to return INPA..INPL → A..L mappings.
    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// Return multi-output link field pairs: (link_field, value_field).
    /// Override in transform to return OUTA..OUTP → A..P mappings.
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }
}

/// Subroutine function type for sub records.
pub type SubroutineFn = Box<dyn Fn(&mut dyn Record) -> CaResult<()> + Send + Sync>;

/// A type-erased record instance stored in the database.
pub struct RecordInstance {
    pub name: String,
    pub record: Box<dyn Record>,
    pub common: CommonFields,
    pub subscribers: HashMap<String, Vec<Subscriber>>,
    // Link parse cache
    pub parsed_inp: ParsedLink,
    pub parsed_out: ParsedLink,
    pub parsed_flnk: ParsedLink,
    pub parsed_sdis: ParsedLink,
    pub parsed_tsel: ParsedLink,
    // Device support
    pub device: Option<Box<dyn super::device_support::DeviceSupport>>,
    // Subroutine (for sub records)
    pub subroutine: Option<Arc<SubroutineFn>>,
    // Re-entrancy guard
    pub processing: AtomicBool,
}

impl RecordInstance {
    pub fn new(name: String, record: impl Record) -> Self {
        Self::new_boxed(name, Box::new(record))
    }

    pub fn new_boxed(name: String, record: Box<dyn Record>) -> Self {
        let rtype = record.record_type();
        let analog_alarm = match rtype {
            "ai" | "ao" | "longin" | "longout" => Some(AnalogAlarmConfig::default()),
            _ => None,
        };
        let mut common = CommonFields::default();
        common.analog_alarm = analog_alarm;

        Self {
            name,
            record,
            common,
            subscribers: HashMap::new(),
            parsed_inp: ParsedLink::None,
            parsed_out: ParsedLink::None,
            parsed_flnk: ParsedLink::None,
            parsed_sdis: ParsedLink::None,
            parsed_tsel: ParsedLink::None,
            device: None,
            subroutine: None,
            processing: AtomicBool::new(false),
        }
    }

    /// Unified field resolution: record fields → common fields → virtual fields.
    pub fn resolve_field(&self, name: &str) -> Option<EpicsValue> {
        let name = name.to_ascii_uppercase();
        self.record
            .get_field(&name)
            .or_else(|| self.get_common_field(&name))
            .or_else(|| self.get_virtual_field(&name))
    }

    /// Build a Snapshot with full metadata for the given field.
    pub fn snapshot_for_field(&self, field: &str) -> Option<super::snapshot::Snapshot> {
        let value = self.resolve_field(field)?;
        let mut snap = super::snapshot::Snapshot::new(
            value,
            self.common.stat,
            self.common.sevr as u16,
            self.common.time,
        );
        self.populate_display_info(&mut snap);
        self.populate_control_info(&mut snap);
        self.populate_enum_info(&mut snap);
        self.populate_common_enum_info(field, &mut snap);
        Some(snap)
    }

    /// Populate DisplayInfo from record fields if applicable.
    fn populate_display_info(&self, snap: &mut super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            "ai" | "ao" | "calc" | "calcout" => {
                let egu = self.record.get_field("EGU")
                    .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                let prec = self.record.get_field("PREC")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0) as i16;
                let hopr = self.record.get_field("HOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lopr = self.record.get_field("LOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let (hihi, high, low, lolo) = self.alarm_limits();
                snap.display = Some(super::snapshot::DisplayInfo {
                    units: egu,
                    precision: prec,
                    upper_disp_limit: hopr,
                    lower_disp_limit: lopr,
                    upper_alarm_limit: hihi,
                    upper_warning_limit: high,
                    lower_warning_limit: low,
                    lower_alarm_limit: lolo,
                });
            }
            "longin" | "longout" => {
                let egu = self.record.get_field("EGU")
                    .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                let hopr = self.record.get_field("HOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lopr = self.record.get_field("LOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let (hihi, high, low, lolo) = self.alarm_limits();
                snap.display = Some(super::snapshot::DisplayInfo {
                    units: egu,
                    precision: 0,
                    upper_disp_limit: hopr,
                    lower_disp_limit: lopr,
                    upper_alarm_limit: hihi,
                    upper_warning_limit: high,
                    lower_warning_limit: low,
                    lower_alarm_limit: lolo,
                });
            }
            _ => {}
        }
    }

    /// Populate ControlInfo from record fields if applicable.
    fn populate_control_info(&self, snap: &mut super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            "ao" | "longout" => {
                // Output records use DRVH/DRVL, fallback to HOPR/LOPR
                let drvh = self.record.get_field("DRVH").and_then(|v| v.to_f64());
                let drvl = self.record.get_field("DRVL").and_then(|v| v.to_f64());
                let hopr = self.record.get_field("HOPR").and_then(|v| v.to_f64()).unwrap_or(0.0);
                let lopr = self.record.get_field("LOPR").and_then(|v| v.to_f64()).unwrap_or(0.0);
                snap.control = Some(super::snapshot::ControlInfo {
                    upper_ctrl_limit: drvh.unwrap_or(hopr),
                    lower_ctrl_limit: drvl.unwrap_or(lopr),
                });
            }
            "ai" | "longin" | "calc" | "calcout" => {
                // Input records use HOPR/LOPR as control limits
                let hopr = self.record.get_field("HOPR").and_then(|v| v.to_f64()).unwrap_or(0.0);
                let lopr = self.record.get_field("LOPR").and_then(|v| v.to_f64()).unwrap_or(0.0);
                snap.control = Some(super::snapshot::ControlInfo {
                    upper_ctrl_limit: hopr,
                    lower_ctrl_limit: lopr,
                });
            }
            _ => {}
        }
    }

    /// Populate EnumInfo from record fields if applicable.
    fn populate_enum_info(&self, snap: &mut super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            "bi" | "bo" | "busy" => {
                let znam = self.record.get_field("ZNAM")
                    .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                let onam = self.record.get_field("ONAM")
                    .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                snap.enums = Some(super::snapshot::EnumInfo {
                    strings: vec![znam, onam],
                });
            }
            "mbbi" | "mbbo" => {
                let state_fields = [
                    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST",
                    "EIST", "NIST", "TEST", "ELST", "TVST", "TTST", "FTST", "FFST",
                ];
                let strings: Vec<String> = state_fields
                    .iter()
                    .map(|f| {
                        self.record.get_field(f)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default()
                    })
                    .collect();
                snap.enums = Some(super::snapshot::EnumInfo { strings });
            }
            _ => {}
        }
    }

    /// Populate enum strings for common fields accessed via CA (e.g. .SCAN).
    fn populate_common_enum_info(&self, field: &str, snap: &mut super::snapshot::Snapshot) {
        match field {
            "SCAN" => {
                snap.enums = Some(super::snapshot::EnumInfo {
                    strings: vec![
                        "Passive".into(),
                        "Event".into(),
                        "I/O Intr".into(),
                        "10 second".into(),
                        "5 second".into(),
                        "2 second".into(),
                        "1 second".into(),
                        ".5 second".into(),
                        ".2 second".into(),
                        ".1 second".into(),
                    ],
                });
            }
            _ => {}
        }
    }

    /// Extract analog alarm limits from CommonFields.
    fn alarm_limits(&self) -> (f64, f64, f64, f64) {
        if let Some(ref aa) = self.common.analog_alarm {
            (aa.hihi, aa.high, aa.low, aa.lolo)
        } else {
            (0.0, 0.0, 0.0, 0.0)
        }
    }

    /// Get a common field value.
    pub fn get_common_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "SEVR" => Some(EpicsValue::Short(self.common.sevr as i16)),
            "STAT" => Some(EpicsValue::Short(self.common.stat as i16)),
            "NSEV" => Some(EpicsValue::Short(self.common.nsev as i16)),
            "NSTA" => Some(EpicsValue::Short(self.common.nsta as i16)),
            "ACKS" => Some(EpicsValue::Short(self.common.acks as i16)),
            "ACKT" => Some(EpicsValue::Char(if self.common.ackt { 1 } else { 0 })),
            "UDF" => Some(EpicsValue::Char(if self.common.udf { 1 } else { 0 })),
            "UDFS" => Some(EpicsValue::Short(self.common.udfs as i16)),
            "SCAN" => Some(EpicsValue::Enum(self.common.scan as u16)),
            "SSCN" => Some(EpicsValue::Enum(self.common.sscn as u16)),
            "PINI" => Some(EpicsValue::Char(if self.common.pini { 1 } else { 0 })),
            "TPRO" => Some(EpicsValue::Char(if self.common.tpro { 1 } else { 0 })),
            "BKPT" => Some(EpicsValue::Char(self.common.bkpt)),
            "FLNK" => Some(EpicsValue::String(self.common.flnk.clone())),
            "INP" => Some(EpicsValue::String(self.common.inp.clone())),
            "OUT" => Some(EpicsValue::String(self.common.out.clone())),
            "DTYP" => Some(EpicsValue::String(self.common.dtyp.clone())),
            "TSE" => Some(EpicsValue::Short(self.common.tse)),
            "TSEL" => Some(EpicsValue::String(self.common.tsel.clone())),
            "ASG" => Some(EpicsValue::String(self.common.asg.clone())),
            "DESC" => Some(EpicsValue::String(self.common.desc.clone())),
            "PHAS" => Some(EpicsValue::Short(self.common.phas)),
            "EVNT" => Some(EpicsValue::Short(self.common.evnt)),
            "PRIO" => Some(EpicsValue::Short(self.common.prio)),
            "DISV" => Some(EpicsValue::Short(self.common.disv)),
            "DISA" => Some(EpicsValue::Short(self.common.disa)),
            "SDIS" => Some(EpicsValue::String(self.common.sdis.clone())),
            "DISS" => Some(EpicsValue::Short(self.common.diss as i16)),
            "HYST" => Some(EpicsValue::Double(self.common.hyst)),
            "LCNT" => Some(EpicsValue::Short(self.common.lcnt)),
            "DISP" => Some(EpicsValue::Char(if self.common.disp { 1 } else { 0 })),
            "PUTF" => Some(EpicsValue::Char(if self.common.putf { 1 } else { 0 })),
            "RPRO" => Some(EpicsValue::Char(if self.common.rpro { 1 } else { 0 })),
            "PACT" => Some(EpicsValue::Char(
                if self.processing.load(std::sync::atomic::Ordering::Acquire) { 1 } else { 0 }
            )),
            "PROC" => Some(EpicsValue::Char(0)), // Always 0 (trigger-only)
            // Analog alarm fields
            "HIHI" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Double(a.hihi)),
            "HIGH" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Double(a.high)),
            "LOW" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Double(a.low)),
            "LOLO" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Double(a.lolo)),
            "HHSV" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Short(a.hhsv as i16)),
            "HSV" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Short(a.hsv as i16)),
            "LSV" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Short(a.lsv as i16)),
            "LLSV" => self.common.analog_alarm.as_ref().map(|a| EpicsValue::Short(a.llsv as i16)),
            _ => None,
        }
    }

    /// Set a common field value. Returns what scan index changes are needed.
    pub fn put_common_field(&mut self, name: &str, value: EpicsValue) -> CaResult<CommonFieldPutResult> {
        let name = name.to_ascii_uppercase();
        self.record.validate_put(&name, &value)?;
        self.record.special(&name, false)?;
        match name.as_str() {
            "SEVR" => {
                if let EpicsValue::Short(v) = value {
                    self.common.sevr = AlarmSeverity::from_u16(v as u16);
                }
            }
            "STAT" => {
                if let EpicsValue::Short(v) = value {
                    self.common.stat = v as u16;
                }
            }
            "NSEV" => {
                if let EpicsValue::Short(v) = value {
                    self.common.nsev = AlarmSeverity::from_u16(v as u16);
                }
            }
            "NSTA" => {
                if let EpicsValue::Short(v) = value {
                    self.common.nsta = v as u16;
                }
            }
            "ACKS" => {
                if let EpicsValue::Short(v) = value {
                    self.common.acks = AlarmSeverity::from_u16(v as u16);
                }
            }
            "ACKT" => {
                if let EpicsValue::Char(v) = value {
                    self.common.ackt = v != 0;
                }
            }
            "UDF" => {
                if let EpicsValue::Char(v) = value {
                    self.common.udf = v != 0;
                }
            }
            "UDFS" => {
                if let EpicsValue::Short(v) = value {
                    self.common.udfs = AlarmSeverity::from_u16(v as u16);
                }
            }
            "SCAN" => {
                let old_scan = self.common.scan;
                let new_scan = match &value {
                    EpicsValue::Short(v) => ScanType::from_u16(*v as u16),
                    EpicsValue::Enum(v) => ScanType::from_u16(*v),
                    EpicsValue::String(s) => ScanType::from_str(s)?,
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.scan = new_scan;
                if old_scan != new_scan {
                    let phas = self.common.phas;
                    self.record.on_put(&name);
                    let _ = self.record.special(&name, true);
                    return Ok(CommonFieldPutResult::ScanChanged { old_scan, new_scan, phas });
                }
            }
            "SSCN" => {
                let new_sscn = match &value {
                    EpicsValue::Short(v) => ScanType::from_u16(*v as u16),
                    EpicsValue::Enum(v) => ScanType::from_u16(*v),
                    EpicsValue::String(s) => ScanType::from_str(s)?,
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.sscn = new_sscn;
            }
            "PINI" => {
                if let EpicsValue::Char(v) = value {
                    self.common.pini = v != 0;
                } else if let EpicsValue::String(s) = &value {
                    self.common.pini = s == "YES" || s == "1" || s == "true";
                }
            }
            "TPRO" => {
                if let EpicsValue::Char(v) = value {
                    self.common.tpro = v != 0;
                }
            }
            "BKPT" => {
                if let EpicsValue::Char(v) = value {
                    self.common.bkpt = v;
                }
            }
            "FLNK" => {
                if let EpicsValue::String(s) = value {
                    self.common.flnk = s;
                    self.parsed_flnk = parse_link_v2(&self.common.flnk);
                }
            }
            "INP" => {
                if let EpicsValue::String(s) = value {
                    self.common.inp = s;
                    self.parsed_inp = parse_link_v2(&self.common.inp);
                }
            }
            "OUT" => {
                if let EpicsValue::String(s) = value {
                    self.common.out = s;
                    self.parsed_out = parse_link_v2(&self.common.out);
                }
            }
            "DTYP" => {
                if let EpicsValue::String(s) = value {
                    self.common.dtyp = s;
                }
            }
            "TSE" => {
                if let EpicsValue::Short(v) = value {
                    self.common.tse = v;
                }
            }
            "TSEL" => {
                if let EpicsValue::String(s) = value {
                    self.common.tsel = s;
                    self.parsed_tsel = parse_link_v2(&self.common.tsel);
                }
            }
            "ASG" => {
                if let EpicsValue::String(s) = value {
                    self.common.asg = s;
                }
            }
            "DESC" => {
                if let EpicsValue::String(s) = value {
                    self.common.desc = s;
                }
            }
            "PHAS" => {
                if let EpicsValue::Short(v) = value {
                    let old_phas = self.common.phas;
                    self.common.phas = v;
                    if old_phas != v && self.common.scan != ScanType::Passive {
                        let scan = self.common.scan;
                        self.record.on_put(&name);
                        let _ = self.record.special(&name, true);
                        return Ok(CommonFieldPutResult::PhasChanged { scan, old_phas, new_phas: v });
                    }
                }
            }
            "EVNT" => {
                if let EpicsValue::Short(v) = value {
                    self.common.evnt = v;
                }
            }
            "PRIO" => {
                if let EpicsValue::Short(v) = value {
                    self.common.prio = v;
                }
            }
            "DISV" => {
                if let EpicsValue::Short(v) = value {
                    self.common.disv = v;
                }
            }
            "DISA" => {
                if let EpicsValue::Short(v) = value {
                    self.common.disa = v;
                }
            }
            "SDIS" => {
                if let EpicsValue::String(s) = value {
                    self.common.sdis = s;
                    self.parsed_sdis = parse_link_v2(&self.common.sdis);
                }
            }
            "DISS" => {
                if let EpicsValue::Short(v) = value {
                    self.common.diss = AlarmSeverity::from_u16(v as u16);
                }
            }
            "HYST" => {
                if let EpicsValue::Double(v) = value {
                    self.common.hyst = v;
                }
            }
            "LCNT" => {
                if let EpicsValue::Short(v) = value { self.common.lcnt = v; }
            }
            "DISP" => {
                match value {
                    EpicsValue::Char(v) => self.common.disp = v != 0,
                    EpicsValue::Short(v) => self.common.disp = v != 0,
                    _ => {}
                }
            }
            "PUTF" => return Err(CaError::ReadOnlyField("PUTF".into())),
            "RPRO" => {
                if let EpicsValue::Char(v) = value {
                    self.common.rpro = v != 0;
                }
            }
            "PACT" => return Err(CaError::ReadOnlyField("PACT".into())),
            "PROC" => { /* Trigger handled by put_record_field_from_ca; no-op here */ }
            // Analog alarm fields
            "HIHI" => {
                if let (Some(a), EpicsValue::Double(v)) = (&mut self.common.analog_alarm, value) {
                    a.hihi = v;
                }
            }
            "HIGH" => {
                if let (Some(a), EpicsValue::Double(v)) = (&mut self.common.analog_alarm, value) {
                    a.high = v;
                }
            }
            "LOW" => {
                if let (Some(a), EpicsValue::Double(v)) = (&mut self.common.analog_alarm, value) {
                    a.low = v;
                }
            }
            "LOLO" => {
                if let (Some(a), EpicsValue::Double(v)) = (&mut self.common.analog_alarm, value) {
                    a.lolo = v;
                }
            }
            "HHSV" => {
                if let (Some(a), EpicsValue::Short(v)) = (&mut self.common.analog_alarm, value) {
                    a.hhsv = AlarmSeverity::from_u16(v as u16);
                }
            }
            "HSV" => {
                if let (Some(a), EpicsValue::Short(v)) = (&mut self.common.analog_alarm, value) {
                    a.hsv = AlarmSeverity::from_u16(v as u16);
                }
            }
            "LSV" => {
                if let (Some(a), EpicsValue::Short(v)) = (&mut self.common.analog_alarm, value) {
                    a.lsv = AlarmSeverity::from_u16(v as u16);
                }
            }
            "LLSV" => {
                if let (Some(a), EpicsValue::Short(v)) = (&mut self.common.analog_alarm, value) {
                    a.llsv = AlarmSeverity::from_u16(v as u16);
                }
            }
            _ => {}
        }
        self.record.on_put(&name);
        let _ = self.record.special(&name, true);
        Ok(CommonFieldPutResult::NoChange)
    }

    /// Get virtual fields (NAME, RTYP).
    pub fn get_virtual_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "NAME" => Some(EpicsValue::String(self.name.clone())),
            "RTYP" => Some(EpicsValue::String(self.record.record_type().to_string())),
            _ => None,
        }
    }

    /// Evaluate alarms based on record type and current value.
    /// Uses rec_gbl_set_sevr to accumulate into nsta/nsev.
    pub fn evaluate_alarms(&mut self) {
        use crate::server::recgbl::{self, alarm_status};

        // Check UDF first
        recgbl::rec_gbl_check_udf(&mut self.common);

        let rtype = self.record.record_type();
        match rtype {
            "ai" | "ao" | "longin" | "longout" => {
                if let Some(ref alarm_cfg) = self.common.analog_alarm.clone() {
                    let val = match self.record.val() {
                        Some(EpicsValue::Double(v)) => v,
                        Some(EpicsValue::Long(v)) => v as f64,
                        _ => return,
                    };
                    self.evaluate_analog_alarm(val, alarm_cfg);
                }
            }
            "bi" | "bo" | "busy" => {
                let val = match self.record.val() {
                    Some(EpicsValue::Enum(v)) => v,
                    _ => return,
                };
                let zsv = self.record.get_field("ZSV").and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None }).unwrap_or(0);
                let osv = self.record.get_field("OSV").and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None }).unwrap_or(0);
                let cosv = self.record.get_field("COSV").and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None }).unwrap_or(0);

                let state_sev = if val == 0 { zsv } else { osv };
                let sev = AlarmSeverity::from_u16(state_sev as u16);
                let cos_sev = AlarmSeverity::from_u16(cosv as u16);
                let final_sev = if cos_sev as u16 > sev as u16 { cos_sev } else { sev };

                if final_sev != AlarmSeverity::NoAlarm {
                    recgbl::rec_gbl_set_sevr(&mut self.common, alarm_status::STATE_ALARM, final_sev);
                }
            }
            "mbbi" | "mbbo" => {
                let val = match self.record.val() {
                    Some(EpicsValue::Enum(v)) => v as usize,
                    _ => return,
                };
                let sv_fields = ["ZRSV", "ONSV", "TWSV", "THSV", "FRSV", "FVSV",
                    "SXSV", "SVSV", "EISV", "NISV", "TESV", "ELSV",
                    "TVSV", "TTSV", "FTSV", "FFSV"];
                let unsv = self.record.get_field("UNSV").and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None }).unwrap_or(0);
                let cosv = self.record.get_field("COSV").and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None }).unwrap_or(0);

                let state_sev = if val < 16 {
                    self.record.get_field(sv_fields[val])
                        .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                        .unwrap_or(unsv)
                } else {
                    unsv
                };

                let sev = AlarmSeverity::from_u16(state_sev as u16);
                let cos_sev = AlarmSeverity::from_u16(cosv as u16);
                let final_sev = if cos_sev as u16 > sev as u16 { cos_sev } else { sev };

                if final_sev != AlarmSeverity::NoAlarm {
                    recgbl::rec_gbl_set_sevr(&mut self.common, alarm_status::STATE_ALARM, final_sev);
                }
            }
            _ => {} // no-op for other types
        }
    }

    fn evaluate_analog_alarm(&mut self, val: f64, cfg: &AnalogAlarmConfig) {
        use crate::server::recgbl::{self, alarm_status};

        let hyst = self.common.hyst;
        let lalm = self.record.get_field("LALM")
            .and_then(|v| v.to_f64())
            .unwrap_or(val);

        let (new_sevr, new_stat) = if cfg.hhsv != AlarmSeverity::NoAlarm && val >= cfg.hihi && cfg.hihi != 0.0 {
            (cfg.hhsv, alarm_status::HIHI_ALARM)
        } else if cfg.llsv != AlarmSeverity::NoAlarm && val <= cfg.lolo && cfg.lolo != 0.0 {
            (cfg.llsv, alarm_status::LOLO_ALARM)
        } else if cfg.hsv != AlarmSeverity::NoAlarm && val >= cfg.high && cfg.high != 0.0 {
            (cfg.hsv, alarm_status::HIGH_ALARM)
        } else if cfg.lsv != AlarmSeverity::NoAlarm && val <= cfg.low && cfg.low != 0.0 {
            (cfg.lsv, alarm_status::LOW_ALARM)
        } else {
            (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM)
        };

        // Apply hysteresis: only change alarm if value moved enough from LALM
        if hyst > 0.0 && self.common.sevr != AlarmSeverity::NoAlarm {
            if new_sevr == AlarmSeverity::NoAlarm && (val - lalm).abs() < hyst {
                // Stay in current alarm (hysteresis prevents clearing)
                // Re-raise the current alarm into nsta/nsev
                let cur_stat = self.common.stat;
                let cur_sevr = self.common.sevr;
                recgbl::rec_gbl_set_sevr(&mut self.common, cur_stat, cur_sevr);
                return;
            }
        }

        if new_sevr != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(&mut self.common, new_stat, new_sevr);
            let _ = self.record.put_field("LALM", EpicsValue::Double(val));
        }
    }

    /// Basic process: process record, evaluate alarms, timestamp, build snapshot.
    /// This does NOT handle links — see process_with_context in database.rs.
    pub fn process_local(&mut self) -> CaResult<ProcessSnapshot> {
        use crate::server::recgbl::{self, EventMask};
        const LCNT_ALARM_THRESHOLD: i16 = 10;

        if self.processing.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.common.lcnt = self.common.lcnt.saturating_add(1);
            if self.common.lcnt >= LCNT_ALARM_THRESHOLD {
                self.common.sevr = AlarmSeverity::Invalid;
                self.common.stat = recgbl::alarm_status::SCAN_ALARM;
            }
            return Ok(ProcessSnapshot {
                changed_fields: Vec::new(),
                event_mask: EventMask::NONE,
            });
        }
        self.common.lcnt = 0;
        struct ProcessGuard(*const AtomicBool);
        unsafe impl Send for ProcessGuard {}
        impl Drop for ProcessGuard {
            fn drop(&mut self) {
                unsafe { &*self.0 }.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _guard = ProcessGuard(&self.processing as *const AtomicBool);

        // Call subroutine if registered (for sub records)
        if let Some(ref sub_fn) = self.subroutine {
            sub_fn(&mut *self.record)?;
        }
        let process_result = self.record.process()?;

        if process_result == RecordProcessResult::AsyncPending {
            // Async: PACT stays set, no further processing this cycle
            // Don't clear processing flag (guard won't run — we leak it intentionally)
            std::mem::forget(_guard);
            return Ok(ProcessSnapshot {
                changed_fields: Vec::new(),
                event_mask: EventMask::NONE,
            });
        }
        if let RecordProcessResult::AsyncPendingNotify(fields) = process_result {
            // Intermediate notification (e.g. DMOV=0 at move start).
            // Unlike AsyncPending, we DO release the processing flag so
            // subsequent I/O Intr cycles can continue processing normally.
            self.common.time = crate::runtime::general_time::get_current();
            // _guard drops here, clearing the processing flag
            return Ok(ProcessSnapshot {
                changed_fields: fields,
                event_mask: EventMask::VALUE | EventMask::ALARM,
            });
        }

        // Evaluate alarms (accumulates into nsta/nsev)
        self.evaluate_alarms();

        // Transfer nsta/nsev → sevr/stat, detect alarm change
        let alarm_result = recgbl::rec_gbl_reset_alarms(&mut self.common);

        self.common.time = crate::runtime::general_time::get_current();
        if self.record.clears_udf() {
            self.common.udf = false;
        }

        // Compute event mask
        let mut event_mask = EventMask::NONE;

        // Deadband check for VAL monitor filtering
        let (include_val, include_archive) = self.check_deadband_ext();
        if include_val {
            event_mask |= EventMask::VALUE;
        }
        if include_archive {
            event_mask |= EventMask::LOG;
        }
        if alarm_result.alarm_changed {
            event_mask |= EventMask::ALARM;
        }

        // Build snapshot
        let mut changed_fields = Vec::new();
        if include_val {
            if let Some(val) = self.record.val() {
                changed_fields.push(("VAL".to_string(), val));
            }
        }
        changed_fields.push(("SEVR".to_string(), EpicsValue::Short(self.common.sevr as i16)));
        changed_fields.push(("STAT".to_string(), EpicsValue::Short(self.common.stat as i16)));
        changed_fields.push(("UDF".to_string(), EpicsValue::Char(if self.common.udf { 1 } else { 0 })));

        // Add subscribed fields not already included
        for (field, subs) in &self.subscribers {
            if !subs.is_empty() && field != "VAL" && field != "SEVR" && field != "STAT" && field != "UDF" {
                if let Some(val) = self.resolve_field(field) {
                    changed_fields.push((field.clone(), val));
                }
            }
        }
        if !changed_fields.is_empty() {
            event_mask |= EventMask::VALUE;
        }

        Ok(ProcessSnapshot { changed_fields, event_mask })
    }

    /// Check deadband (MDEL/ADEL) for VAL monitor/archive filtering.
    /// Returns (monitor_trigger, archive_trigger).
    /// Updates ALST/MLST in the record when triggered.
    pub fn check_deadband_ext(&mut self) -> (bool, bool) {
        let mdel = match self.record.get_field("MDEL") {
            Some(v) => match v.to_f64() { Some(f) => f, None => return (true, true) },
            None => return (true, true),
        };
        let adel = self.record.get_field("ADEL").and_then(|v| v.to_f64()).unwrap_or(0.0);
        let mlst = self.record.get_field("MLST").and_then(|v| v.to_f64()).unwrap_or(0.0);
        let alst = self.record.get_field("ALST").and_then(|v| v.to_f64()).unwrap_or(0.0);
        let val = match self.record.val().and_then(|v| v.to_f64()) {
            Some(v) => v,
            None => return (true, true),
        };

        let monitor_trigger = mdel < 0.0 || (val - mlst).abs() > mdel;
        let archive_trigger = adel < 0.0 || (val - alst).abs() > adel;

        if archive_trigger {
            let _ = self.record.put_field("ALST", EpicsValue::Double(val));
        }
        if monitor_trigger {
            let _ = self.record.put_field("MLST", EpicsValue::Double(val));
        }

        (monitor_trigger, archive_trigger)
    }

    /// Build a Snapshot for a given value, populated with the record's display metadata.
    fn make_monitor_snapshot(&self, value: EpicsValue) -> super::snapshot::Snapshot {
        let mut snap = super::snapshot::Snapshot::new(
            value,
            self.common.stat,
            self.common.sevr as u16,
            self.common.time,
        );
        self.populate_display_info(&mut snap);
        self.populate_control_info(&mut snap);
        self.populate_enum_info(&mut snap);
        snap
    }

    /// Notify subscribers from a snapshot (call outside lock).
    /// Uses event_mask to filter: only notify subscribers whose mask intersects.
    pub fn notify_from_snapshot(&self, snapshot: &ProcessSnapshot) {
        use crate::server::recgbl::EventMask;
        let posting_mask = snapshot.event_mask;

        for (field, value) in &snapshot.changed_fields {
            if let Some(subs) = self.subscribers.get(field) {
                // Build a full snapshot once per field (with display metadata)
                let mon_snap = self.make_monitor_snapshot(value.clone());
                for sub in subs {
                    let sub_mask = EventMask::from_bits(sub.mask);
                    // Only send when posting mask intersects subscriber mask.
                    // Empty posting mask means nothing changed — skip.
                    if !posting_mask.is_empty() && sub_mask.intersects(posting_mask) {
                        let _ = sub.tx.try_send(MonitorEvent {
                            snapshot: mon_snap.clone(),
                        });
                    }
                }
            }
        }
    }

    /// Notify subscribers of a specific field, filtering by event mask.
    pub fn notify_field(&self, field: &str, mask: crate::server::recgbl::EventMask) {
        if let Some(subs) = self.subscribers.get(field) {
            if let Some(value) = self.resolve_field(field) {
                let mon_snap = self.make_monitor_snapshot(value);
                for sub in subs {
                    let sub_mask = crate::server::recgbl::EventMask::from_bits(sub.mask);
                    if mask.is_empty() || sub_mask.intersects(mask) {
                        let _ = sub.tx.try_send(MonitorEvent {
                            snapshot: mon_snap.clone(),
                        });
                    }
                }
            }
        }
    }

    /// Add a subscriber for a specific field.
    pub fn add_subscriber(
        &mut self,
        field: &str,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> mpsc::Receiver<MonitorEvent> {
        let (tx, rx) = mpsc::channel(64);
        let sub = Subscriber {
            sid,
            data_type,
            mask,
            tx,
        };
        self.subscribers
            .entry(field.to_string())
            .or_default()
            .push(sub);
        rx
    }

    /// Remove a subscriber by subscription ID from all fields.
    pub fn remove_subscriber(&mut self, sid: u32) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| s.sid != sid);
        }
    }

    /// Clean up closed subscriber channels.
    pub fn cleanup_subscribers(&mut self) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| !s.tx.is_closed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::ao::AoRecord;
    use crate::server::records::bi::BiRecord;
    use crate::server::records::stringin::StringinRecord;

    #[test]
    fn test_ai_record_type() {
        let rec = AiRecord::new(25.0);
        assert_eq!(rec.record_type(), "ai");
    }

    #[test]
    fn test_ai_get_val() {
        let rec = AiRecord::new(42.0);
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[test]
    fn test_ai_put_val() {
        let mut rec = AiRecord::new(0.0);
        rec.put_field("VAL", EpicsValue::Double(99.0)).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 99.0).abs() < 1e-10),
            other => panic!("expected Double(99.0), got {:?}", other),
        }
    }

    #[test]
    fn test_ai_string_field() {
        let mut rec = AiRecord::default();
        rec.put_field("EGU", EpicsValue::String("celsius".into())).unwrap();
        match rec.get_field("EGU") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "celsius"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_ai_field_list() {
        let rec = AiRecord::default();
        let fields = rec.field_list();
        assert_eq!(fields.len(), 24); // 20 base + 4 sim fields
        assert_eq!(fields[0].name, "VAL");
        assert_eq!(fields[0].dbf_type, DbFieldType::Double);
        assert_eq!(fields[1].name, "EGU");
    }

    #[test]
    fn test_ai_unknown_field() {
        let rec = AiRecord::default();
        assert!(rec.get_field("NONEXISTENT").is_none());
    }

    #[test]
    fn test_ai_put_type_mismatch() {
        let mut rec = AiRecord::default();
        let result = rec.put_field("VAL", EpicsValue::String("bad".into()));
        assert!(result.is_err());
    }

    #[test]
    fn test_ai_put_unknown_field() {
        let mut rec = AiRecord::default();
        let result = rec.put_field("NONEXISTENT", EpicsValue::Double(1.0));
        assert!(result.is_err());
    }

    #[test]
    fn test_ao_record() {
        let mut rec = AoRecord::new(10.0);
        assert_eq!(rec.record_type(), "ao");
        rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 20.0).abs() < 1e-10),
            other => panic!("expected Double(20.0), got {:?}", other),
        }
    }

    #[test]
    fn test_bi_record() {
        let mut rec = BiRecord::new(0);
        assert_eq!(rec.record_type(), "bi");
        rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Enum(v)) => assert_eq!(v, 1),
            other => panic!("expected Enum(1), got {:?}", other),
        }
        rec.put_field("ZNAM", EpicsValue::String("Off".into())).unwrap();
        rec.put_field("ONAM", EpicsValue::String("On".into())).unwrap();
        match rec.get_field("ZNAM") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "Off"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_stringin_record() {
        let rec = StringinRecord::new("hello");
        assert_eq!(rec.record_type(), "stringin");
        match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "hello"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_val_and_set_val() {
        let mut rec = AiRecord::new(5.0);
        match rec.val() {
            Some(EpicsValue::Double(v)) => assert!((v - 5.0).abs() < 1e-10),
            other => panic!("expected Double(5.0), got {:?}", other),
        }
        rec.set_val(EpicsValue::Double(10.0)).unwrap();
        match rec.val() {
            Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10),
            other => panic!("expected Double(10.0), got {:?}", other),
        }
    }

    #[test]
    fn test_record_instance() {
        let rec = AiRecord::new(25.0);
        let instance = RecordInstance::new("TEMP".into(), rec);
        assert_eq!(instance.name, "TEMP");
        match instance.record.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 25.0).abs() < 1e-10),
            other => panic!("expected Double(25.0), got {:?}", other),
        }
    }

    #[test]
    fn test_read_only_field() {
        use epics_macros::EpicsRecord;

        #[derive(EpicsRecord)]
        #[record(type = "test")]
        struct TestRecord {
            #[field(type = "Double")]
            pub val: f64,
            #[field(type = "String", read_only)]
            pub name: String,
        }

        let mut rec = TestRecord {
            val: 1.0,
            name: "fixed".into(),
        };

        // Can read the read-only field
        match rec.get_field("NAME") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "fixed"),
            other => panic!("expected String, got {:?}", other),
        }

        // Cannot write the read-only field
        let result = rec.put_field("NAME", EpicsValue::String("changed".into()));
        assert!(result.is_err());

        // Can still write the mutable field
        rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 2.0).abs() < 1e-10),
            other => panic!("expected Double(2.0), got {:?}", other),
        }

        // Verify field_list read_only flag
        let fields = rec.field_list();
        assert!(!fields[0].read_only); // VAL
        assert!(fields[1].read_only);  // NAME
    }

    #[test]
    fn test_parse_pv_name() {
        use crate::server::database::parse_pv_name;
        assert_eq!(parse_pv_name("TEMP"), ("TEMP", "VAL"));
        assert_eq!(parse_pv_name("TEMP.EGU"), ("TEMP", "EGU"));
        assert_eq!(parse_pv_name("TEMP.HOPR"), ("TEMP", "HOPR"));
        assert_eq!(parse_pv_name("A.B.C"), ("A.B", "C"));
    }

    #[test]
    fn test_resolve_field_priority() {
        let rec = AiRecord::new(25.0);
        let instance = RecordInstance::new("TEMP".into(), rec);

        // Record field
        assert!(matches!(instance.resolve_field("VAL"), Some(EpicsValue::Double(_))));
        // Common field
        assert!(matches!(instance.resolve_field("SEVR"), Some(EpicsValue::Short(0))));
        assert!(matches!(instance.resolve_field("SCAN"), Some(EpicsValue::Enum(0))));
        // Virtual field
        match instance.resolve_field("NAME") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "TEMP"),
            other => panic!("expected String(TEMP), got {:?}", other),
        }
        match instance.resolve_field("RTYP") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "ai"),
            other => panic!("expected String(ai), got {:?}", other),
        }
        // Analog alarm fields available for ai
        assert!(instance.resolve_field("HIHI").is_some());
        // Unknown field
        assert!(instance.resolve_field("NONEXISTENT").is_none());
    }

    #[test]
    fn test_common_field_put() {
        let rec = AiRecord::new(25.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);

        // Set SCAN
        let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        assert!(matches!(result, CommonFieldPutResult::ScanChanged { .. })); // SCAN changed
        assert_eq!(instance.common.scan, ScanType::Sec1);

        // Set analog alarm threshold
        instance.put_common_field("HIHI", EpicsValue::Double(100.0)).unwrap();
        assert_eq!(instance.common.analog_alarm.as_ref().unwrap().hihi, 100.0);
    }

    #[test]
    fn test_evaluate_alarms() {
        use crate::server::recgbl;
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);
        instance.common.udf = false; // Clear UDF so it doesn't interfere

        // Set alarm thresholds
        instance.put_common_field("HIHI", EpicsValue::Double(100.0)).unwrap();
        instance.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16)).unwrap();
        instance.put_common_field("HIGH", EpicsValue::Double(80.0)).unwrap();
        instance.put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16)).unwrap();

        // No alarm
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

        // HIGH alarm
        instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // HIHI alarm
        instance.record.set_val(EpicsValue::Double(105.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Major);
    }

    #[test]
    fn test_parse_link() {
        assert!(parse_link("").is_none());

        let link = parse_link("TEMP").unwrap();
        assert_eq!(link.record, "TEMP");
        assert_eq!(link.field, "VAL");

        let link = parse_link("TEMP.EGU").unwrap();
        assert_eq!(link.record, "TEMP");
        assert_eq!(link.field, "EGU");

        let link = parse_link("TEMP.VAL PP").unwrap();
        assert_eq!(link.record, "TEMP");
        assert_eq!(link.field, "VAL");
        assert_eq!(link.policy, LinkProcessPolicy::ProcessPassive);

        let link = parse_link("TEMP.VAL NPP").unwrap();
        assert_eq!(link.policy, LinkProcessPolicy::NoProcess);
    }

    #[test]
    fn test_parse_link_v2() {
        // Empty
        assert_eq!(parse_link_v2(""), ParsedLink::None);
        assert_eq!(parse_link_v2("  "), ParsedLink::None);

        // Integer constant
        assert_eq!(parse_link_v2("42"), ParsedLink::Constant("42".to_string()));

        // Float constant (was bug: "3.14" used to parse as DB link record="3", field="14")
        assert_eq!(parse_link_v2("3.14"), ParsedLink::Constant("3.14".to_string()));
        assert_eq!(parse_link_v2("-1.5"), ParsedLink::Constant("-1.5".to_string()));

        // DB link — bare record name
        assert_eq!(parse_link_v2("TEMP"), ParsedLink::Db(DbLink {
            record: "TEMP".into(), field: "VAL".into(),
            policy: LinkProcessPolicy::ProcessPassive,
        }));

        // DB link — record.field
        assert_eq!(parse_link_v2("TEMP.EGU"), ParsedLink::Db(DbLink {
            record: "TEMP".into(), field: "EGU".into(),
            policy: LinkProcessPolicy::ProcessPassive,
        }));

        // DB link with NPP
        assert_eq!(parse_link_v2("TEMP.EGU NPP"), ParsedLink::Db(DbLink {
            record: "TEMP".into(), field: "EGU".into(),
            policy: LinkProcessPolicy::NoProcess,
        }));

        // CA/PVA protocols
        assert_eq!(parse_link_v2("ca://PV:NAME"), ParsedLink::Ca("PV:NAME".to_string()));
        assert_eq!(parse_link_v2("pva://PV:NAME"), ParsedLink::Pva("PV:NAME".to_string()));

        // Quoted string constant
        assert_eq!(parse_link_v2("\"hello\""), ParsedLink::Constant("hello".to_string()));

        // Constant value extraction
        let c = parse_link_v2("3.14");
        assert_eq!(c.constant_value(), Some(EpicsValue::Double(3.14)));
        let c = parse_link_v2("\"hello\"");
        assert_eq!(c.constant_value(), Some(EpicsValue::String("hello".into())));
        assert_eq!(parse_link_v2("TEMP").constant_value(), None);
    }

    #[test]
    fn test_link_cache_invalidation() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);

        assert_eq!(instance.parsed_inp, ParsedLink::None);
        instance.put_common_field("INP", EpicsValue::String("SOURCE.VAL".into())).unwrap();
        if let ParsedLink::Db(ref db) = instance.parsed_inp {
            assert_eq!(db.record, "SOURCE");
        } else {
            panic!("expected Db link");
        }

        // Change link → cache updated
        instance.put_common_field("INP", EpicsValue::String("OTHER".into())).unwrap();
        if let ParsedLink::Db(ref db) = instance.parsed_inp {
            assert_eq!(db.record, "OTHER");
            assert_eq!(db.field, "VAL");
        } else {
            panic!("expected Db link");
        }

        // Clear link → cache cleared
        instance.put_common_field("INP", EpicsValue::String("".into())).unwrap();
        assert_eq!(instance.parsed_inp, ParsedLink::None);
    }

    #[test]
    fn test_ai_linear_conversion() {
        let mut rec = AiRecord::default();
        rec.linr = 1; // LINEAR
        rec.eguf = 100.0;
        rec.egul = 0.0;
        rec.eslo = 1.0;
        rec.roff = 0;
        rec.aslo = 1.0;
        rec.aoff = 0.0;

        rec.rval = 50;
        rec.process().unwrap();
        // (50 + 0) * 1.0 + 0.0 * 1.0 + 0.0 = 50.0
        assert!((rec.val - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_ai_linear_with_offsets() {
        let mut rec = AiRecord::default();
        rec.linr = 1;
        rec.egul = 10.0;
        rec.eslo = 0.5;
        rec.roff = 100;
        rec.aslo = 2.0;
        rec.aoff = 5.0;

        rec.rval = 200;
        rec.process().unwrap();
        // (200 + 100) * 2.0 + 5.0 = 605.0
        // 605.0 * 0.5 + 10.0 = 312.5
        assert!((rec.val - 312.5).abs() < 1e-10);
    }

    #[test]
    fn test_ai_smoothing() {
        let mut rec = AiRecord::default();
        rec.linr = 1;
        rec.eslo = 1.0;
        rec.aslo = 1.0;
        rec.smoo = 0.5;

        rec.rval = 100;
        rec.process().unwrap();
        // First process: no smoothing (init was false)
        assert!((rec.val - 100.0).abs() < 1e-10);
        assert!(rec.init);

        // Second process with same value — should be 100 * 0.5 + 100 * 0.5 = 100
        rec.rval = 200;
        rec.process().unwrap();
        // new_val = 200 * 0.5 + 100 * 0.5 = 150
        assert!((rec.val - 150.0).abs() < 1e-10);
    }

    #[test]
    fn test_ai_no_conversion() {
        let mut rec = AiRecord::default();
        rec.linr = 0; // NO_CONVERSION
        rec.val = 42.0; // Set directly (as soft channel would)
        rec.process().unwrap();
        // VAL should be unchanged
        assert!((rec.val - 42.0).abs() < 1e-10);
    }

    #[test]
    fn test_common_fields_desc() {
        let rec = AiRecord::new(25.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);

        // DESC is now a common field
        instance.put_common_field("DESC", EpicsValue::String("Temperature".into())).unwrap();
        match instance.get_common_field("DESC") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
            other => panic!("expected String, got {:?}", other),
        }
        // Also accessible via resolve_field (common level)
        match instance.resolve_field("DESC") {
            Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn test_common_fields_new() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // PHAS default
        assert_eq!(instance.common.phas, 0);
        instance.put_common_field("PHAS", EpicsValue::Short(2)).unwrap();
        assert_eq!(instance.common.phas, 2);

        // DISV default is 1
        assert_eq!(instance.common.disv, 1);

        // HYST
        instance.put_common_field("HYST", EpicsValue::Double(5.0)).unwrap();
        assert!((instance.common.hyst - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_hyst_alarm_hysteresis() {
        use crate::server::recgbl;
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEMP".into(), rec);
        instance.common.udf = false;

        instance.put_common_field("HIGH", EpicsValue::Double(80.0)).unwrap();
        instance.put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16)).unwrap();
        instance.put_common_field("HYST", EpicsValue::Double(5.0)).unwrap();

        // Go into HIGH alarm at val=85 → LALM=85
        instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // val=82: still >= HIGH=80, alarm stays → LALM updated to 82
        instance.record.set_val(EpicsValue::Double(82.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // val=78: below HIGH=80, but |78-LALM(82)|=4 < hyst=5 → stays in alarm
        instance.record.set_val(EpicsValue::Double(78.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // val=76: below HIGH=80, |76-82|=6 >= hyst=5 → alarm clears
        instance.record.set_val(EpicsValue::Double(76.0)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_deadband_mdel() {
        let mut rec = AiRecord::default();
        rec.mdel = 5.0;
        rec.adel = 0.0;
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // First process: val=0, mlst=0 → |0-0|=0, not > 5 → no trigger
        // But first time should trigger (MDEL=0 means any change triggers)
        // With MDEL=5, |0-0|=0 which is NOT > 5, so no trigger
        instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
        let snap = instance.process_local().unwrap();
        // VAL not included since |0-0| is not > 5
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=3: |3-0|=3, not > 5 → no trigger
        instance.record.set_val(EpicsValue::Double(3.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=6: |6-0|=6 > 5 → trigger, MLST updated to 6
        instance.record.set_val(EpicsValue::Double(6.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=10: |10-6|=4, not > 5 → no trigger
        instance.record.set_val(EpicsValue::Double(10.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        // val=12: |12-6|=6 > 5 → trigger
        instance.record.set_val(EpicsValue::Double(12.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    }

    #[test]
    fn test_deadband_mdel_zero() {
        let mut rec = AiRecord::default();
        rec.mdel = 0.0;
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // MDEL=0 means any change triggers
        instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
        let snap = instance.process_local().unwrap();
        // |0-0|=0, not > 0 → no trigger (same value)
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));

        instance.record.set_val(EpicsValue::Double(0.001)).unwrap();
        let snap = instance.process_local().unwrap();
        // |0.001-0|=0.001 > 0 → trigger
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    }

    #[test]
    fn test_deadband_mdel_negative() {
        let mut rec = AiRecord::default();
        rec.mdel = -1.0;
        let mut instance = RecordInstance::new("TEST".into(), rec);

        // MDEL < 0 means always trigger
        instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
        let snap = instance.process_local().unwrap();
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
    }

    #[test]
    fn test_bi_state_alarm() {
        use crate::server::recgbl;
        let mut rec = BiRecord::new(0);
        rec.zsv = AlarmSeverity::Major as i16;
        rec.osv = AlarmSeverity::Minor as i16;

        let mut instance = RecordInstance::new("SW".into(), rec);
        instance.common.udf = false;

        // VAL=0 → ZSV=Major
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Major);

        // VAL=1 → OSV=Minor
        instance.record.set_val(EpicsValue::Enum(1)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);
    }

    #[test]
    fn test_mbbi_state_alarm() {
        use crate::server::recgbl;
        use crate::server::records::mbbi::MbbiRecord;

        let mut rec = MbbiRecord::new(0);
        rec.onsv = AlarmSeverity::Minor as i16;
        rec.twsv = AlarmSeverity::Major as i16;

        let mut instance = RecordInstance::new("SEL".into(), rec);
        instance.common.udf = false;

        // VAL=0 → ZRSV=0 (NoAlarm)
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

        // VAL=1 → ONSV=Minor
        instance.record.set_val(EpicsValue::Enum(1)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

        // VAL=2 → TWSV=Major
        instance.record.set_val(EpicsValue::Enum(2)).unwrap();
        instance.evaluate_alarms();
        recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert_eq!(instance.common.sevr, AlarmSeverity::Major);
    }

    #[test]
    fn test_mbbi_unsv() {
        use crate::server::records::mbbi::MbbiRecord;

        let mut rec = MbbiRecord::new(0);
        rec.unsv = AlarmSeverity::Invalid as i16;

        let mut instance = RecordInstance::new("SEL".into(), rec);

        // VAL=15 → FFSV=0 (NoAlarm), not UNSV
        instance.record.set_val(EpicsValue::Enum(15)).unwrap();
        instance.evaluate_alarms();
        assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_deadband_alarm_always_included() {
        let mut rec = AiRecord::default();
        rec.mdel = 100.0; // Very high deadband — VAL never triggers
        let mut instance = RecordInstance::new("TEST".into(), rec);

        instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
        let snap = instance.process_local().unwrap();
        // VAL not included due to deadband
        assert!(!snap.changed_fields.iter().any(|(k, _)| k == "VAL"));
        // But SEVR/STAT/UDF always included
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "SEVR"));
        assert!(snap.changed_fields.iter().any(|(k, _)| k == "STAT"));
    }

    #[test]
    fn test_pact_reads_zero_when_idle() {
        let instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        match instance.get_common_field("PACT") {
            Some(EpicsValue::Char(0)) => {}
            other => panic!("expected Char(0), got {:?}", other),
        }
    }

    #[test]
    fn test_pact_write_rejected() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        let result = instance.put_common_field("PACT", EpicsValue::Char(1));
        assert!(matches!(result, Err(CaError::ReadOnlyField(_))));
    }

    #[test]
    fn test_lcnt_zero_after_process() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        instance.common.lcnt = 5; // Pre-set to non-zero
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 0);
    }

    #[test]
    fn test_lcnt_increments_on_reentrance() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        // Simulate active processing
        instance.processing.store(true, std::sync::atomic::Ordering::Release);
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 1);
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 2);
    }

    #[test]
    fn test_lcnt_alarm_threshold() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        instance.processing.store(true, std::sync::atomic::Ordering::Release);
        for _ in 0..10 {
            let _ = instance.process_local().unwrap();
        }
        assert!(instance.common.lcnt >= 10);
        assert_eq!(instance.common.sevr, AlarmSeverity::Invalid);
        assert_eq!(instance.common.stat, 12); // SCAN_ALARM
    }

    #[test]
    fn test_lcnt_reset_on_success() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        instance.common.lcnt = 5;
        // processing is false (default), so process_local should succeed
        let _ = instance.process_local().unwrap();
        assert_eq!(instance.common.lcnt, 0);
    }

    #[test]
    fn test_proc_reads_zero() {
        let instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        match instance.get_common_field("PROC") {
            Some(EpicsValue::Char(0)) => {}
            other => panic!("expected Char(0), got {:?}", other),
        }
    }

    #[test]
    fn test_disp_get_put() {
        let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
        // Default is false
        match instance.get_common_field("DISP") {
            Some(EpicsValue::Char(0)) => {}
            other => panic!("expected Char(0), got {:?}", other),
        }
        // Set to true
        instance.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
        assert!(instance.common.disp);
        match instance.get_common_field("DISP") {
            Some(EpicsValue::Char(1)) => {}
            other => panic!("expected Char(1), got {:?}", other),
        }
    }

    // --- PR 1: Hook Framework tests ---

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc as TestArc;

    /// Mock record that tracks special/validate_put/on_put calls via shared counters
    struct HookTrackingRecord {
        val: f64,
        special_before_count: TestArc<AtomicU32>,
        special_after_count: TestArc<AtomicU32>,
        on_put_count: TestArc<AtomicU32>,
        reject_field: Option<String>,
    }

    impl Record for HookTrackingRecord {
        fn record_type(&self) -> &'static str { "test_hook" }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match name {
                "VAL" => {
                    if let EpicsValue::Double(v) = value { self.val = v; Ok(()) }
                    else { Err(CaError::InvalidValue("bad type".into())) }
                }
                _ => Err(CaError::FieldNotFound(name.into())),
            }
        }
        fn field_list(&self) -> &'static [FieldDesc] {
            use crate::types::DbFieldType;
            static FIELDS: &[FieldDesc] = &[
                FieldDesc { name: "VAL", dbf_type: DbFieldType::Double, read_only: false },
            ];
            FIELDS
        }
        fn validate_put(&self, field: &str, _value: &EpicsValue) -> CaResult<()> {
            if let Some(ref reject) = self.reject_field {
                if field == reject {
                    return Err(CaError::InvalidValue("rejected by validate_put".into()));
                }
            }
            Ok(())
        }
        fn on_put(&mut self, _field: &str) {
            self.on_put_count.fetch_add(1, Ordering::SeqCst);
        }
        fn special(&mut self, _field: &str, after: bool) -> CaResult<()> {
            if after {
                self.special_after_count.fetch_add(1, Ordering::SeqCst);
            } else {
                self.special_before_count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[test]
    fn test_special_called_on_common_put() {
        let special_before = TestArc::new(AtomicU32::new(0));
        let special_after = TestArc::new(AtomicU32::new(0));
        let rec = HookTrackingRecord {
            val: 0.0,
            special_before_count: special_before.clone(),
            special_after_count: special_after.clone(),
            on_put_count: TestArc::new(AtomicU32::new(0)),
            reject_field: None,
        };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.put_common_field("DESC", EpicsValue::String("hello".into())).unwrap();
        assert_eq!(special_before.load(Ordering::SeqCst), 1);
        assert_eq!(special_after.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_validate_put_rejects_common_field() {
        let rec = HookTrackingRecord {
            val: 0.0,
            special_before_count: TestArc::new(AtomicU32::new(0)),
            special_after_count: TestArc::new(AtomicU32::new(0)),
            on_put_count: TestArc::new(AtomicU32::new(0)),
            reject_field: Some("SCAN".into()),
        };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into()));
        assert!(result.is_err());
    }

    #[test]
    fn test_on_put_called_for_common_field() {
        let on_put = TestArc::new(AtomicU32::new(0));
        let rec = HookTrackingRecord {
            val: 0.0,
            special_before_count: TestArc::new(AtomicU32::new(0)),
            special_after_count: TestArc::new(AtomicU32::new(0)),
            on_put_count: on_put.clone(),
            reject_field: None,
        };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.put_common_field("DESC", EpicsValue::String("test".into())).unwrap();
        assert_eq!(on_put.load(Ordering::SeqCst), 1);
    }

    // --- PR 2: Scan Index tests ---

    #[test]
    fn test_phas_change_returns_result() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        // Set SCAN to non-Passive
        instance.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        // Now change PHAS
        let result = instance.put_common_field("PHAS", EpicsValue::Short(5)).unwrap();
        assert!(matches!(result, CommonFieldPutResult::PhasChanged { old_phas: 0, new_phas: 5, .. }));
    }

    #[test]
    fn test_phas_change_passive_no_result() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        // SCAN is Passive by default
        let result = instance.put_common_field("PHAS", EpicsValue::Short(5)).unwrap();
        assert_eq!(result, CommonFieldPutResult::NoChange);
    }

    #[test]
    fn test_scan_change_includes_phas() {
        let rec = AiRecord::new(0.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.put_common_field("PHAS", EpicsValue::Short(3)).unwrap();
        let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        match result {
            CommonFieldPutResult::ScanChanged { phas, .. } => assert_eq!(phas, 3),
            other => panic!("expected ScanChanged, got {:?}", other),
        }
    }

    // --- PR 5: UDF Policy tests ---

    struct NoUdfClearRecord { val: f64 }
    impl Record for NoUdfClearRecord {
        fn record_type(&self) -> &'static str { "test_noudf" }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name { "VAL" => Some(EpicsValue::Double(self.val)), _ => None }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match name {
                "VAL" => {
                    if let EpicsValue::Double(v) = value { self.val = v; Ok(()) }
                    else { Err(CaError::InvalidValue("bad".into())) }
                }
                _ => Err(CaError::FieldNotFound(name.into())),
            }
        }
        fn field_list(&self) -> &'static [FieldDesc] { &[] }
        fn clears_udf(&self) -> bool { false }
    }

    #[test]
    fn test_udf_cleared_after_process() {
        let rec = AiRecord::new(1.0);
        let mut instance = RecordInstance::new("TEST".into(), rec);
        assert!(instance.common.udf);
        instance.process_local().unwrap();
        assert!(!instance.common.udf);
    }

    #[test]
    fn test_udf_not_cleared_when_clears_udf_false() {
        let rec = NoUdfClearRecord { val: 1.0 };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        assert!(instance.common.udf);
        instance.process_local().unwrap();
        assert!(instance.common.udf); // UDF stays true
    }

    #[test]
    fn test_udf_alarm_persists() {
        use crate::server::recgbl;
        let rec = NoUdfClearRecord { val: 1.0 };
        let mut instance = RecordInstance::new("TEST".into(), rec);
        instance.common.udf = true;
        instance.process_local().unwrap();
        // UDF should still be true → alarm check should produce UDF_ALARM
        assert!(instance.common.udf);
        // The process_local already ran evaluate_alarms + reset_alarms
        // With clears_udf=false, UDF stays true but evaluate_alarms was called before UDF check
        // Let's verify via another process cycle:
        instance.evaluate_alarms();
        let result = recgbl::rec_gbl_reset_alarms(&mut instance.common);
        assert!(result.alarm_changed || instance.common.sevr == AlarmSeverity::Invalid);
    }

    // ---- PR3: Snapshot generation tests ----

    #[test]
    fn test_snapshot_ai_with_display_metadata() {
        use crate::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(42.0);
        rec.egu = "degC".to_string();
        rec.prec = 3;
        rec.hopr = 100.0;
        rec.lopr = -50.0;
        let mut inst = RecordInstance::new("AI:TEST".into(), rec);
        inst.common.analog_alarm = Some(AnalogAlarmConfig {
            hihi: 90.0, high: 80.0, low: -20.0, lolo: -40.0,
            hhsv: AlarmSeverity::Major, hsv: AlarmSeverity::Minor,
            lsv: AlarmSeverity::Minor, llsv: AlarmSeverity::Major,
        });

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.value, EpicsValue::Double(42.0));
        let disp = snap.display.as_ref().unwrap();
        assert_eq!(disp.units, "degC");
        assert_eq!(disp.precision, 3);
        assert_eq!(disp.upper_disp_limit, 100.0);
        assert_eq!(disp.lower_disp_limit, -50.0);
        assert_eq!(disp.upper_alarm_limit, 90.0);
        assert_eq!(disp.upper_warning_limit, 80.0);
        assert_eq!(disp.lower_warning_limit, -20.0);
        assert_eq!(disp.lower_alarm_limit, -40.0);
        // ai uses HOPR/LOPR as control limits
        let ctrl = snap.control.as_ref().unwrap();
        assert_eq!(ctrl.upper_ctrl_limit, 100.0);
        assert_eq!(ctrl.lower_ctrl_limit, -50.0);
        assert!(snap.enums.is_none());
    }

    #[test]
    fn test_snapshot_ao_with_drvh_drvl() {
        use crate::server::records::ao::AoRecord;
        let mut rec = AoRecord::new(10.0);
        rec.egu = "V".to_string();
        rec.hopr = 100.0;
        rec.lopr = 0.0;
        rec.drvh = 50.0;
        rec.drvl = 5.0;
        let inst = RecordInstance::new("AO:TEST".into(), rec);

        let snap = inst.snapshot_for_field("VAL").unwrap();
        let ctrl = snap.control.as_ref().unwrap();
        assert_eq!(ctrl.upper_ctrl_limit, 50.0);
        assert_eq!(ctrl.lower_ctrl_limit, 5.0);
        let disp = snap.display.as_ref().unwrap();
        assert_eq!(disp.units, "V");
    }

    #[test]
    fn test_snapshot_bi_enum_strings() {
        use crate::server::records::bi::BiRecord;
        let mut rec = BiRecord::new(0);
        rec.znam = "Off".to_string();
        rec.onam = "On".to_string();
        let inst = RecordInstance::new("BI:TEST".into(), rec);

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert!(snap.display.is_none());
        assert!(snap.control.is_none());
        let enums = snap.enums.as_ref().unwrap();
        assert_eq!(enums.strings.len(), 2);
        assert_eq!(enums.strings[0], "Off");
        assert_eq!(enums.strings[1], "On");
    }

    #[test]
    fn test_snapshot_mbbi_16_strings() {
        use crate::server::records::mbbi::MbbiRecord;
        let mut rec = MbbiRecord::default();
        rec.zrst = "Zero".to_string();
        rec.onst = "One".to_string();
        rec.twst = "Two".to_string();
        rec.ffst = "Fifteen".to_string();
        let inst = RecordInstance::new("MBBI:TEST".into(), rec);

        let snap = inst.snapshot_for_field("VAL").unwrap();
        let enums = snap.enums.as_ref().unwrap();
        assert_eq!(enums.strings.len(), 16);
        assert_eq!(enums.strings[0], "Zero");
        assert_eq!(enums.strings[1], "One");
        assert_eq!(enums.strings[2], "Two");
        assert_eq!(enums.strings[15], "Fifteen");
        // Empty strings for unset
        assert_eq!(enums.strings[3], "");
    }

    #[test]
    fn test_snapshot_longin_display() {
        use crate::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(999);
        rec.egu = "counts".to_string();
        rec.hopr = 10000;
        rec.lopr = 0;
        let inst = RecordInstance::new("LONGIN:TEST".into(), rec);

        let snap = inst.snapshot_for_field("VAL").unwrap();
        let disp = snap.display.as_ref().unwrap();
        assert_eq!(disp.units, "counts");
        assert_eq!(disp.precision, 0);
        assert_eq!(disp.upper_disp_limit, 10000.0);
        assert_eq!(disp.lower_disp_limit, 0.0);
        let ctrl = snap.control.as_ref().unwrap();
        assert_eq!(ctrl.upper_ctrl_limit, 10000.0);
        assert_eq!(ctrl.lower_ctrl_limit, 0.0);
    }

    #[test]
    fn test_snapshot_stringin_no_metadata() {
        use crate::server::records::stringin::StringinRecord;
        let rec = StringinRecord::new("hello");
        let inst = RecordInstance::new("SI:TEST".into(), rec);

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.value, EpicsValue::String("hello".to_string()));
        assert!(snap.display.is_none());
        assert!(snap.control.is_none());
        assert!(snap.enums.is_none());
    }

    #[test]
    fn test_snapshot_field_not_found() {
        use crate::server::records::ai::AiRecord;
        let rec = AiRecord::new(1.0);
        let inst = RecordInstance::new("AI:TEST".into(), rec);
        assert!(inst.snapshot_for_field("NONEXISTENT").is_none());
    }

    #[test]
    fn test_snapshot_alarm_state() {
        use crate::server::records::ai::AiRecord;
        let rec = AiRecord::new(1.0);
        let mut inst = RecordInstance::new("AI:TEST".into(), rec);
        inst.common.stat = 7; // HIGH_ALARM
        inst.common.sevr = AlarmSeverity::Minor;

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.alarm.status, 7);
        assert_eq!(snap.alarm.severity, 1); // Minor = 1
    }
}
