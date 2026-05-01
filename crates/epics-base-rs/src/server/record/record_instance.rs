use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use crate::runtime::sync::mpsc;

use crate::error::{CaError, CaResult};
use crate::server::pv::{MonitorEvent, Subscriber};
use crate::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo};
use crate::types::{DbFieldType, EpicsValue};

use super::alarm::{AlarmSeverity, AnalogAlarmConfig};
use super::common_fields::CommonFields;
use super::link::{ParsedLink, parse_link_v2};
use super::record_trait::{
    CommonFieldPutResult, ProcessSnapshot, Record, RecordProcessResult, SubroutineFn,
};
use super::scan::ScanType;

/// Cached metadata for a record.
///
/// Stores the result of `populate_display_info` / `populate_control_info` /
/// `populate_enum_info` so subsequent `snapshot_for_field` /
/// `make_monitor_snapshot` calls can skip rebuilding the metadata. The
/// cache is invalidated whenever a metadata-class field is written
/// (EGU, PREC, HOPR, LOPR, alarm limits, DRVH/DRVL, state strings).
///
/// In a CA-only IOC this is a CPU win; in a hybrid CA + PVA IOC where
/// every snapshot needs full metadata for NTScalar serialization, the
/// cache eliminates redundant per-event populate work.
#[derive(Clone, Default)]
pub(crate) struct MetadataSnapshot {
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
}

/// Returns true if writing to this field should invalidate the metadata
/// cache. Field name is expected uppercase.
///
/// **MUST be kept in sync with `populate_display_info`,
/// `populate_control_info`, and `populate_enum_info`.** If you add a
/// new source field there, add it here too — otherwise the cache will
/// serve stale metadata until some other tracked field is written.
///
/// Currently uncovered (because they are not yet populated by any
/// `populate_*` function): `DESC` (would map to `display.description`
/// — populate hook missing), `Q:form` info tag (would map to
/// `display.form`). Add to this set if/when those are wired up.
fn is_metadata_field(name: &str) -> bool {
    matches!(
        name,
        // Display info (analog + integer + motor)
        "EGU" | "PREC" | "HOPR" | "LOPR" | "HLM" | "LLM"
        // Alarm limits (used by both display and the analog_alarm config)
        | "HIHI" | "HIGH" | "LOW" | "LOLO"
        // Output ctrl limits
        | "DRVH" | "DRVL"
        // bi/bo/busy enum strings
        | "ZNAM" | "ONAM"
        // mbbi/mbbo state strings (16 levels)
        | "ZRST" | "ONST" | "TWST" | "THST" | "FRST" | "FVST" | "SXST" | "SVST"
        | "EIST" | "NIST" | "TEST" | "ELST" | "TVST" | "TTST" | "FTST" | "FFST"
    )
}

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
    pub device: Option<Box<dyn super::super::device_support::DeviceSupport>>,
    // Subroutine (for sub records)
    pub subroutine: Option<Arc<SubroutineFn>>,
    // Re-entrancy guard
    pub processing: AtomicBool,
    // Deferred put_notify completion (fires when async processing completes)
    pub put_notify_tx: Option<crate::runtime::sync::oneshot::Sender<()>>,
    // Last posted values for subscribed fields (generic change detection)
    pub last_posted: HashMap<String, EpicsValue>,
    /// Generation counter for ReprocessAfter timer cancellation.
    /// Bumped each process cycle. Spawned timers check this to avoid
    /// stale re-processes from accumulated timers.
    pub reprocess_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Cached metadata (display/control/enums) — `None` means stale or
    /// not yet built. Populated lazily by `snapshot_for_field` /
    /// `make_monitor_snapshot` and invalidated by `invalidate_metadata_cache`
    /// whenever a metadata-class field (EGU/PREC/HOPR/LOPR/limit/state)
    /// is written.
    ///
    /// Wrapped in `std::sync::Mutex` for interior mutability — the
    /// containing `RecordInstance` is shared via `Arc<RwLock<...>>` from
    /// `PvDatabase`, and snapshot construction holds a read lock; the
    /// inner Mutex lets us still mutate the cache from a `&self` method.
    ///
    /// # Cache invariant (CONTRACT)
    ///
    /// The cache is **only correct under the following contract**: every
    /// code path that mutates a metadata-class field (the set defined in
    /// the file-private `is_metadata_field` predicate) MUST call
    /// [`RecordInstance::notify_field_written`] (or
    /// [`RecordInstance::invalidate_metadata_cache`] directly) afterward.
    ///
    /// All current write paths in `field_io.rs` already do this. If you
    /// add a new code path that:
    ///
    /// - calls `instance.record.put_field(...)` directly, OR
    /// - mutates record fields from inside `Record::process()`,
    ///   `Record::on_put`, or `Record::special` and that mutation could
    ///   touch a metadata-class field, OR
    /// - lets a `Box<dyn Record>` implementation expose its own
    ///   mutation methods that change metadata fields,
    ///
    /// then call `instance.notify_field_written(field_name)` to keep the
    /// cache consistent. Forgetting will produce a stale snapshot —
    /// monitors will continue to see the old EGU/PREC/limits until the
    /// next legitimate metadata-field write triggers invalidation.
    ///
    /// # Symmetric note for `populate_*` extensions
    ///
    /// If a future change adds a new field to `populate_display_info`,
    /// `populate_control_info`, or `populate_enum_info` (e.g. populating
    /// `display.form` from a record's `Q:form` info tag, or
    /// `display.description` from DESC), the new source field name MUST
    /// also be added to `is_metadata_field` so writes to it invalidate
    /// the cache.
    pub(crate) metadata_cache: StdMutex<Option<MetadataSnapshot>>,
}

impl RecordInstance {
    pub fn new(name: String, record: impl Record) -> Self {
        Self::new_boxed(name, Box::new(record))
    }

    pub fn new_boxed(name: String, record: Box<dyn Record>) -> Self {
        let rtype = record.record_type();
        let analog_alarm = match rtype {
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" => {
                Some(AnalogAlarmConfig::default())
            }
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
            put_notify_tx: None,
            last_posted: HashMap::new(),
            reprocess_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metadata_cache: StdMutex::new(None),
        }
    }

    /// Invalidate the metadata cache. Called after writing any
    /// metadata-class field (EGU, PREC, HOPR/LOPR, alarm limits,
    /// DRVH/DRVL, enum strings). The next snapshot will rebuild the
    /// cache from the new values.
    pub fn invalidate_metadata_cache(&self) {
        if let Ok(mut guard) = self.metadata_cache.lock() {
            *guard = None;
        }
    }

    /// Hook called by the database after a field is written. If the
    /// field is in the metadata-class set, the cache is invalidated so
    /// the next snapshot picks up the new value.
    ///
    /// Field name is automatically uppercased.
    pub fn notify_field_written(&self, field: &str) {
        let upper = field.to_ascii_uppercase();
        if is_metadata_field(&upper) {
            self.invalidate_metadata_cache();
        }
    }

    /// Returns the cached MetadataSnapshot, building and storing it on
    /// the first call (or after invalidation). Used by both
    /// `snapshot_for_field` and `make_monitor_snapshot` so the populate
    /// cost is paid at most once per metadata-stable interval.
    fn cached_metadata(&self) -> MetadataSnapshot {
        // Fast path: cache hit
        if let Ok(guard) = self.metadata_cache.lock()
            && let Some(cached) = guard.as_ref()
        {
            return cached.clone();
        }

        // Cache miss: build a fresh metadata snapshot
        let mut tmp = super::super::snapshot::Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        self.populate_display_info(&mut tmp);
        self.populate_control_info(&mut tmp);
        self.populate_enum_info(&mut tmp);

        let meta = MetadataSnapshot {
            display: tmp.display,
            control: tmp.control,
            enums: tmp.enums,
        };

        // Store back; ignore poisoning (cache is best-effort).
        if let Ok(mut guard) = self.metadata_cache.lock() {
            *guard = Some(meta.clone());
        }
        meta
    }

    /// Check if the record is currently processing (PACT equivalent).
    pub fn is_processing(&self) -> bool {
        self.processing.load(std::sync::atomic::Ordering::Acquire)
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
    pub fn snapshot_for_field(&self, field: &str) -> Option<super::super::snapshot::Snapshot> {
        let value = self.resolve_field(field)?;
        let mut snap = super::super::snapshot::Snapshot::new(
            value,
            self.common.stat,
            self.common.sevr as u16,
            self.common.time,
        );

        // Pull display/control/enums from the metadata cache (build on
        // first call, hit thereafter until invalidated by a metadata-class
        // field write).
        let meta = self.cached_metadata();
        snap.display = meta.display;
        snap.control = meta.control;
        snap.enums = meta.enums;

        // Common-field enum mapping (e.g. .SCAN choices) is field-specific
        // and not part of the per-record cache.
        self.populate_common_enum_info(field, &mut snap);
        Some(snap)
    }

    /// Populate DisplayInfo from record fields if applicable.
    fn populate_display_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            "ai" | "ao" | "calc" | "calcout" => {
                let egu = self
                    .record
                    .get_field("EGU")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let prec = self
                    .record
                    .get_field("PREC")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0) as i16;
                let hopr = self
                    .record
                    .get_field("HOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lopr = self
                    .record
                    .get_field("LOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let (hihi, high, low, lolo) = self.alarm_limits();
                snap.display = Some(super::super::snapshot::DisplayInfo {
                    units: egu,
                    precision: prec,
                    upper_disp_limit: hopr,
                    lower_disp_limit: lopr,
                    upper_alarm_limit: hihi,
                    upper_warning_limit: high,
                    lower_warning_limit: low,
                    lower_alarm_limit: lolo,
                    ..Default::default()
                });
            }
            "longin" | "longout" => {
                let egu = self
                    .record
                    .get_field("EGU")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let hopr = self
                    .record
                    .get_field("HOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lopr = self
                    .record
                    .get_field("LOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let (hihi, high, low, lolo) = self.alarm_limits();
                snap.display = Some(super::super::snapshot::DisplayInfo {
                    units: egu,
                    precision: 0,
                    upper_disp_limit: hopr,
                    lower_disp_limit: lopr,
                    upper_alarm_limit: hihi,
                    upper_warning_limit: high,
                    lower_warning_limit: low,
                    lower_alarm_limit: lolo,
                    ..Default::default()
                });
            }
            "motor" => {
                let egu = self
                    .record
                    .get_field("EGU")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let prec = self
                    .record
                    .get_field("PREC")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0) as i16;
                let hlm = self
                    .record
                    .get_field("HLM")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let llm = self
                    .record
                    .get_field("LLM")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                snap.display = Some(super::super::snapshot::DisplayInfo {
                    units: egu,
                    precision: prec,
                    upper_disp_limit: hlm,
                    lower_disp_limit: llm,
                    upper_alarm_limit: 0.0,
                    upper_warning_limit: 0.0,
                    lower_warning_limit: 0.0,
                    lower_alarm_limit: 0.0,
                    ..Default::default()
                });
            }
            _ => {}
        }
    }

    /// Populate ControlInfo from record fields if applicable.
    fn populate_control_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            "ao" | "longout" => {
                // Output records use DRVH/DRVL, fallback to HOPR/LOPR
                let drvh = self.record.get_field("DRVH").and_then(|v| v.to_f64());
                let drvl = self.record.get_field("DRVL").and_then(|v| v.to_f64());
                let hopr = self
                    .record
                    .get_field("HOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lopr = self
                    .record
                    .get_field("LOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                snap.control = Some(super::super::snapshot::ControlInfo {
                    upper_ctrl_limit: drvh.unwrap_or(hopr),
                    lower_ctrl_limit: drvl.unwrap_or(lopr),
                });
            }
            "motor" => {
                // Motor records use HLM/LLM as control limits
                let hlm = self
                    .record
                    .get_field("HLM")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let llm = self
                    .record
                    .get_field("LLM")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                snap.control = Some(super::super::snapshot::ControlInfo {
                    upper_ctrl_limit: hlm,
                    lower_ctrl_limit: llm,
                });
            }
            "ai" | "longin" | "calc" | "calcout" => {
                // Input records use HOPR/LOPR as control limits
                let hopr = self
                    .record
                    .get_field("HOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lopr = self
                    .record
                    .get_field("LOPR")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                snap.control = Some(super::super::snapshot::ControlInfo {
                    upper_ctrl_limit: hopr,
                    lower_ctrl_limit: lopr,
                });
            }
            _ => {}
        }
    }

    /// Populate EnumInfo from record fields if applicable.
    fn populate_enum_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            "bi" | "bo" | "busy" => {
                let znam = self
                    .record
                    .get_field("ZNAM")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let onam = self
                    .record
                    .get_field("ONAM")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                snap.enums = Some(super::super::snapshot::EnumInfo {
                    strings: vec![znam, onam],
                });
            }
            "mbbi" | "mbbo" => {
                let state_fields = [
                    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST", "EIST", "NIST",
                    "TEST", "ELST", "TVST", "TTST", "FTST", "FFST",
                ];
                let strings: Vec<String> = state_fields
                    .iter()
                    .map(|f| {
                        self.record
                            .get_field(f)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default()
                    })
                    .collect();
                snap.enums = Some(super::super::snapshot::EnumInfo { strings });
            }
            _ => {}
        }
    }

    /// Populate enum strings for common fields accessed via CA (e.g. .SCAN).
    fn populate_common_enum_info(&self, field: &str, snap: &mut super::super::snapshot::Snapshot) {
        match field {
            "SCAN" => {
                snap.enums = Some(super::super::snapshot::EnumInfo {
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
                if self.processing.load(std::sync::atomic::Ordering::Acquire) {
                    1
                } else {
                    0
                },
            )),
            "PROC" => Some(EpicsValue::Char(0)), // Always 0 (trigger-only)
            // Analog alarm fields
            "HIHI" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Double(a.hihi)),
            "HIGH" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Double(a.high)),
            "LOW" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Double(a.low)),
            "LOLO" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Double(a.lolo)),
            "HHSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.hhsv as i16)),
            "HSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.hsv as i16)),
            "LSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.lsv as i16)),
            "LLSV" => self
                .common
                .analog_alarm
                .as_ref()
                .map(|a| EpicsValue::Short(a.llsv as i16)),
            _ => None,
        }
    }

    /// Set a common field value. Returns what scan index changes are needed.
    pub fn put_common_field(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> CaResult<CommonFieldPutResult> {
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
                    let sev = AlarmSeverity::from_u16(v as u16);
                    // Writing ACKS clears alarm acknowledge if written severity >= current
                    if sev >= self.common.sevr {
                        self.common.acks = AlarmSeverity::NoAlarm;
                    }
                }
            }
            "ACKT" => match value {
                EpicsValue::Char(v) => self.common.ackt = v != 0,
                EpicsValue::Short(v) => self.common.ackt = v != 0,
                _ => {}
            },
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
                    return Ok(CommonFieldPutResult::ScanChanged {
                        old_scan,
                        new_scan,
                        phas,
                    });
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
                    // C parity (acd1aef): CP/CPP modifiers on output links are
                    // meaningless (they request "process on change" semantics
                    // that only apply to input links). dbParseLink in C strips
                    // them and emits an errlogPrintf warning naming the source
                    // record and field. Mirror the diagnostic here.
                    let trimmed = s.trim_end();
                    if trimmed.ends_with(" CP") || trimmed.ends_with(" CPP") {
                        tracing::warn!(
                            target: "epics_base_rs::record",
                            record = %name,
                            field = "OUT",
                            "CP/CPP modifier ignored on output link"
                        );
                    }
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
                        return Ok(CommonFieldPutResult::PhasChanged {
                            scan,
                            old_phas,
                            new_phas: v,
                        });
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
                if let EpicsValue::Short(v) = value {
                    self.common.lcnt = v;
                }
            }
            "DISP" => match value {
                EpicsValue::Char(v) => self.common.disp = v != 0,
                EpicsValue::Short(v) => self.common.disp = v != 0,
                _ => {}
            },
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

        // Check CALC_ALARM for calc/calcout records
        let rtype = self.record.record_type();
        if rtype == "calc" || rtype == "calcout" || rtype == "scalcout" {
            // calc_alarm is exposed as a boolean field - check it
            if let Some(EpicsValue::Char(1)) = self.record.get_field("CALC_ALARM") {
                recgbl::rec_gbl_set_sevr(
                    &mut self.common,
                    alarm_status::CALC_ALARM,
                    crate::server::record::AlarmSeverity::Invalid,
                );
            }
        }

        match rtype {
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" => {
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
                let zsv = self
                    .record
                    .get_field("ZSV")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let osv = self
                    .record
                    .get_field("OSV")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let cosv = self
                    .record
                    .get_field("COSV")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);

                // Guard: val > 1 means no alarm check (like C)
                if val <= 1 {
                    // State alarm: ZSV for val==0, OSV for val==1
                    let state_sev = if val == 0 { zsv } else { osv };
                    let sev = AlarmSeverity::from_u16(state_sev as u16);
                    if sev != AlarmSeverity::NoAlarm {
                        recgbl::rec_gbl_set_sevr(&mut self.common, alarm_status::STATE_ALARM, sev);
                    }

                    // COS alarm: only fires when val changed from LALM
                    let lalm = self
                        .record
                        .get_field("LALM")
                        .and_then(|v| {
                            if let EpicsValue::Enum(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(val);

                    if val != lalm {
                        let cos_sev = AlarmSeverity::from_u16(cosv as u16);
                        if cos_sev != AlarmSeverity::NoAlarm {
                            recgbl::rec_gbl_set_sevr(
                                &mut self.common,
                                alarm_status::COS_ALARM,
                                cos_sev,
                            );
                        }
                        let _ = self.record.put_field("LALM", EpicsValue::Enum(val));
                    }
                }
            }
            "mbbi" | "mbbo" => {
                let val = match self.record.val() {
                    Some(EpicsValue::Enum(v)) => v as usize,
                    _ => return,
                };
                let sv_fields = [
                    "ZRSV", "ONSV", "TWSV", "THSV", "FRSV", "FVSV", "SXSV", "SVSV", "EISV", "NISV",
                    "TESV", "ELSV", "TVSV", "TTSV", "FTSV", "FFSV",
                ];
                let unsv = self
                    .record
                    .get_field("UNSV")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let cosv = self
                    .record
                    .get_field("COSV")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);

                // State alarm: per-state severity or UNSV for unknown states
                let state_sev = if val < 16 {
                    self.record
                        .get_field(sv_fields[val])
                        .and_then(|v| {
                            if let EpicsValue::Short(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0)
                } else {
                    unsv
                };

                let sev = AlarmSeverity::from_u16(state_sev as u16);
                if sev != AlarmSeverity::NoAlarm {
                    recgbl::rec_gbl_set_sevr(&mut self.common, alarm_status::STATE_ALARM, sev);
                }

                // COS alarm: only when val changed from LALM (like bi/bo)
                let lalm = self
                    .record
                    .get_field("LALM")
                    .and_then(|v| {
                        if let EpicsValue::Enum(s) = v {
                            Some(s as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(val);

                if val != lalm {
                    let cos_sev = AlarmSeverity::from_u16(cosv as u16);
                    if cos_sev != AlarmSeverity::NoAlarm {
                        recgbl::rec_gbl_set_sevr(
                            &mut self.common,
                            alarm_status::COS_ALARM,
                            cos_sev,
                        );
                    }
                    let _ = self.record.put_field("LALM", EpicsValue::Enum(val as u16));
                }
            }
            _ => {} // no-op for other types
        }
    }

    fn evaluate_analog_alarm(&mut self, val: f64, cfg: &AnalogAlarmConfig) {
        use crate::server::recgbl::{self, alarm_status};

        let hyst = self.common.hyst;
        let lalm = self
            .record
            .get_field("LALM")
            .and_then(|v| v.to_f64())
            .unwrap_or(val);

        // C-style per-level hysteresis: alarm fires if val passes the level,
        // OR if we were already at that alarm level (lalm == alev) and val
        // hasn't retreated past the hysteresis margin.
        let (new_sevr, new_stat, alev) = if cfg.hhsv != AlarmSeverity::NoAlarm
            && (val >= cfg.hihi || (lalm == cfg.hihi && val >= cfg.hihi - hyst))
        {
            (cfg.hhsv, alarm_status::HIHI_ALARM, cfg.hihi)
        } else if cfg.llsv != AlarmSeverity::NoAlarm
            && (val <= cfg.lolo || (lalm == cfg.lolo && val <= cfg.lolo + hyst))
        {
            (cfg.llsv, alarm_status::LOLO_ALARM, cfg.lolo)
        } else if cfg.hsv != AlarmSeverity::NoAlarm
            && (val >= cfg.high || (lalm == cfg.high && val >= cfg.high - hyst))
        {
            (cfg.hsv, alarm_status::HIGH_ALARM, cfg.high)
        } else if cfg.lsv != AlarmSeverity::NoAlarm
            && (val <= cfg.low || (lalm == cfg.low && val <= cfg.low + hyst))
        {
            (cfg.lsv, alarm_status::LOW_ALARM, cfg.low)
        } else {
            (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, 0.0)
        };

        if new_sevr != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(&mut self.common, new_stat, new_sevr);
            // C sets LALM to the alarm threshold level, not the current value
            let _ = self.record.put_field("LALM", EpicsValue::Double(alev));
        } else {
            // No alarm condition: reset LALM to current value (like C)
            let _ = self.record.put_field("LALM", EpicsValue::Double(val));
        }
    }

    /// Basic process: process record, evaluate alarms, timestamp, build snapshot.
    /// This does NOT handle links — see process_with_context in database.rs.
    pub fn process_local(&mut self) -> CaResult<ProcessSnapshot> {
        use crate::server::recgbl::{self, EventMask};
        const LCNT_ALARM_THRESHOLD: i16 = 10;

        if self
            .processing
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
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
        // RAII guard that resets `self.processing` to false on drop —
        // both for the normal exit path and for any `?` early return.
        // The guard holds a raw pointer rather than a reference because
        // we still need `self` mutably while the guard is alive (the
        // record body below mutates other `self` fields).
        struct ProcessGuard(*const AtomicBool);
        // SAFETY: AtomicBool is Sync; raw pointers don't auto-derive
        // Send. We hand-roll Send because the ptr targets a field of
        // `self`, which the caller already proves can be borrowed
        // through this code path. The pointer is only ever read for an
        // atomic store, never written, dereferenced for raw access, or
        // escaped from this scope.
        unsafe impl Send for ProcessGuard {}
        impl Drop for ProcessGuard {
            fn drop(&mut self) {
                // SAFETY: `self.0` was constructed from
                // `&self.processing as *const AtomicBool` below, where
                // `self` is the live RecordInstance whose lifetime
                // strictly outlives `_guard`. RecordInstance is
                // !Unpin-equivalent in practice (we never move it
                // while held in the database's `Arc<RwLock<_>>`), so
                // the pointer remains valid until Drop runs.
                unsafe { &*self.0 }.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _guard = ProcessGuard(&self.processing as *const AtomicBool);

        // Call subroutine if registered (for sub records)
        if let Some(ref sub_fn) = self.subroutine {
            sub_fn(&mut *self.record)?;
        }
        let outcome = self.record.process()?;
        let process_result = outcome.result;
        // Note: process_local() does not execute ProcessActions — those are
        // handled by the full process_record_with_links() path in processing.rs.

        // If the record reports it modified a metadata-class field during
        // process(), invalidate the metadata cache so the next snapshot
        // rebuilds from the new values. Default impl returns false, so
        // most records pay zero cost here.
        if self.record.took_metadata_change() {
            self.invalidate_metadata_cache();
        }

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
            // Filter out fields that haven't actually changed, and update
            // MLST/last_posted for those that have.
            let mut changed_fields = Vec::new();
            for (name, val) in fields {
                let changed = match self.last_posted.get(&name) {
                    Some(prev) => prev != &val,
                    None => true,
                };
                if changed {
                    if name == "VAL" {
                        if let Some(f) = val.to_f64() {
                            self.put_coerced("MLST", f);
                            self.common.mlst = Some(f);
                        }
                    }
                    self.last_posted.insert(name.clone(), val.clone());
                    changed_fields.push((name, val));
                }
            }
            let event_mask = if changed_fields.is_empty() {
                EventMask::NONE
            } else {
                EventMask::VALUE | EventMask::ALARM
            };
            // _guard drops here, clearing the processing flag
            return Ok(ProcessSnapshot {
                changed_fields,
                event_mask,
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
        if alarm_result.alarm_changed {
            changed_fields.push((
                "SEVR".to_string(),
                EpicsValue::Short(self.common.sevr as i16),
            ));
            changed_fields.push((
                "STAT".to_string(),
                EpicsValue::Short(self.common.stat as i16),
            ));
        }

        // Add subscribed fields that actually changed since last notification.
        let mut sub_updates: Vec<(String, EpicsValue)> = Vec::new();
        for (field, subs) in &self.subscribers {
            if !subs.is_empty() && field != "VAL" && field != "SEVR" && field != "STAT" {
                if let Some(val) = self.resolve_field(field) {
                    let changed = match self.last_posted.get(field) {
                        Some(prev) => prev != &val,
                        None => true,
                    };
                    if changed {
                        sub_updates.push((field.clone(), val));
                    }
                }
            }
        }
        if !sub_updates.is_empty() {
            for (field, val) in &sub_updates {
                self.last_posted.insert(field.clone(), val.clone());
            }
            changed_fields.extend(sub_updates);
            event_mask |= EventMask::VALUE;
        }

        Ok(ProcessSnapshot {
            changed_fields,
            event_mask,
        })
    }

    /// Check deadband (MDEL/ADEL) for VAL monitor/archive filtering.
    /// Returns (monitor_trigger, archive_trigger).
    /// Updates ALST/MLST in the record when triggered.
    /// For records without MDEL/ADEL fields (e.g. motor), defaults to MDEL=0
    /// (trigger on any actual change) and uses CommonFields.mlst/alst as fallback.
    /// Put a f64 value into a record field, coercing to the field's native type.
    pub(crate) fn put_coerced(&mut self, field: &str, val: f64) {
        use crate::types::EpicsValue;
        let target_type = self
            .record
            .get_field(field)
            .map(|v| v.db_field_type())
            .unwrap_or(crate::types::DbFieldType::Double);
        let coerced = EpicsValue::Double(val).convert_to(target_type);
        let _ = self.record.put_field(field, coerced);
    }

    pub fn check_deadband_ext(&mut self) -> (bool, bool) {
        let val = match self.record.val().and_then(|v| v.to_f64()) {
            Some(v) => v,
            None => return (true, true),
        };

        let mdel = self
            .record
            .get_field("MDEL")
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0);
        let adel = self
            .record
            .get_field("ADEL")
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0);

        // Use record's MLST/ALST fields if available, otherwise fall back to CommonFields
        let mlst = self
            .record
            .get_field("MLST")
            .and_then(|v| v.to_f64())
            .or(self.common.mlst)
            .unwrap_or(f64::NAN);
        let alst = self
            .record
            .get_field("ALST")
            .and_then(|v| v.to_f64())
            .or(self.common.alst)
            .unwrap_or(f64::NAN);

        let monitor_trigger = mdel < 0.0 || mlst.is_nan() || (val - mlst).abs() > mdel;
        let archive_trigger = adel < 0.0 || alst.is_nan() || (val - alst).abs() > adel;

        if archive_trigger {
            self.put_coerced("ALST", val);
            self.common.alst = Some(val);
        }
        if monitor_trigger {
            self.put_coerced("MLST", val);
            self.common.mlst = Some(val);
        }

        (monitor_trigger, archive_trigger)
    }

    /// Build a Snapshot for a given value, populated with the record's display metadata.
    /// Uses the metadata cache so the populate cost is paid at most once
    /// per metadata-stable interval (cf. `cached_metadata`).
    fn make_monitor_snapshot(&self, value: EpicsValue) -> super::super::snapshot::Snapshot {
        let mut snap = super::super::snapshot::Snapshot::new(
            value,
            self.common.stat,
            self.common.sevr as u16,
            self.common.time,
        );
        let meta = self.cached_metadata();
        snap.display = meta.display;
        snap.control = meta.control;
        snap.enums = meta.enums;
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
                        let event = MonitorEvent {
                            snapshot: mon_snap.clone(),
                            origin: 0,
                        };
                        if sub.tx.try_send(event.clone()).is_err() {
                            if let Ok(mut slot) = sub.coalesced.lock() {
                                *slot = Some(event);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Notify subscribers of a specific field, filtering by event mask.
    pub fn notify_field(&self, field: &str, mask: crate::server::recgbl::EventMask) {
        self.notify_field_with_origin(field, mask, 0);
    }

    /// Notify subscribers with an origin tag for self-write filtering.
    pub fn notify_field_with_origin(
        &self,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
    ) {
        if let Some(subs) = self.subscribers.get(field) {
            if let Some(value) = self.resolve_field(field) {
                let mon_snap = self.make_monitor_snapshot(value);
                for sub in subs {
                    let sub_mask = crate::server::recgbl::EventMask::from_bits(sub.mask);
                    if mask.is_empty() || sub_mask.intersects(mask) {
                        let event = MonitorEvent {
                            snapshot: mon_snap.clone(),
                            origin,
                        };
                        if sub.tx.try_send(event.clone()).is_err() {
                            if let Ok(mut slot) = sub.coalesced.lock() {
                                *slot = Some(event);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Add a subscriber for a specific field. Returns `None` when the
    /// per-field subscriber cap (`EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`)
    /// is reached. C-G15: the parallel cap on `ProcessVariable`
    /// (P-G14) defends against a misbehaving client opening many
    /// MONITOR ops against one shared PV; the same defence is needed
    /// for record fields, which the CA server's
    /// `ChannelTarget::RecordField` path lands on.
    pub fn add_subscriber(
        &mut self,
        field: &str,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<mpsc::Receiver<MonitorEvent>> {
        let cap = crate::server::pv::max_subscribers_per_pv();
        let field_str = field.to_string();
        let bucket = self.subscribers.entry(field_str.clone()).or_default();
        if bucket.len() >= cap {
            tracing::warn!(
                record = %self.name,
                field = %field_str,
                live = bucket.len(),
                cap,
                "record field subscriber cap reached, refusing add_subscriber"
            );
            return None;
        }
        let (tx, rx) = mpsc::channel(64);
        bucket.push(Subscriber {
            sid,
            data_type,
            mask,
            tx,
            coalesced: std::sync::Arc::new(std::sync::Mutex::new(None)),
        });
        // Initialize last_posted with current value so the first process cycle
        // doesn't treat it as "changed" (the initial value is already sent
        // to the client as part of EVENT_ADD response).
        if !self.last_posted.contains_key(&field_str) {
            if let Some(val) = self.resolve_field(&field_str) {
                self.last_posted.insert(field_str, val);
            }
        }
        Some(rx)
    }

    /// Remove a subscriber by subscription ID from all fields.
    pub fn remove_subscriber(&mut self, sid: u32) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| s.sid != sid);
        }
    }

    /// Take any pending coalesced overflow event for `sid` across all
    /// fields. Drops-oldest semantics: if the per-subscriber mpsc filled
    /// while the consumer was slow, the newest event was stashed in the
    /// coalesce slot and is returned here.
    pub fn pop_coalesced(&self, sid: u32) -> Option<MonitorEvent> {
        for subs in self.subscribers.values() {
            for sub in subs {
                if sub.sid == sid {
                    if let Ok(mut slot) = sub.coalesced.lock() {
                        if let Some(ev) = slot.take() {
                            return Some(ev);
                        }
                    }
                }
            }
        }
        None
    }

    /// Clean up closed subscriber channels.
    pub fn cleanup_subscribers(&mut self) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| !s.tx.is_closed());
        }
    }
}

#[cfg(test)]
mod metadata_cache_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    /// Helper: build an AiRecord wrapped in a RecordInstance with EGU/PREC/HOPR/LOPR set.
    fn ai_instance() -> RecordInstance {
        let mut rec = AiRecord::default();
        let _ = rec.put_field("EGU", EpicsValue::String("degC".into()));
        let _ = rec.put_field("PREC", EpicsValue::Short(2));
        let _ = rec.put_field("HOPR", EpicsValue::Double(100.0));
        let _ = rec.put_field("LOPR", EpicsValue::Double(0.0));
        let _ = rec.put_field("VAL", EpicsValue::Double(25.0));
        RecordInstance::new("TEMP".to_string(), rec)
    }

    #[test]
    fn metadata_field_set_check() {
        // Sanity check that the metadata field set is recognized.
        assert!(is_metadata_field("EGU"));
        assert!(is_metadata_field("PREC"));
        assert!(is_metadata_field("HOPR"));
        assert!(is_metadata_field("LOPR"));
        assert!(is_metadata_field("HIHI"));
        assert!(is_metadata_field("DRVH"));
        assert!(is_metadata_field("ZNAM"));
        assert!(is_metadata_field("ZRST"));
        assert!(is_metadata_field("FFST"));

        // Non-metadata fields should NOT invalidate the cache
        assert!(!is_metadata_field("VAL"));
        assert!(!is_metadata_field("DESC"));
        assert!(!is_metadata_field("SCAN"));
        assert!(!is_metadata_field("PHAS"));
    }

    #[test]
    fn cache_starts_empty_then_populates_on_first_snapshot() {
        let inst = ai_instance();

        // Cache starts empty
        assert!(inst.metadata_cache.lock().unwrap().is_none());

        // First snapshot triggers populate + cache store
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let display = snap.display.expect("ai snapshot must have display");
        assert_eq!(display.units, "degC");
        assert_eq!(display.precision, 2);
        assert_eq!(display.upper_disp_limit, 100.0);
        assert_eq!(display.lower_disp_limit, 0.0);

        // Cache is now populated
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }

    #[test]
    fn cache_hit_returns_same_metadata() {
        let inst = ai_instance();

        // Prime the cache
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        let display1 = snap1.display.unwrap();

        // Subsequent snapshots return the same cached metadata
        let snap2 = inst.snapshot_for_field("VAL").unwrap();
        let display2 = snap2.display.unwrap();

        assert_eq!(display1.units, display2.units);
        assert_eq!(display1.precision, display2.precision);
        assert_eq!(display1.upper_disp_limit, display2.upper_disp_limit);
        assert_eq!(display1.lower_disp_limit, display2.lower_disp_limit);
    }

    #[test]
    fn invalidate_clears_cache() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        inst.invalidate_metadata_cache();
        assert!(inst.metadata_cache.lock().unwrap().is_none());
    }

    #[test]
    fn notify_field_written_invalidates_for_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Writing a metadata field should invalidate
        inst.notify_field_written("EGU");
        assert!(inst.metadata_cache.lock().unwrap().is_none());
    }

    #[test]
    fn notify_field_written_skips_non_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Writing a value field should NOT invalidate the cache
        inst.notify_field_written("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Same for DESC
        inst.notify_field_written("DESC");
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }

    #[test]
    fn notify_field_written_is_case_insensitive() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Lowercase metadata field name should still trigger invalidation
        inst.notify_field_written("egu");
        assert!(inst.metadata_cache.lock().unwrap().is_none());
    }

    #[test]
    fn cache_picks_up_new_value_after_invalidation() {
        let mut inst = ai_instance();

        // First snapshot: degC
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap1.display.unwrap().units, "degC");

        // Mutate EGU and invalidate
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("mV".into()));
        inst.notify_field_written("EGU");

        // Second snapshot: mV (rebuilt)
        let snap2 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap2.display.unwrap().units, "mV");
    }

    #[test]
    fn make_monitor_snapshot_uses_cache() {
        let inst = ai_instance();
        assert!(inst.metadata_cache.lock().unwrap().is_none());

        // make_monitor_snapshot should also populate the cache
        let snap = inst.make_monitor_snapshot(EpicsValue::Double(42.0));
        assert!(snap.display.is_some());
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Subsequent call hits cache
        let snap2 = inst.make_monitor_snapshot(EpicsValue::Double(43.0));
        let d1 = snap.display.unwrap();
        let d2 = snap2.display.unwrap();
        assert_eq!(d1.units, d2.units);
        assert_eq!(d1.precision, d2.precision);
    }

    /// Stub record that simulates a record whose process() mutates an
    /// internal metadata field. Used to verify that the
    /// `Record::took_metadata_change()` hook actually triggers cache
    /// invalidation in `process_local()`.
    struct MutatingMetaRecord {
        val: f64,
        egu: String,
        took_change: bool,
    }

    impl Record for MutatingMetaRecord {
        fn record_type(&self) -> &'static str {
            "ai" // pretend to be ai so populate_display_info populates EGU
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            // Simulate dynamic metadata change inside processing
            self.egu = "kV".to_string();
            self.took_change = true;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "EGU" => Some(EpicsValue::String(self.egu.clone())),
                "PREC" => Some(EpicsValue::Short(0)),
                "HOPR" => Some(EpicsValue::Double(0.0)),
                "LOPR" => Some(EpicsValue::Double(0.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("VAL", EpicsValue::Double(v)) => {
                    self.val = v;
                    Ok(())
                }
                ("EGU", EpicsValue::String(s)) => {
                    self.egu = s;
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        fn took_metadata_change(&mut self) -> bool {
            let was = self.took_change;
            self.took_change = false; // reset after reporting
            was
        }
    }

    #[test]
    fn process_local_invalidates_cache_on_took_metadata_change() {
        let mut inst = RecordInstance::new(
            "MUT".to_string(),
            MutatingMetaRecord {
                val: 1.0,
                egu: "V".to_string(),
                took_change: false,
            },
        );

        // Build the cache once with the original EGU
        let snap1 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap1.display.unwrap().units, "V");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Run process_local — the stub record sets took_change inside process()
        let _ = inst.process_local();

        // Cache should now be invalidated (took_metadata_change returned true)
        assert!(
            inst.metadata_cache.lock().unwrap().is_none(),
            "process_local should invalidate cache when took_metadata_change is true"
        );

        // Next snapshot picks up the new EGU
        let snap2 = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap2.display.unwrap().units, "kV");
    }

    /// Stub record that does NOT mutate metadata fields. Verifies the
    /// default `took_metadata_change` returns false and the cache stays.
    struct StableMetaRecord {
        val: f64,
    }
    impl Record for StableMetaRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            self.val += 1.0;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "EGU" => Some(EpicsValue::String("V".into())),
                "PREC" => Some(EpicsValue::Short(0)),
                "HOPR" => Some(EpicsValue::Double(0.0)),
                "LOPR" => Some(EpicsValue::Double(0.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
            Ok(())
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        // took_metadata_change uses default impl (returns false)
    }

    #[test]
    fn process_local_keeps_cache_when_no_metadata_change() {
        let mut inst = RecordInstance::new("STABLE".to_string(), StableMetaRecord { val: 0.0 });

        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Run process_local several times — cache should remain intact
        let _ = inst.process_local();
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        let _ = inst.process_local();
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        let _ = inst.process_local();
        assert!(inst.metadata_cache.lock().unwrap().is_some());
    }
}
