use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::error::{CaError, CaResult};
use crate::server::event_queue::{EventReader, EventUser};
use crate::server::pv::{MonitorEvent, Subscriber};
use crate::server::recgbl::EventMask;
use crate::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::alarm::{AlarmSeverity, AnalogAlarmConfig};
use super::common_fields::CommonFields;
use super::link::{
    ParsedLink, out_link_discards_cp, parse_forward_link_v2, parse_link_v2, parse_output_link_v2,
};
use super::menu_choices::MenuBound;
use super::pini::PiniMode;
use super::record_trait::{
    AuxPostMask, CommonFieldPutResult, ProcessSnapshot, Record, RecordProcessResult, SubroutineFn,
};
use super::scan::{ScanType, SimModeScan};

/// Put-notify completion wait-set — the C `dbNotify.c` `processNotify`
/// waitList analogue (`dbNotifyAdd` / `dbNotifyCompletion`).
///
/// A `ca_put_callback` / WRITE_NOTIFY completion must fire only after the
/// originating (put-target) record AND every record reached through its
/// FLNK / OUT / process-action dispatch chain (synchronous *or* async)
/// has finished processing. A single wait-set owns the completion
/// oneshot; only it fires, and only when the last chain member leaves.
///
/// Counting convention: [`Self::new`] arms `pending = 1` for the
/// originating record (which always joins). Every additional PP target
/// that will process under the active notify [`Self::enter`]s on join
/// (C `dbNotifyAdd`), and every record [`Self::leave`]s when its
/// processing completes (C `dbNotifyCompletion`). The oneshot fires on
/// the `leave` that drops `pending` to zero.
pub struct NotifyWaitSet {
    pending: AtomicUsize,
    tx: StdMutex<Option<crate::runtime::sync::oneshot::Sender<()>>>,
}

impl NotifyWaitSet {
    /// Arm a wait-set whose `tx` fires when the chain settles. `pending`
    /// starts at 1 for the originating record — its completion `leave`s
    /// that implicit slot, so a put with no chain targets fires
    /// immediately on the originating record's own completion.
    pub fn new(tx: crate::runtime::sync::oneshot::Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(1),
            tx: StdMutex::new(Some(tx)),
        })
    }

    /// A PP target joined the chain (C `dbNotifyAdd`). Balanced by exactly
    /// one [`Self::leave`].
    pub fn enter(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
    }

    /// A record finished its contribution (C `dbNotifyCompletion`). Fires
    /// the completion oneshot on the `leave` that empties the set.
    pub fn leave(&self) {
        let prev = self.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev >= 1, "NotifyWaitSet::leave underflow");
        if prev == 1 {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    /// True once every chain member has left (the completion has fired).
    /// Used by the put entry to decide synchronous (return `None`) vs
    /// async-pending (return the receiver) completion.
    pub fn completed(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }
}

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

/// Returns true if this field is property-class — the C `prop(YES)`
/// dbd attribute: writing a changed value posts `DBE_PROPERTY` to the
/// record's subscribers AND invalidates the metadata cache. Field name
/// is expected uppercase.
///
/// **Every field read by `populate_display_info`,
/// `populate_control_info`, or `populate_enum_info` MUST be in this
/// set** — otherwise the cache serves stale metadata until some other
/// tracked field is written. The reverse does not hold: a field may be
/// property-class without being a cache source (e.g. the motor fields
/// below feed the live-computed `field_metadata_override`, never the
/// cache — its invalidation on their write is harmless).
///
/// Currently uncovered (because it is not yet populated by any
/// `populate_*` function): `DESC` (would map to `display.description`
/// — populate hook missing). The `Q:form` info tag is now wired
/// (`populate_display_info` -> `display.form`), but as an immutable
/// load-time info tag — not a runtime field — it needs no cache
/// invalidation and so is intentionally absent from this field set.
fn is_metadata_field(name: &str) -> bool {
    matches!(
        name,
        // Display info (analog + integer + motor) — `prop(YES)` in
        // ai/ao/longin/longout DBDs.
        "EGU" | "PREC" | "HOPR" | "LOPR" | "HLM" | "LLM"
        // Alarm limits (used by both display and the analog_alarm config) —
        // ai/ao/longin/longout `prop(YES)`.
        | "HIHI" | "HIGH" | "LOW" | "LOLO"
        // Alarm severities for the four limit thresholds —
        // ai/ao/longin/longout `prop(YES)` per upstream DBDs
        // (`aiRecord.dbd.pod` lines 357-388).
        | "HHSV" | "HSV" | "LSV" | "LLSV"
        // Output ctrl limits — ao/longout `prop(YES)`.
        | "DRVH" | "DRVL"
        // motor `prop(YES)` (`motorRecord.dbd` 154/161/289/361/368):
        // VBAS/VMAX bound VELO's range, MRES the RVAL/RRBV raw range,
        // DHLM/DLLM the DVAL/DRBV range — all served per field by
        // `Record::field_metadata_override` (C get_graphic_double /
        // get_control_double). HLM/LLM/EGU/PREC and the alarm limits
        // are motor `prop(YES)` too, already listed above.
        | "VBAS" | "VMAX" | "MRES" | "DHLM" | "DLLM"
        // bi/bo/busy enum strings — `prop(YES)`.
        | "ZNAM" | "ONAM"
        // bi/bo state severities — `biRecord.dbd.pod` / `boRecord.dbd.pod`
        // `prop(YES)` for ZSV/OSV/COSV (zero / one / change-of-state).
        | "ZSV" | "OSV" | "COSV"
        // mbbi/mbbo state strings (16 levels) — `prop(YES)`.
        | "ZRST" | "ONST" | "TWST" | "THST" | "FRST" | "FVST" | "SXST" | "SVST"
        | "EIST" | "NIST" | "TEST" | "ELST" | "TVST" | "TTST" | "FTST" | "FFST"
    )
}

/// One alarm limit for a DBR_AL_DOUBLE response: the value when its
/// severity threshold is enabled, `NaN` otherwise. Mirrors C
/// `get_alarm_double`'s `prec->hhsv ? prec->hihi : epicsNAN`.
fn gated(severity: AlarmSeverity, limit: f64) -> f64 {
    if severity != AlarmSeverity::NoAlarm {
        limit
    } else {
        f64::NAN
    }
}

fn parse_alarm_severity(value: &EpicsValue) -> AlarmSeverity {
    match value {
        EpicsValue::Short(v) => AlarmSeverity::from_u16(*v as u16),
        EpicsValue::String(s) => AlarmSeverity::from_u16(match s.as_str_lossy().as_ref() {
            "NO_ALARM" => 0,
            "MINOR" => 1,
            "MAJOR" => 2,
            "INVALID" => 3,
            other => other.parse::<u16>().unwrap_or(0),
        }),
        other => AlarmSeverity::from_u16(other.to_f64().unwrap_or(0.0) as u16),
    }
}

/// Coerce a db-loaded `String` for a numeric/menu **common** field to that
/// field's canonical DBF type before [`RecordInstance::put_common_field`]
/// dispatches on it.
///
/// The db loader applies a record's own fields with the typed
/// `EpicsValue::parse(desc.dbf_type, value_str)` (`db_loader::apply_fields`),
/// but a field absent from `field_list` is pushed to the common-field path as
/// a raw `EpicsValue::String` — it has no `FieldDesc` to parse against. The
/// numeric common-field arms in `put_common_field` match only their typed
/// variant, so without this step a `.db` `field(PHAS, "1")`,
/// `field(PRIO, "HIGH")`, `field(DISS, "MAJOR")`, `field(DISA, "1")`, … is
/// silently dropped at IOC load. Routing the String through the same
/// `EpicsValue::parse` the record-field path uses handles the numeric *and*
/// menu-label forms uniformly, so the arm receives the value it expects.
///
/// Only fields whose canonical type is numeric/menu are listed; the
/// Port of libcom `epicsParseInt32(str, &to, 10, NULL)`
/// (`libcom/src/misc/epicsStdlib.c:26-53,245-261`), which is how pvxs parses
/// the `nsec:lsb:` digit count. Returns `None` for every status the C
/// returns non-zero for:
///
/// - `S_stdlib_noConversion` — `strtol` consumed nothing (empty / no digits)
/// - `S_stdlib_extraneous` — trailing non-space bytes with `units == NULL`
/// - `S_stdlib_overflow` — outside `epicsInt32`
///
/// Leading and trailing ASCII whitespace and a leading `+`/`-` sign are
/// accepted, matching `epicsParseLong`'s `isspace` skips and `strtol`.
fn epics_parse_int32_base10(s: &str) -> Option<i32> {
    // `while ((c = *str) && isspace(c)) ++str;` then `strtol(str, &endp, 10)`.
    let body = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (sign, digits) = match body.strip_prefix(['+', '-']) {
        Some(rest) if body.starts_with('-') => (-1i64, rest),
        Some(rest) => (1i64, rest),
        None => (1i64, body),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None; // endp == str → S_stdlib_noConversion
    }
    // `if (c && !units) return S_stdlib_extraneous;` after skipping trailing
    // whitespace.
    if !digits[end..]
        .trim_start_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return None;
    }
    // ERANGE from `strtol`, then the explicit `epicsInt32` range check.
    let magnitude: i64 = digits[..end].parse().ok()?;
    i32::try_from(sign * magnitude).ok()
}

/// String-typed common fields (DESC, ASG, OUT, TSEL, …) and already-typed
/// non-String writes pass through untouched, and an unparseable String is
/// returned as-is so the arm drops it exactly as before. The runtime
/// alarm-output fields (SEVR/STAT/NSEV/NSTA/ACKS) and debug flags
/// (RPRO/TPRO/BKPT) are deliberately omitted: they are recomputed every
/// process, not `.db` init directives, so coercing a loaded value would be
/// overwritten immediately.
fn coerce_common_field_string(
    name: &str,
    value: EpicsValue,
    bound: MenuBound,
) -> CaResult<EpicsValue> {
    let s = match &value {
        EpicsValue::String(s) => s,
        _ => return Ok(value),
    };
    // Canonical DBF type per numeric/menu common field, chosen to match the
    // variant its `put_common_field` arm binds. The `DBF_MENU` fields take
    // `Enum`, C's `epicsEnum16` menu index.
    let dbf = match name {
        "SCAN" | "SSCN" | "PINI" => DbFieldType::Enum,
        "TSE" | "PHAS" | "PRIO" | "DISV" | "DISA" | "DISS" | "LCNT" | "UDFS" | "ACKT" | "ACKS"
        | "SEVR" | "STAT" | "NSEV" | "NSTA" => DbFieldType::Short,
        "DISP" | "UDF" => DbFieldType::Char,
        _ => return Ok(value),
    };
    let text = s.as_str_lossy();
    // A `DBF_MENU` common field resolves its label against THAT field's own
    // menu through the one converter every menu-field string put uses
    // (C `dbConvert.c::putStringMenu`: exact label, else an index below
    // `nChoice`, else `S_db_badChoice`) — the same rule the record-specific
    // menu fields follow in `coerce_write_value`. The failure PROPAGATES: the
    // field-blind `EpicsValue::parse` fallback below must never see a menu
    // field, or `caput REC.PRIO Bogus` lands as index 0 instead of failing.
    //
    // SCAN/SSCN/PINI are menu fields like any other and go through the same
    // converter. They used to each carry a hand-written `from_str` that drifted
    // from C: `ScanType::from_str` case-folded and invented `"0.5 second"`
    // aliases for menuScan's `".5 second"` (and mapped any out-of-range index
    // to Passive), `SimModeScan::from_str` took any u16, `PiniMode::from_str`
    // trimmed. C has ONE converter and it does none of that.
    if let Some(choices) = super::menu_choices::shared_menu_choices(name) {
        return super::menu_choices::resolve_menu_field_string_bounded(
            name, choices, dbf, &text, bound,
        );
    }
    // Numeric (non-menu) common field: C's `dbPut` runs the string through
    // `epicsParse*`, which tolerates whitespace around the digits.
    match EpicsValue::parse(dbf, text.trim()) {
        Ok(parsed) => Ok(parsed),
        Err(_) => Ok(value),
    }
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
    // Put-notify wait-set this record currently belongs to (C
    // `precord->ppn`). Set when the record joins an active put-notify
    // (originating put target, or a FLNK/OUT PP target via `dbNotifyAdd`);
    // taken + `leave`d when the record's processing completes. `None`
    // outside any put-notify. See [`NotifyWaitSet`].
    pub notify: Option<Arc<NotifyWaitSet>>,
    // Last posted values for subscribed fields (generic change detection)
    pub last_posted: HashMap<String, EpicsValue>,
    /// Set by `check_deadband_ext` for waveform/aai/aao when their
    /// content hash changed this cycle (C `monitor()` On Change mode,
    /// waveformRecord.c:310-319). The snapshot builders read it to post
    /// `HASH` with a literal `DBE_VALUE` event, independent of the VAL
    /// post mask. False for every record without the MPST/APST/HASH
    /// mechanism.
    pub(crate) array_hash_changed: bool,
    /// One-shot "skip the registered subroutine this cycle" signal for aSub
    /// `LFLG=READ`. The async processing path resolves the `SUBL` link before
    /// taking this lock; when the resolved name is bad (C `fetch_values` ->
    /// `S_db_BadSub`) or the link read failed, C `process` runs `do_sub` only
    /// on `!status`, so the subroutine is skipped. Set by the resolution
    /// apply, consumed (and cleared) by [`Self::run_registered_subroutine`];
    /// `false` for every record without a pending bad re-resolution.
    pub(crate) suppress_subroutine_run: bool,
    /// Generation counter for ReprocessAfter timer cancellation.
    /// Bumped each process cycle. Spawned timers check this to avoid
    /// stale re-processes from accumulated timers.
    pub reprocess_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Per-record info tags from `info("key", "value")` directives in
    /// the .db file (epics-base info(...) grammar). Consumers include
    /// asyn (`asyn:READBACK`), record-as-PV bridge tags
    /// (`Q:group`, `Q:form`), and IOC-specific extensions. Empty for
    /// records loaded without info(...) clauses.
    pub info: HashMap<String, String>,
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
    /// `display.description` from DESC), the new source field name MUST
    /// also be added to `is_metadata_field` so writes to it invalidate
    /// the cache. (The `Q:form` -> `display.form` mapping is exempt: it
    /// reads an immutable load-time info tag, not a runtime field.)
    pub(crate) metadata_cache: StdMutex<Option<MetadataSnapshot>>,
}

/// The cycle status [`RecordInstance::run_registered_subroutine`] reports when
/// `do_sub` was skipped — C's `fetch_values` failure / `S_db_BadSub` path, which
/// leaves `process`'s `status` non-zero (aSubRecord.c:216-224).
const SUBROUTINE_STATUS_SKIPPED: i64 = -1;
/// No subroutine is bound: C `do_sub` returns `S_db_BadSub` (aSubRecord.c:255).
const SUBROUTINE_STATUS_NO_SUB: i64 = -2;
/// The bound subroutine returned `Err` — no C counterpart (a C subroutine
/// returns a `long`), and a failed cycle either way.
const SUBROUTINE_STATUS_ERROR: i64 = -3;

/// C `monitor()`'s post of the deadband field, as assembled by the single owner
/// [`RecordInstance::deadband_post`].
pub(crate) struct DeadbandPost {
    /// C's `monitor_mask` for this cycle. Also the mask the
    /// [`Record::fields_posted_with_value_mask`] secondaries ride: C posts them
    /// from INSIDE the `if (monitor_mask)` guard, with the same mask.
    pub mask: EventMask,
    /// The deadband field's own post — `(field, value)`. `None` when no class
    /// fired (C's `if (monitor_mask)` skips the post) or the field does not
    /// resolve.
    pub field: Option<(String, EpicsValue)>,
}

impl RecordInstance {
    pub fn new(name: String, record: impl Record) -> Self {
        Self::new_boxed(name, Box::new(record))
    }

    pub fn new_boxed(name: String, record: Box<dyn Record>) -> Self {
        let rtype = record.record_type();
        let analog_alarm = match rtype {
            // C parity: every record type whose dbd carries
            // HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV gets an analog-alarm
            // config slot. Previously calc / calcout were missing —
            // their put_field for those fields silently no-op'd
            // because `self.common.analog_alarm` was None at the
            // mutation site. Confirmed via
            // calcRecord.dbd.pod:716-744 (HIHI..LLSV) and
            // calcoutRecord.dbd.pod:1103+ (same). `sub` carries the same
            // HIHI/HIGH/LOLO/LOW + HHSV/HSV/LSV/LLSV set
            // (subRecord.dbd.pod:569-642) and runs the analog `checkAlarms`.
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" | "calc" | "calcout"
            | "sub" => Some(AnalogAlarmConfig::default()),
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
            notify: None,
            last_posted: HashMap::new(),
            array_hash_changed: false,
            suppress_subroutine_run: false,
            reprocess_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            info: HashMap::new(),
            metadata_cache: StdMutex::new(None),
        }
    }

    /// Set a single `info("key", "value")` tag on this record. Last
    /// write wins. Used by the .db loader (`info(...)` directive) and
    /// `dbpf`-style tools.
    pub fn set_info(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.info.insert(key.into(), value.into());
    }

    /// Look up a single info tag. Returns `None` when the record has
    /// no tag with that key.
    pub fn get_info(&self, key: &str) -> Option<&str> {
        self.info.get(key).map(|s| s.as_str())
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

    /// Like [`notify_field_written`] but skips the invalidation when
    /// the put did not actually change the field's value. Mirrors
    /// epics-base `faac1df1` — `DBE_PROPERTY` events fire only on
    /// real changes, not on idempotent writes (the C path compares
    /// `paddr->pfield` against the converted payload before setting
    /// the `propertyUpdate` flag).
    ///
    /// `prev` is the value captured BEFORE the put. Callers that
    /// don't need the change-detection (e.g. internal writers that
    /// know the field is non-metadata) can keep using
    /// [`notify_field_written`].
    // must post EventMask::PROPERTY to all field subscribers when metadata changes
    pub fn notify_field_written_if_changed(&self, field: &str, prev: Option<&EpicsValue>) {
        let upper = field.to_ascii_uppercase();
        if !is_metadata_field(&upper) {
            return;
        }
        let now = self.record.get_field(&upper);
        if prev != now.as_ref() {
            self.invalidate_metadata_cache();
            // mirror C dbAccess.c:1396-1397 db_post_events(precord, NULL, DBE_PROPERTY).
            // Collect keys first to avoid a re-entrant immutable borrow on subscribers.
            let fields: Vec<String> = self.subscribers.keys().cloned().collect();
            for f in fields {
                self.notify_field_with_origin(&f, crate::server::recgbl::EventMask::PROPERTY, 0);
            }
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

    /// Resolve a field for EPICS `$` long-string (character-array) access.
    ///
    /// The `$` channel-name modifier (C `dbChannel.c:486-505`) re-views a
    /// field as a `DBR_CHAR` array: a `DBF_STRING` field becomes a char
    /// array of `field_size` elements, a link field a char array of
    /// `PVLINK_STRINGSZ`, and every other field type is rejected with
    /// `S_dbLib_fieldNotFound`. pvxs serves that char view as a
    /// `form = "String"` long-string `NTScalar` — it reads the `DBR_CHAR`
    /// bytes and NUL-terminates them back into a string
    /// (`ioc/iocsource.cpp:133-136`, `ioc/channel.cpp:62-74`).
    ///
    /// Both `DBF_STRING` fields and link fields resolve to an
    /// [`EpicsValue::String`] in this database (a link resolves to its
    /// textual form, see [`Self::get_common_field`]), so a field is
    /// `$`-eligible exactly when it resolves to a string value. Returns
    /// that string value for an eligible field, or `None` for a field the
    /// `$` modifier cannot view as a char array (the
    /// `S_dbLib_fieldNotFound` case) — the single owner of the
    /// dbChannel `$`-eligibility rule for the channel-resolution layer.
    pub fn resolve_string_view_field(&self, name: &str) -> Option<EpicsValue> {
        match self.resolve_field(name)? {
            v @ EpicsValue::String(_) => Some(v),
            _ => None,
        }
    }

    /// Choice table for a field served as `DBR_ENUM` from a `DBF_MENU`:
    /// the record's own record-specific menu
    /// ([`Record::menu_field_choices`](super::record_trait::Record::menu_field_choices)),
    /// else a shared menu keyed by field name
    /// ([`shared_menu_choices`](super::menu_choices::shared_menu_choices)).
    fn menu_choices_for(&self, field: &str) -> Option<&'static [&'static str]> {
        self.record
            .menu_field_choices(field)
            .or_else(|| super::menu_choices::shared_menu_choices(field))
    }

    /// Promote a `DBF_MENU` field's value to its `DBR_ENUM` client form: a
    /// menu index stored as a short becomes [`EpicsValue::Enum`], so the
    /// wire type a client sees is `DBR_ENUM` (CA) / `NTEnum` (PVA),
    /// matching C dbStaticLib serving `DBF_MENU` as `DBR_ENUM`. The
    /// menu index is held internally as `DbFieldType::Short`, so only that
    /// representation is promoted; a same-named field that is not a menu
    /// index here (e.g. `scalcout.OSV`, a string) is returned unchanged.
    /// Idempotent for a value already delivered as `Enum` (`.SCAN`/`SSCN`,
    /// the record-specific `SELM`).
    fn promote_menu_value(&self, field: &str, value: EpicsValue) -> EpicsValue {
        if self.menu_choices_for(field).is_some() {
            if let EpicsValue::Short(idx) = value {
                return EpicsValue::Enum(idx as u16);
            }
        }
        value
    }

    /// The client-facing value of `field`: the resolved value with a
    /// `DBF_MENU` field promoted to its `DBR_ENUM` form (see
    /// [`Self::promote_menu_value`]), so a wire type derived directly from
    /// the value matches the GET/MONITOR data. Used by the CA create-
    /// channel path, which reads the native type from the value rather
    /// than from [`Self::snapshot_for_field`].
    pub fn client_field_value(&self, field: &str) -> Option<EpicsValue> {
        let value = self.resolve_field(field)?;
        Some(self.promote_menu_value(field, value))
    }

    /// Attach the `DBF_MENU` → `DBR_ENUM` representation to a built
    /// snapshot: promote the value to [`EpicsValue::Enum`] and attach the
    /// menu's `menu()` choice labels so the CA/PVA enum encoders present
    /// them. The single owner of "menu field -> (enum value, choice
    /// table)" for both the GET ([`Self::snapshot_for_field`]) and MONITOR
    /// ([`Self::make_monitor_snapshot`]) snapshot builders, so the wire
    /// form is identical on every delivery path. A same-named non-menu
    /// field (whose value is not a menu index) keeps its plain value and
    /// gets no choice table.
    fn attach_menu_enum(&self, field: &str, snap: &mut super::super::snapshot::Snapshot) {
        let Some(choices) = self.menu_choices_for(field) else {
            return;
        };
        snap.value = self.promote_menu_value(field, snap.value.clone());
        if matches!(snap.value, EpicsValue::Enum(_)) {
            snap.enums = Some(super::super::snapshot::EnumInfo {
                strings: choices.iter().map(|s| PvString::from(*s)).collect(),
            });
        }
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
        // Default the served `timeStamp.userTag` to the record's `utag`,
        // mirroring pvxs `iocsource.cpp:245` (`auto utag = meta.utag;`).
        // The 64-bit `epicsUTag` narrows to the int32 NT wire field by
        // truncating to the low 32 bits — pvxs assigns the same uint64
        // straight into the `Int32` `timeStamp.userTag`. The `Q:time:tag`
        // nsec-LSB split below overrides this when configured, matching
        // pvxs `if(info.nsecMask) utag = meta.time.nsec & info.nsecMask;`
        // (:247).
        snap.user_tag = self.common.utag as i32;

        // Pull display/control/enums from the metadata cache (build on
        // first call, hit thereafter until invalidated by a metadata-class
        // field write).
        let meta = self.cached_metadata();
        snap.display = meta.display;
        snap.control = meta.control;
        snap.enums = meta.enums;

        // Per-field RSET metadata (C get_units/get_precision/
        // get_graphic_double/get_control_double/get_alarm_double key on
        // dbGetFieldIndex) patches the record-level cache for this field.
        self.apply_field_metadata_override(field, &mut snap);

        // DBF_MENU field (a shared menu such as `SCAN`/`OMSL`/`HHSV`/... or
        // a record-specific menu such as `sel.SELM`): carry the menu index
        // as DBR_ENUM and attach its `menu()` choice labels. See
        // `attach_menu_enum`. This overrides any record VAL enum table
        // copied from the metadata cache above, because a menu field
        // carries its own menu's choices, not the record's VAL state
        // strings.
        self.attach_menu_enum(field, &mut snap);

        // apply `info(Q:time:tag, "nsec:lsb:N")` — pvxs
        // `iocsource.cpp:239-248` publishes `nanoseconds & ~nsecMask` and
        // moves `nanoseconds & nsecMask` into `timeStamp.userTag`. The
        // split is applied to both `snap.timestamp` and `snap.user_tag` so
        // downstream encoders (NTScalar `timeStamp`, QSRV groups) all see
        // the same shape. A zero mask (tag absent or unparseable) is a
        // no-op inside the helper, exactly as pvxs's `if(info.nsecMask)`
        // gate is.
        crate::server::snapshot::apply_nsec_mask(&mut snap, self.qtime_nsec_mask());

        Some(snap)
    }

    /// Resolve `info(Q:time:tag)` to pvxs's `MappingInfo::nsecMask`.
    /// Returns 0 (the "no split" mask) when the tag is absent or does not
    /// parse — pvxs leaves `nsecMask` at its 0 initialiser in that case.
    ///
    /// pvxs `ioc/typeutils.cpp:79-88`:
    ///
    /// ```c
    /// if(auto val = ent.info("Q:time:tag")) {
    ///     epicsInt32 dig = 0;
    ///     if(strncmp(val, "nsec:lsb:", 9)==0 && !epicsParseInt32(&val[9], &dig, 10, nullptr)) {
    ///         nsecMask = (uint64_t(1u)<<dig)-1u;
    ///     }
    /// }
    /// ```
    ///
    /// The prefix test is a byte-exact `strncmp` — no case folding and no
    /// whitespace tolerance, so `NSEC:LSB:4` and `nsec: lsb: 4` do NOT
    /// match and leave the timestamp alone. There is no bounds clamp
    /// either: any `dig` `epicsParseInt32` accepts is shifted verbatim, so
    /// `nsec:lsb:31` yields the `0x7FFF_FFFF` mask pvxs actually serves.
    fn qtime_nsec_mask(&self) -> u64 {
        let Some(rest) = self
            .get_info("Q:time:tag")
            .and_then(|v| v.strip_prefix("nsec:lsb:"))
        else {
            return 0;
        };
        let Some(dig) = epics_parse_int32_base10(rest) else {
            return 0;
        };
        // C shifts `uint64_t(1u)` by an `epicsInt32`. A `dig` outside
        // `0..=63` is UB in C++; every ISA EPICS builds on (x86-64 `shlq`,
        // aarch64 `lsl`) takes the shift count modulo 64, which is what
        // `wrapping_shl` does — so `nsec:lsb:64` disables the split
        // (mask 0) and a negative `dig` shifts by `dig & 63`, the same
        // masks pvxs produces on those hosts.
        1u64.wrapping_shl(dig as u32) - 1
    }

    /// Populate DisplayInfo from record fields if applicable.
    /// Resolve the `Q:form` info-tag value to a `display.form` menu index.
    ///
    /// pvxs publishes the fixed seven-entry form menu
    /// (Default/String/Binary/Decimal/Hex/Exponential/Engineering) for every
    /// numeric value and, for the VAL field only, sets `display.form.index`
    /// to the slot whose name equals the field's `Q:form` info tag
    /// (`iocsource.cpp:42-62`, case-sensitive). Unset or unrecognised ->
    /// `None` (form stays 0 = Default), exactly as pvxs leaves the index
    /// untouched on no match.
    fn q_form_index(&self) -> Option<i16> {
        const FORM_NAMES: [&str; 7] = [
            "Default",
            "String",
            "Binary",
            "Decimal",
            "Hex",
            "Exponential",
            "Engineering",
        ];
        let tag = self.info.get("Q:form")?;
        FORM_NAMES
            .iter()
            .position(|name| name == tag)
            .map(|i| i as i16)
    }

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
            "longin" | "longout" | "int64in" | "int64out" => {
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
                // longin/longout severity-gate (get_alarm_double);
                // int64in/int64out send the limits verbatim (C is
                // unconditional for those two record types only).
                let (hihi, high, low, lolo) = match rtype {
                    "int64in" | "int64out" => self.alarm_limits_unchecked(),
                    _ => self.alarm_limits(),
                };
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
            // waveform/aai/aao — HOPR/LOPR/PREC/EGU for VAL display limits.
            // (waveformRecord.c:251-252,239; aaiRecord.c:280-281,268; aaoRecord.c:283-284)
            "waveform" | "aai" | "aao" => {
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
                snap.display = Some(super::super::snapshot::DisplayInfo {
                    units: egu,
                    precision: prec,
                    upper_disp_limit: hopr,
                    lower_disp_limit: lopr,
                    upper_alarm_limit: 0.0,
                    upper_warning_limit: 0.0,
                    lower_warning_limit: 0.0,
                    lower_alarm_limit: 0.0,
                    ..Default::default()
                });
            }
            // compress — HOPR/LOPR/PREC/EGU for VAL display limits.
            // (compressRecord.c:478-479,464,455)
            "compress" => {
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
                snap.display = Some(super::super::snapshot::DisplayInfo {
                    units: egu,
                    precision: prec,
                    upper_disp_limit: hopr,
                    lower_disp_limit: lopr,
                    upper_alarm_limit: 0.0,
                    upper_warning_limit: 0.0,
                    lower_warning_limit: 0.0,
                    lower_alarm_limit: 0.0,
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
        // Apply the `Q:form` display-format hint. The match above builds
        // `snap.display` only for numeric record types — the same set for
        // which pvxs emits `display.form.choices` — so a present `Q:form`
        // tag maps to `display.form.index` exactly when pvxs applies it
        // (`iocsource.cpp:42-62`, VAL-only; the per-record DisplayInfo here
        // *is* the VAL field's metadata).
        if let Some(display) = snap.display.as_mut() {
            if let Some(form) = self.q_form_index() {
                display.form = form;
            }
        }
    }

    /// Populate ControlInfo from record fields if applicable.
    fn populate_control_info(&self, snap: &mut super::super::snapshot::Snapshot) {
        let rtype = self.record.record_type();
        match rtype {
            // ao unconditionally uses DRVH/DRVL (aoRecord.c:356-357).
            "ao" => {
                let upper = self
                    .record
                    .get_field("DRVH")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let lower = self
                    .record
                    .get_field("DRVL")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                snap.control = Some(super::super::snapshot::ControlInfo {
                    upper_ctrl_limit: upper,
                    lower_ctrl_limit: lower,
                });
            }
            // longout/int64out use DRVH/DRVL only when drvh > drvl, else HOPR/LOPR
            // (longoutRecord.c:282-287, int64outRecord.c:265-270).
            "longout" | "int64out" => {
                let drvh = self
                    .record
                    .get_field("DRVH")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let drvl = self
                    .record
                    .get_field("DRVL")
                    .and_then(|v| v.to_f64())
                    .unwrap_or(0.0);
                let (upper, lower) = if drvh > drvl {
                    (drvh, drvl)
                } else {
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
                    (hopr, lopr)
                };
                snap.control = Some(super::super::snapshot::ControlInfo {
                    upper_ctrl_limit: upper,
                    lower_ctrl_limit: lower,
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
            // int64in uses HOPR/LOPR as control limits (int64inRecord.c:226-227)
            "ai" | "int64in" | "longin" | "calc" | "calcout" => {
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
            // Array records map their VAL control limits to HOPR/LOPR, exactly
            // like the display limits above (waveformRecord.c get_control_double
            // VAL case; aaiRecord.c:293-303; aaoRecord.c; compressRecord.c:487-501).
            // Without this arm an array DBR_CTRL collapses the control range to
            // 0/0 while the scalar records expose it.
            "waveform" | "aai" | "aao" | "compress" => {
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
            // bi/bo/busy — C trims no_str to 1 when ZNAM set and ONAM empty (boRecord.c:342-352).
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
                let no_str_1 = !znam.is_empty() && onam.is_empty();
                let mut strings = vec![znam, onam];
                if no_str_1 {
                    strings.truncate(1);
                }
                snap.enums = Some(super::super::snapshot::EnumInfo { strings });
            }
            // mbbi/mbbo — C uses highwater mark: last non-empty index + 1 (mbbiRecord.c:262-269).
            "mbbi" | "mbbo" => {
                let state_fields = [
                    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST", "EIST", "NIST",
                    "TEST", "ELST", "TVST", "TTST", "FTST", "FFST",
                ];
                let mut strings: Vec<PvString> = state_fields
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
                let no_str = strings
                    .iter()
                    .rposition(|s| !s.is_empty())
                    .map(|i| i + 1)
                    .unwrap_or(0);
                strings.truncate(no_str);
                snap.enums = Some(super::super::snapshot::EnumInfo { strings });
            }
            _ => {}
        }
    }

    /// Extract analog alarm limits from CommonFields.
    // DBR_GR_*/DBR_CTRL_* alarm limits MUST be severity-gated — C
    // get_alarm_double returns `prec->hhsv ? prec->hihi : epicsNAN`
    // (aiRecord.c:295-298 and ao/longin/longout/calc/calcout). int64in/
    // int64out are the sole exception (unconditional, int64inRecord.c:239-243)
    // and use `alarm_limits_unchecked()`. NaN encodes byte-exact for every
    // DBR variant: f64/f32 keep NaN, integer casts make `NaN as iN == 0`,
    // matching dbAccess.c:300-326 (`finite(ald)?cast:0`).
    fn alarm_limits(&self) -> (f64, f64, f64, f64) {
        match self.common.analog_alarm {
            // Each limit is reported only when its severity is enabled,
            // exactly as C `get_alarm_double` (`x ? limit : epicsNAN`).
            Some(ref aa) => (
                gated(aa.hhsv, aa.hihi),
                gated(aa.hsv, aa.high),
                gated(aa.lsv, aa.low),
                gated(aa.llsv, aa.lolo),
            ),
            // No analog-alarm config ⇒ all severities are NO_ALARM in C,
            // so every limit is NaN (not 0).
            None => (f64::NAN, f64::NAN, f64::NAN, f64::NAN),
        }
    }

    // int64in/int64out are the one analog family whose C `get_alarm_double`
    // is UNCONDITIONAL (int64inRecord.c:239-243, int64outRecord.c:283-287):
    // the limits are sent verbatim regardless of HHSV/HSV/LSV/LLSV. Keep a
    // separate accessor so the gated `alarm_limits()` cannot leak into this
    // path.
    fn alarm_limits_unchecked(&self) -> (f64, f64, f64, f64) {
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
            // epics-base PR #568 / #566 — alarm message string.
            "AMSG" => Some(EpicsValue::String(self.common.amsg.clone().into())),
            "NAMSG" => Some(EpicsValue::String(self.common.namsg.clone().into())),
            "ACKS" => Some(EpicsValue::Short(self.common.acks as i16)),
            // `ACKT` and `PINI` are `DBF_MENU` (`menuYesNo` /`menuPini`,
            // `dbCommon.dbd.pod:335,169`), not `DBF_UCHAR`: they carry a menu
            // index, which `promote_menu_value` lifts to `DBR_ENUM` with the
            // menu's choice strings. Storing them as `Short` is what makes
            // them eligible for that promotion — see `promote_menu_value`.
            "ACKT" => Some(EpicsValue::Short(if self.common.ackt { 1 } else { 0 })),
            "UDF" => Some(EpicsValue::Char(if self.common.udf { 1 } else { 0 })),
            "UDFS" => Some(EpicsValue::Short(self.common.udfs as i16)),
            "SCAN" => Some(EpicsValue::Enum(self.common.scan as u16)),
            "SSCN" => Some(EpicsValue::Enum(self.common.sscn.to_u16())),
            "PINI" => Some(EpicsValue::Short(self.common.pini.to_u16() as i16)),
            "TPRO" => Some(EpicsValue::Char(if self.common.tpro { 1 } else { 0 })),
            "BKPT" => Some(EpicsValue::Char(self.common.bkpt)),
            "FLNK" => Some(EpicsValue::String(self.common.flnk.clone().into())),
            "INP" => Some(EpicsValue::String(self.common.inp.clone().into())),
            "OUT" => Some(EpicsValue::String(self.common.out.clone().into())),
            "DTYP" => Some(EpicsValue::String(self.common.dtyp.clone().into())),
            "TSE" => Some(EpicsValue::Short(self.common.tse)),
            "TSEL" => Some(EpicsValue::String(self.common.tsel.clone().into())),
            // C `UTAG` is DBF_UINT64 — exposed natively as the unsigned
            // 64-bit value variant so values above i64::MAX round-trip.
            "UTAG" => Some(EpicsValue::UInt64(self.common.utag)),
            "ASG" => Some(EpicsValue::String(self.common.asg.clone().into())),
            "ASL" => Some(EpicsValue::Char(self.common.asl)),
            "DESC" => Some(EpicsValue::String(self.common.desc.clone())),
            "PHAS" => Some(EpicsValue::Short(self.common.phas)),
            "EVNT" => Some(EpicsValue::String(self.common.evnt.clone().into())),
            "PRIO" => Some(EpicsValue::Short(self.common.prio)),
            "DISV" => Some(EpicsValue::Short(self.common.disv)),
            "DISA" => Some(EpicsValue::Short(self.common.disa)),
            "SDIS" => Some(EpicsValue::String(self.common.sdis.clone().into())),
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
            // swait OUTN is aliased to common.out
            "OUTN" => {
                if self.record.record_type() == "swait" {
                    Some(EpicsValue::String(self.common.out.clone().into()))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Set a common field value from a runtime `dbPut` (CA/PVA/`dbpf`/link).
    /// Returns what scan index changes are needed.
    ///
    /// A `DBF_MENU` common field's string is converted by C's runtime
    /// converter, `dbConvert.c::putStringMenu` — see [`MenuBound::DbPut`].
    pub fn put_common_field(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> CaResult<CommonFieldPutResult> {
        self.put_common_field_bounded(name, value, MenuBound::DbPut)
    }

    /// Set a common field value from the `.db` loader, which in C is a
    /// different converter with a different out-of-menu bound
    /// (`dbStaticRun.c::dbPutStringNum`; see [`MenuBound::DbLoad`]). It is what
    /// lets `field(SSCN,"65535")` — the menuScan "use SCAN" sentinel, out of
    /// the menu's 0-9 range — load, while `caput REC.SSCN 65535` is refused at
    /// runtime exactly as C refuses it.
    pub fn put_common_field_db_load(
        &mut self,
        name: &str,
        value: EpicsValue,
    ) -> CaResult<CommonFieldPutResult> {
        self.put_common_field_bounded(name, value, MenuBound::DbLoad)
    }

    fn put_common_field_bounded(
        &mut self,
        name: &str,
        value: EpicsValue,
        bound: MenuBound,
    ) -> CaResult<CommonFieldPutResult> {
        let name = name.to_ascii_uppercase();
        self.record.validate_put(&name, &value)?;
        self.record.special(&name, false)?;
        // The db loader hands every common field to this path as a raw
        // `EpicsValue::String` (no per-field `FieldDesc` to parse against).
        // Coerce it to the field's canonical numeric/menu type up front so the
        // typed arms below apply a `field(PHAS, "1")` / `field(PRIO, "HIGH")`
        // directive instead of silently dropping it at IOC load. String-typed
        // and already-typed values pass through unchanged.
        let value = coerce_common_field_string(&name, value, bound)?;
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
            "AMSG" => {
                if let EpicsValue::String(s) = value {
                    self.common.amsg = s.as_str_lossy().into_owned();
                }
            }
            "NAMSG" => {
                if let EpicsValue::String(s) = value {
                    self.common.namsg = s.as_str_lossy().into_owned();
                }
            }
            "ACKS" => {
                if let EpicsValue::Short(v) = value {
                    let sev = AlarmSeverity::from_u16(v as u16);
                    // C `dbAccess.c:1309` putAcks:
                    //   `if (*psev >= precord->acks) precord->acks = 0;`
                    // The written severity is compared against the
                    // STORED unacknowledged severity `acks` — NOT the
                    // current `sevr`. An operator acknowledging an
                    // alarm at the severity that was latched into ACKS
                    // must clear it even after `sevr` has since
                    // dropped; comparing against `sevr` instead would
                    // leave a stale unacknowledged alarm stuck.
                    if sev >= self.common.acks {
                        self.common.acks = AlarmSeverity::NoAlarm;
                    }
                }
            }
            "ACKT" => {
                let new_ackt = match value {
                    EpicsValue::Char(v) => v != 0,
                    EpicsValue::Short(v) => v != 0,
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.ackt = new_ackt;
                // C `dbAccess.c:1294-1297` putAckt: when ACKT is set
                // false (transient acknowledgement disabled) and the
                // stored unacknowledged severity is higher than the
                // current `sevr`, lower `acks` down to `sevr` — a
                // transient alarm that has already cleared should not
                // keep a sticky higher-severity ACKS once transient
                // acknowledgement is turned off.
                if !new_ackt && self.common.acks > self.common.sevr {
                    self.common.acks = self.common.sevr;
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
            // The `String` form never reaches these three menu arms:
            // `coerce_common_field_string` has already run it through the one
            // menu converter, which either produced an `Enum` index or failed
            // the put with `S_db_badChoice`.
            "SCAN" => {
                let old_scan = self.common.scan;
                let new_scan = match &value {
                    EpicsValue::Short(v) => ScanType::from_u16(*v as u16),
                    EpicsValue::Enum(v) => ScanType::from_u16(*v),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.scan = new_scan;
                if old_scan != new_scan {
                    let phas = self.common.phas;
                    // C `dbPutField` on SCAN runs `scanDelete` then `scanAdd`
                    // (dbScan.c:236-248), which call the record's device support
                    // `get_ioint_info(1)` / `get_ioint_info(0)`. Only a change of
                    // I/O Intr *membership* reaches those; a Passive→"1 second"
                    // move calls neither.
                    let was_io_intr = old_scan == ScanType::IoIntr;
                    let is_io_intr = new_scan == ScanType::IoIntr;
                    if was_io_intr != is_io_intr {
                        self.record.set_io_intr_scan(is_io_intr);
                    }
                    self.record.on_put(&name);
                    self.record.special(&name, true)?;
                    return Ok(CommonFieldPutResult::ScanChanged {
                        old_scan,
                        new_scan,
                        phas,
                    });
                }
            }
            "SSCN" => {
                let new_sscn = match &value {
                    EpicsValue::Short(v) => SimModeScan::from_u16(*v as u16),
                    EpicsValue::Enum(v) => SimModeScan::from_u16(*v),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.sscn = new_sscn;
            }
            // `PINI` is `menu(menuPini)` — the six choices NO/YES/RUN/RUNNING/
            // PAUSE/PAUSED (`menuPini.dbd.pod:59-65`). Resolved exactly like
            // `SCAN`: a menu label or a bare index, never a truthiness test.
            // The pre-fix `bool` arm collapsed `RUN` (index 2) to `false`, so
            // `caput REC.PINI RUN` *disabled* PINI instead of selecting the
            // iocRun pass.
            "PINI" => {
                self.common.pini = match &value {
                    EpicsValue::Short(v) => PiniMode::from_u16(*v as u16),
                    EpicsValue::Char(v) => PiniMode::from_u16(*v as u16),
                    EpicsValue::Enum(v) => PiniMode::from_u16(*v),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
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
                    self.common.flnk = s.as_str_lossy().into_owned();
                    self.parsed_flnk = parse_forward_link_v2(&self.common.flnk);
                }
            }
            "INP" => {
                if let EpicsValue::String(s) = value {
                    self.common.inp = s.as_str_lossy().into_owned();
                    self.parsed_inp = parse_link_v2(&self.common.inp);
                }
            }
            "OUT" => {
                if let EpicsValue::String(s) = value {
                    let s = s.as_str_lossy();
                    // C `dbParseLink` (dbStaticLib.c:2382-2386) discards a
                    // CP/CPP modifier on a DBF_OUTLINK and warns once, naming
                    // the holder record, its field and the target. The discard
                    // itself is owned by `parse_output_link_v2` below; only the
                    // diagnostic lives here, where the record name exists and
                    // the link text is being (re)loaded rather than re-parsed
                    // per process cycle.
                    if out_link_discards_cp(&s) {
                        tracing::warn!(
                            target: "epics_base_rs::record",
                            record = %self.name,
                            field = "OUT",
                            link = %s,
                            "Discarding CP/CPP modifier in CA output link"
                        );
                    }
                    self.common.out = s.into_owned();
                    // C `dbDbPutValue` (dbDbLink.c:386-389): an OUT
                    // link processes its target only on an explicit
                    // ` PP` token (or a `.PROC` destination). A bare
                    // OUT link is NPP — `parse_output_link_v2`
                    // downgrades the modifier-less `ProcessPassive`
                    // default that `parse_link_v2` would otherwise
                    // apply.
                    self.parsed_out = parse_output_link_v2(&self.common.out);
                    // C `longoutRecord.c::special` (PR #6c573b4 part 2)
                    // and similar OOCH-style hooks need `after=true`
                    // to fire after the link has actually moved. The
                    // earlier `validate_put` + `special(name, false)`
                    // pair only covered the before-side.
                    self.record.special(&name, true)?;
                }
            }
            "DTYP" => {
                if let EpicsValue::String(s) = value {
                    self.common.dtyp = s.as_str_lossy().into_owned();
                }
            }
            "TSE" => {
                if let EpicsValue::Short(v) = value {
                    self.common.tse = v;
                }
            }
            "TSEL" => {
                if let EpicsValue::String(s) = value {
                    self.common.tsel = s.as_str_lossy().into_owned();
                    self.parsed_tsel = parse_link_v2(&self.common.tsel);
                }
            }
            "UTAG" => {
                // C UTAG is DBF_UINT64 — accept any integer-shaped value and
                // store the unsigned 64-bit tag. The db loader feeds every
                // common field as EpicsValue::String, so parse field(UTAG, "N")
                // rather than dropping it silently at IOC load; a CA write to
                // this u64 field crosses as DBR_DOUBLE (CA has no uint64 wire
                // type), so accept Double too.
                match value {
                    EpicsValue::UInt64(v) => self.common.utag = v,
                    EpicsValue::Int64(v) => self.common.utag = v as u64,
                    EpicsValue::Long(v) => self.common.utag = v as u64,
                    EpicsValue::Short(v) => self.common.utag = v as u64,
                    EpicsValue::Enum(v) => self.common.utag = v as u64,
                    EpicsValue::Char(v) => self.common.utag = v as u64,
                    EpicsValue::Double(v) => self.common.utag = v as u64,
                    EpicsValue::String(s) => {
                        if let Ok(EpicsValue::UInt64(v)) =
                            EpicsValue::parse(DbFieldType::UInt64, s.as_str_lossy().trim())
                        {
                            self.common.utag = v;
                        }
                    }
                    _ => {}
                }
            }
            "ASG" => {
                if let EpicsValue::String(s) = value {
                    self.common.asg = s.as_str_lossy().into_owned();
                }
            }
            "ASL" => {
                // C dbCommon.ASL is `epicsUInt32` in the .dbd but
                // only ever 0 or 1; accept Char / Short / Long for
                // the common put paths and clamp to {0, 1}.
                // db_loader feeds every common field as
                // `EpicsValue::String`; also accept that so a
                // `.db` `field(ASL, "1")` directive isn't silently
                // ignored at IOC load.
                let n: i64 = match value {
                    EpicsValue::Char(v) => v as i64,
                    EpicsValue::Short(v) => v as i64,
                    EpicsValue::Long(v) => v as i64,
                    EpicsValue::Int64(v) => v,
                    EpicsValue::String(s) => s.as_str_lossy().trim().parse().unwrap_or(0),
                    _ => return Ok(CommonFieldPutResult::NoChange),
                };
                self.common.asl = if n != 0 { 1 } else { 0 };
            }
            "DESC" => {
                if let EpicsValue::String(s) = value {
                    // DBF_STRING data field — store the bytes verbatim so a
                    // non-UTF-8 DESC round-trips unchanged.
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
                        self.record.special(&name, true)?;
                        return Ok(CommonFieldPutResult::PhasChanged {
                            scan,
                            old_phas,
                            new_phas: v,
                        });
                    }
                }
            }
            "EVNT" => {
                // C `EVNT` is DBF_STRING (event name). Accept a
                // string directly; accept a numeric value too for
                // backward compatibility (numeric events / a calc
                // record driving EVNT) by formatting it as a string.
                match value {
                    EpicsValue::String(s) => self.common.evnt = s.as_str_lossy().into_owned(),
                    EpicsValue::Short(v) => self.common.evnt = v.to_string(),
                    EpicsValue::Long(v) => self.common.evnt = v.to_string(),
                    EpicsValue::Enum(v) => self.common.evnt = v.to_string(),
                    EpicsValue::Double(v) => {
                        // Match C `eventNameToHandle`: a double with
                        // an integer part is treated as that integer.
                        self.common.evnt = (v as i64).to_string();
                    }
                    _ => {}
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
                    self.common.sdis = s.as_str_lossy().into_owned();
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
            // Analog alarm fields — accept Double, Long, or String (DB-load path sends String)
            "HIHI" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    if let Some(v) = value.to_f64().or_else(|| {
                        if let EpicsValue::String(s) = &value {
                            s.as_str_lossy().parse::<f64>().ok()
                        } else {
                            None
                        }
                    }) {
                        a.hihi = v;
                    }
                }
            }
            "HIGH" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    if let Some(v) = value.to_f64().or_else(|| {
                        if let EpicsValue::String(s) = &value {
                            s.as_str_lossy().parse::<f64>().ok()
                        } else {
                            None
                        }
                    }) {
                        a.high = v;
                    }
                }
            }
            "LOW" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    if let Some(v) = value.to_f64().or_else(|| {
                        if let EpicsValue::String(s) = &value {
                            s.as_str_lossy().parse::<f64>().ok()
                        } else {
                            None
                        }
                    }) {
                        a.low = v;
                    }
                }
            }
            "LOLO" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    if let Some(v) = value.to_f64().or_else(|| {
                        if let EpicsValue::String(s) = &value {
                            s.as_str_lossy().parse::<f64>().ok()
                        } else {
                            None
                        }
                    }) {
                        a.lolo = v;
                    }
                }
            }
            "HHSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.hhsv = parse_alarm_severity(&value);
                }
            }
            "HSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.hsv = parse_alarm_severity(&value);
                }
            }
            "LSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.lsv = parse_alarm_severity(&value);
                }
            }
            "LLSV" => {
                if let Some(a) = &mut self.common.analog_alarm {
                    a.llsv = parse_alarm_severity(&value);
                }
            }
            // swait-specific: OUTN is the output link name for swait records.
            // Mirrors to common.out so the processing framework dispatches it.
            "OUTN" => {
                if self.record.record_type() != "swait" {
                    // No OUTN field on any other record type — the same
                    // `S_dbLib_fieldNotFound` the catch-all below reports.
                    return Err(self.unknown_field_error(name));
                }
                if let EpicsValue::String(s) = value {
                    self.common.out = s.as_str_lossy().into_owned();
                    // Bare OUT link is NPP — see the "OUT" arm.
                    self.parsed_out = parse_output_link_v2(&self.common.out);
                }
            }
            // C `dbNameToAddr` (dbAccess.c:660-676) resolves the field part
            // with `dbFindFieldPart`, then falls back to `dbGetAttributePart`.
            // A name that is neither a record field, nor a dbCommon field, nor
            // an attribute resolves to nothing (`S_dbLib_fieldNotFound`), so
            // `dbPutField` is never reached and the caller reports the error —
            // `dbpf` prints "PV '%s' not found" and returns -1 (dbTest.c:787-795).
            // Returning success here made a put to a misspelled field a silent
            // no-op.
            _ => return Err(self.unknown_field_error(name)),
        }
        self.record.on_put(&name);
        // C `dbPut` (dbAccess.c:1399-1405) returns the after-put
        // `dbPutSpecial(paddr, 1)` status to the caller — the stored value
        // stays, but the monitor post and the process are skipped and the
        // client sees the failure. Never drop it.
        self.record.special(&name, true)?;
        Ok(CommonFieldPutResult::NoChange)
    }

    /// The error C reports for a write to a field name that
    /// [`Self::put_common_field`] does not own.
    ///
    /// Two C outcomes, split by whether the name resolves at all:
    ///
    /// - A record *attribute* (`NAME`, `RTYP`) resolves — `dbGetAttributePart`
    ///   succeeds — but the write is refused: `NAME` is `special(SPC_NOMOD)`
    ///   (dbCommon.dbd:13-17) so `dbPutSpecial` pass 0 returns `S_db_noMod`
    ///   (dbAccess.c:123-124), and an attribute address carries
    ///   `special == SPC_ATTRIBUTE`, which `dbPutField` rejects with the same
    ///   `S_db_noMod` (dbAccess.c:1252-1253).
    /// - Anything else does not resolve: `S_dbLib_fieldNotFound`.
    fn unknown_field_error(&self, name: String) -> CaError {
        if self.get_virtual_field(&name).is_some() {
            CaError::ReadOnlyField(name)
        } else {
            CaError::FieldNotFound(name)
        }
    }

    /// Get virtual fields (NAME, RTYP).
    pub fn get_virtual_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "NAME" => Some(EpicsValue::String(self.name.clone().into())),
            "RTYP" => Some(EpicsValue::String(
                self.record.record_type().to_string().into(),
            )),
            _ => None,
        }
    }

    /// Evaluate alarms based on record type and current value.
    /// Uses rec_gbl_set_sevr to accumulate into nsta/nsev.
    ///
    /// CALC_ALARM is NOT raised here. C raises it inside the record's own
    /// `process()` (`calcRecord.c:121-123`, `calcoutRecord.c:238-241`,
    /// `sCalcoutRecord.c:357-363`, `aCalcoutRecord.c:304-305`,
    /// `swaitRecord.c:409-410`), and in the port [`Record::check_alarms`] — which
    /// runs immediately before this — is that owner. It used to be raised here
    /// instead, keyed on a hardcoded `rtype` list plus a `CALC_ALARM` pseudo-field
    /// no DBD declares; swait is what that construction cost: it carried the flag
    /// but was not on the list, so a failed `calcPerform` alarmed nowhere.
    pub fn evaluate_alarms(&mut self) {
        use crate::server::recgbl;

        // Check UDF first
        recgbl::rec_gbl_check_udf(&mut self.common);

        let rtype = self.record.record_type();
        match rtype {
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" | "calc" | "calcout"
            | "sub" => {
                if let Some(ref alarm_cfg) = self.common.analog_alarm.clone() {
                    let val = match self.record.val() {
                        Some(EpicsValue::Double(v)) => v,
                        Some(EpicsValue::Long(v)) => v as f64,
                        Some(EpicsValue::Int64(v)) => v as f64,
                        _ => return,
                    };
                    self.evaluate_analog_alarm(val, alarm_cfg);
                }
            }
            // bi / bo / busy / mbbi / mbbo STATE+COS (and mbbo SOFT)
            // alarm evaluation now lives in each record's
            // `Record::check_alarms` hook (C `checkAlarms`). Keeping an
            // arm here would double-raise.
            _ => {} // no-op for other types
        }
    }

    fn evaluate_analog_alarm(&mut self, val: f64, cfg: &AnalogAlarmConfig) {
        use crate::server::recgbl::{self, alarm_status};

        // C `checkAlarms` returns immediately on a UDF cycle: it raises
        // `UDF_ALARM`/`UDFS` (already done by `rec_gbl_check_udf` in
        // `evaluate_alarms`), zeroes `AFVL` on the AFTC-capable records, and
        // returns BEFORE the range check — so `LALM` is left untouched and
        // `AFVL` is not filtered this cycle. The identical guard appears in
        // every record that shares this arm (`aiRecord.c:319-323`,
        // `aoRecord.c:383-386`, `longinRecord.c:274-278`,
        // `longoutRecord.c:317-320`, `int64inRecord.c:267-271`,
        // `int64outRecord.c:298-301`, `calcRecord.c:300-304`,
        // `calcoutRecord.c:563-566`). AFTC-capable records (ai/longin/
        // int64in/calc) carry `AFVL` and zero it (`prec->afvl = 0`); the
        // out records (ao/longout/int64out/calcout) have no `AFVL` and just
        // return. Running the range check here would drift `LALM` to `val`
        // (NaN on an undefined cycle) and filter `AFVL` — both observable.
        if self.common.udf {
            if matches!(
                self.record.record_type(),
                "calc" | "ai" | "longin" | "int64in"
            ) && self.record.get_field("AFVL").and_then(|v| v.to_f64()) != Some(0.0)
            {
                let _ = self.record.put_field("AFVL", EpicsValue::Double(0.0));
            }
            return;
        }

        let hyst = self.common.hyst;
        let lalm = self
            .record
            .get_field("LALM")
            .and_then(|v| v.to_f64())
            .unwrap_or(val);

        // C-style per-level hysteresis: alarm fires if val passes the level,
        // OR if we were already at that alarm level (lalm == alev) and val
        // hasn't retreated past the hysteresis margin.
        //
        // `alarm_range` is the C-style integer level: 1=Lolo, 2=Low,
        // 3=Normal, 4=High, 5=Hihi. Required for the calc-record AFTC
        // filter (`calcRecord.c::checkAlarms:339-381`) which filters
        // on the range level (not on severity) and re-maps back.
        let (mut new_sevr, mut new_stat, mut alev, mut alarm_range) = if cfg.hhsv
            != AlarmSeverity::NoAlarm
            && (val >= cfg.hihi || (lalm == cfg.hihi && val >= cfg.hihi - hyst))
        {
            (cfg.hhsv, alarm_status::HIHI_ALARM, cfg.hihi, 5u16)
        } else if cfg.llsv != AlarmSeverity::NoAlarm
            && (val <= cfg.lolo || (lalm == cfg.lolo && val <= cfg.lolo + hyst))
        {
            (cfg.llsv, alarm_status::LOLO_ALARM, cfg.lolo, 1u16)
        } else if cfg.hsv != AlarmSeverity::NoAlarm
            && (val >= cfg.high || (lalm == cfg.high && val >= cfg.high - hyst))
        {
            (cfg.hsv, alarm_status::HIGH_ALARM, cfg.high, 4u16)
        } else if cfg.lsv != AlarmSeverity::NoAlarm
            && (val <= cfg.low || (lalm == cfg.low && val <= cfg.low + hyst))
        {
            (cfg.lsv, alarm_status::LOW_ALARM, cfg.low, 2u16)
        } else {
            (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, val, 3u16)
        };

        // C parity: the alarm-range AFTC low-pass filter
        // (`{ai,longin,int64in,calc}Record.c::checkAlarms`) smooths the
        // integer `alarmRange` and re-maps. Only records that carry the
        // AFTC/AFVL fields run it — `ao`/`longout`/`int64out`/`calcout`
        // have no AFTC field (confirmed via the respective `.dbd.pod`),
        // so they are excluded.
        let aftc_capable = matches!(
            self.record.record_type(),
            "calc" | "ai" | "longin" | "int64in"
        );
        if aftc_capable {
            let aftc = self
                .record
                .get_field("AFTC")
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0);
            let afvl = self
                .record
                .get_field("AFVL")
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0);
            if aftc > 0.0 {
                let now = crate::runtime::general_time::get_current();
                let (filtered_range, new_afvl) = crate::server::records::alarm_filter::aftc_filter(
                    alarm_range,
                    aftc,
                    afvl,
                    self.common.time,
                    now,
                );
                let _ = self.record.put_field("AFVL", EpicsValue::Double(new_afvl));
                if filtered_range != alarm_range {
                    // Re-map filtered range back to (sevr, stat, alev).
                    let (mapped_sevr, mapped_stat, mapped_alev) = match filtered_range {
                        5 => (cfg.hhsv, alarm_status::HIHI_ALARM, cfg.hihi),
                        4 => (cfg.hsv, alarm_status::HIGH_ALARM, cfg.high),
                        2 => (cfg.lsv, alarm_status::LOW_ALARM, cfg.low),
                        1 => (cfg.llsv, alarm_status::LOLO_ALARM, cfg.lolo),
                        _ => (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, val),
                    };
                    new_sevr = mapped_sevr;
                    new_stat = mapped_stat;
                    alev = mapped_alev;
                    alarm_range = filtered_range;
                }
            } else {
                // aftc <= 0 disables the filter. C `checkAlarms`
                // (e.g. aiRecord.c:356,402) initialises the local
                // `afvl = 0` and unconditionally stores `prec->afvl =
                // afvl` at the end, so a disabled filter drives AFVL to
                // 0. Mirror that here so a stale accumulator from a prior
                // `aftc > 0` run cannot mis-seed the filter if AFTC is
                // re-enabled later.
                if afvl != 0.0 {
                    let _ = self.record.put_field("AFVL", EpicsValue::Double(0.0));
                }
            }
        }
        let _ = alarm_range; // suppress unused-var on non-calc paths

        if new_sevr != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(&mut self.common, new_stat, new_sevr);
            // C sets LALM to the alarm threshold level, not the current value
            let _ = self.record.put_field("LALM", EpicsValue::Double(alev));
        } else {
            // No alarm condition: reset LALM to current value (like C)
            let _ = self.record.put_field("LALM", EpicsValue::Double(val));
        }
    }

    /// Invoke the registered subroutine (`sub`/`aSub` `SNAM`) if one is
    /// bound, before the record's `process()` body runs.
    ///
    /// C `subRecord.c::do_sub` / `aSubRecord.c::do_sub` call the named
    /// subroutine on EVERY `process()`. The function registry lives on the
    /// framework (`RecordInstance::subroutine`), not on the record, so the
    /// record's own `process()` is a no-op for these two types and the
    /// framework must drive the call. This is the SINGLE owner of that call
    /// for every dispatch path: the main engine
    /// (`process_record_with_links_inner`, the SCAN / event / CA-put-to-PP /
    /// FLNK path) and the by-name `process_local` (`db.process_record`,
    /// QSRV group / foreign-call path) both route through here, so a
    /// `sub`/`aSub` runs identically regardless of how it is processed.
    /// Previously only `process_local` invoked the subroutine, so on the
    /// main engine path `VAL`/`VALA..VALU`/`OUTA..OUTU` never updated.
    /// The cycle's status is delivered to the record on EVERY exit path — see
    /// [`Record::set_subroutine_status`], which aSub's OUT-link gate reads. The
    /// delivery is factored out of the body below so a future early return
    /// cannot skip it: the body returns the status, this wrapper publishes it.
    pub(crate) fn run_registered_subroutine(&mut self) -> CaResult<()> {
        let outcome = self.run_subroutine_body();
        // A subroutine that errored out has no C counterpart (a C subroutine
        // returns a `long`); it is a failed cycle, so it takes the non-zero
        // arm — no outputs.
        let status = *outcome.as_ref().unwrap_or(&SUBROUTINE_STATUS_ERROR);
        self.record.set_subroutine_status(status);
        outcome.map(|_| ())
    }

    /// Returns C `process`'s `status` for this cycle: 0 only when `do_sub` ran
    /// and returned 0.
    fn run_subroutine_body(&mut self) -> CaResult<i64> {
        use crate::server::recgbl::{self, alarm_status};

        // aSub `LFLG=READ`: a `SUBL` re-resolution that found a bad/unregistered
        // name (C `fetch_values` -> `S_db_BadSub`) or failed to read the link
        // signals "skip do_sub this cycle" — C `process` runs `do_sub` only on
        // `!status`. The framework's failed input-link fetch arms the same flag.
        // One-shot: taken (cleared) whether or not a subroutine is set, so it
        // never leaks into the next cycle. The single consumer of the flag,
        // shared by every process path.
        if std::mem::take(&mut self.suppress_subroutine_run) {
            return Ok(SUBROUTINE_STATUS_SKIPPED);
        }

        // Clone the Arc so the borrow on `self.subroutine` is released
        // before we mutate `self.record` / `self.common` below.
        let Some(sub_fn) = self.subroutine.clone() else {
            // C `do_sub` with no bound routine returns `S_db_BadSub`
            // (aSubRecord.c:255-258), so the cycle's status is non-zero.
            return Ok(SUBROUTINE_STATUS_NO_SUB);
        };
        // C `do_sub` returns the subroutine's `long` status.
        let status = sub_fn(&mut *self.record)?;

        // aSub publishes the status as VAL (C `aSubRecord.c:223`
        // `prec->val = status`). The subroutine's computed outputs live in
        // VALA..VALU, so VAL is the return code and overwrites whatever the
        // closure may have written to VAL. `sub` does NOT do this — its VAL
        // is the value the subroutine computed.
        if self.record.record_type() == "aSub" {
            let _ = self
                .record
                .put_field("VAL", EpicsValue::Double(status as f64));
        }

        // A negative status raises SOFT_ALARM at the record's BRSV severity
        // (C `do_sub`: `if (status < 0) recGblSetSevr(SOFT_ALARM,
        // prec->brsv)`). It accumulates into nsta/nsev for this cycle's
        // recGblResetAlarms commit and runs before checkAlarms, so a higher
        // analog severity (e.g. the shared analog-alarm owner) still wins via
        // the raise-only rule. BRSV defaults to NO_ALARM, under which
        // recGblSetSevr is a no-op.
        if status < 0 {
            let brsv = self
                .record
                .get_field("BRSV")
                .and_then(|v| v.to_f64())
                .map(|f| AlarmSeverity::from_u16(f as u16))
                .unwrap_or(AlarmSeverity::NoAlarm);
            recgbl::rec_gbl_set_sevr(&mut self.common, alarm_status::SOFT_ALARM, brsv);
        }
        Ok(status)
    }

    /// The single owner of a process cycle's SUBSCRIBER posts — C `monitor()`'s
    /// "post every subscribed field this cycle touched" loop.
    ///
    /// Every processing path (`process_record_with_links_inner`, the deferred
    /// async-completion path, the simulation path, and [`Self::process_local`])
    /// calls this; none of them may reimplement the rules, because a rule that
    /// holds on one path and not another is a monitor that fires on a scan cycle
    /// but not on an async completion. The per-field mask resolvers
    /// ([`AuxPostMask`], [`crate::server::record::value_gate`]) were already
    /// single-owned for the same reason — this is the loop around them.
    ///
    /// It also UPDATES `last_posted` for everything it emits, and it TAKES the
    /// record's per-cycle post mask ([`Record::take_cycle_posted_fields`]), so
    /// it must run exactly once per cycle.
    ///
    /// The rules, in order:
    ///
    /// * The deadband field (default VAL) and SEVR/STAT/AMSG/UDF are emitted by
    ///   the caller with their own C masks and are skipped here.
    /// * [`Record::event_posted_fields`] post from their own event path
    ///   (waveform HASH) — never from change detection.
    /// * [`Record::process_posted_fields`], when declared, is the closed set of
    ///   fields a process cycle may post at all.
    /// * A secondary value field ([`Record::fields_posted_with_value_mask`])
    ///   carries VAL's monitor mask, gated per its [`ValuePostGate`].
    /// * A CHANGED field carries [`AuxPostMask::mask_for`].
    /// * An UNCHANGED field posts only if the record marked it this cycle:
    ///   statically ([`Record::force_posted_fields`]), per-cycle
    ///   ([`Record::take_cycle_posted_fields`]), on the alarm transition
    ///   ([`Record::alarm_cycle_monitored_fields`]), or in the DBE_LOG sweep
    ///   ([`Record::log_swept_fields`]).
    pub(crate) fn collect_subscriber_posts(
        &mut self,
        deadband_field: &str,
        deadband_mask: EventMask,
        alarm_bits: EventMask,
        aux_post: AuxPostMask,
        include_val: bool,
    ) -> Vec<(String, EpicsValue, EventMask)> {
        use crate::server::record::{ValuePostGate, value_gate};

        // C's default for a change-detected auxiliary post:
        // `monitor_mask | DBE_VALUE | DBE_LOG` (calcRecord.c:420, subRecord.c:400;
        // motor `DBE_VAL_LOG` for marked fields, motorRecord.cc:3522-3645).
        let aux_mask = alarm_bits | EventMask::VALUE | EventMask::LOG;
        let alarm_fanout: &[&str] = if alarm_bits.is_empty() {
            &[]
        } else {
            self.record.alarm_cycle_monitored_fields()
        };
        let force_fields = self.record.force_posted_fields();
        // TAKE — this also clears the state it answers from (C's
        // `pcalc->newm = 0`), which is why this loop may run only once per cycle.
        let cycle_posted = self.record.take_cycle_posted_fields();
        let log_swept = self.record.log_swept_fields();
        let value_masked = self.record.fields_posted_with_value_mask();
        let event_posted = self.record.event_posted_fields();
        let process_posted = self.record.process_posted_fields();

        let mut sub_updates: Vec<(String, EpicsValue, EventMask)> = Vec::new();
        for (field, subs) in &self.subscribers {
            if subs.is_empty()
                || field == deadband_field
                || field == "SEVR"
                || field == "STAT"
                || field == "AMSG"
                || field == "UDF"
                || event_posted.contains(&field.as_str())
                || !process_posted.is_none_or(|allowed| allowed.contains(&field.as_str()))
            {
                continue;
            }
            let Some(val) = self.resolve_field(field) else {
                continue;
            };
            let changed = match self.last_posted.get(field) {
                Some(prev) => prev != &val,
                None => true,
            };
            if let Some(gate) = value_gate(value_masked, field) {
                // C posts this secondary value field with VAL's own monitor_mask,
                // from inside the guard that decides whether VAL posts at all —
                // never a forced DBE_VALUE|DBE_LOG. `ValuePostGate` says whether C
                // also re-tests the field's own value inside that guard (ai RVAL,
                // aiRecord.c:462) or posts it whenever the guard fires (timestamp
                // RVAL, timestampRecord.c:160).
                let post = match gate {
                    ValuePostGate::OnChange => changed && !deadband_mask.is_empty(),
                    ValuePostGate::WithValue => include_val,
                };
                if post {
                    sub_updates.push((field.clone(), val, deadband_mask));
                }
            } else if changed {
                sub_updates.push((
                    field.clone(),
                    val,
                    aux_post.mask_for(field, alarm_bits, deadband_mask),
                ));
            } else if force_fields.contains(&field.as_str())
                || cycle_posted.contains(&field.as_str())
            {
                // C `monitor()` posts a re-marked field with
                // `monitor_mask | DBE_VAL_LOG` even when unchanged — whether the
                // mark is static (`force_posted_fields`) or this cycle's bit mask
                // (`take_cycle_posted_fields`: aCalcout AMASK/NEWM).
                sub_updates.push((field.clone(), val, aux_mask));
            } else if alarm_fanout.contains(&field.as_str()) {
                // C motor `monitor()` (motorRecord.cc:3513-3645) posts every listed
                // field once `monitor_mask != 0`, so a DBE_ALARM-only subscriber
                // observes the alarm moment on any of them.
                sub_updates.push((field.clone(), val, alarm_bits));
            } else if log_swept.contains(&field.as_str()) {
                // C scalerRecord.c:770-787 `monitor()`: every idle process re-posts
                // each S1..Snch with a literal DBE_LOG regardless of change. Sn does
                // not change on an idle cycle, so changed/unchanged stay disjoint
                // (no double post).
                sub_updates.push((field.clone(), val, EventMask::LOG));
            }
        }
        for (field, val, _) in &sub_updates {
            self.last_posted.insert(field.clone(), val.clone());
        }
        sub_updates
    }

    /// Basic process: process record, evaluate alarms, timestamp, build snapshot.
    /// This does NOT handle links — see process_with_context in database.rs.
    ///
    /// Returns the value/log snapshot plus a list of alarm-field posts
    /// (`SEVR`/`STAT`/`AMSG`/`ACKS`) with their individual C event masks.
    /// `SEVR` is posted `DBE_VALUE` only; `STAT`/`AMSG` carry `DBE_ALARM`
    /// (sevr/amsg change) and/or `DBE_VALUE` (stat change). The caller
    /// must fire these via `notify_field` so a `DBE_VALUE`-only `.SEVR`
    /// subscriber is not missed on an alarm-only change and a
    /// `DBE_ALARM`-only subscriber is not wrongly notified — C parity
    /// with `recGblResetAlarms` (recGbl.c:201-220), matching the
    /// `processing.rs` link path.
    pub fn process_local(
        &mut self,
    ) -> CaResult<(
        ProcessSnapshot,
        Vec<(&'static str, crate::server::recgbl::EventMask)>,
    )> {
        use crate::server::recgbl::{self, EventMask};
        const LCNT_ALARM_THRESHOLD: i16 = 10;

        if self
            .processing
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // C `dbProcess` PACT-active guard (dbAccess.c:544-557):
            //
            //   if ((precord->stat == SCAN_ALARM) ||
            //       (precord->lcnt++ < MAX_LOCK) ||
            //       (precord->sevr >= INVALID_ALARM)) goto all_done;
            //   recGblSetSevrMsg(precord, SCAN_ALARM, INVALID_ALARM,
            //                    "Async in progress");
            //
            // The alarm fires EXACTLY ONCE — on the attempt whose
            // pre-increment lcnt equals MAX_LOCK — and is then blocked
            // by the stat == SCAN_ALARM / sevr >= INVALID bails, the
            // same shape as the link path
            // (`process_record_with_links_inner`). The pre-fix guard
            // here used post-increment `lcnt >= threshold` with no
            // already-raised bail, so every reentrant attempt past the
            // threshold re-posted the unchanged SEVR/STAT/VAL (and the
            // first fire came one attempt early); it also wrote
            // sevr/stat directly, skipping `recGblSetSevrMsg` +
            // `recGblResetAlarms` — losing the "Async in progress"
            // AMSG and the acks bookkeeping the reset performs.
            let already_scan_alarm = self.common.stat == recgbl::alarm_status::SCAN_ALARM;
            let already_invalid = self.common.sevr >= AlarmSeverity::Invalid;
            let lcnt_before = self.common.lcnt;
            self.common.lcnt = lcnt_before.saturating_add(1);
            if already_scan_alarm || lcnt_before < LCNT_ALARM_THRESHOLD || already_invalid {
                return Ok((
                    ProcessSnapshot {
                        changed_fields: Vec::new(),
                    },
                    Vec::new(),
                ));
            }
            recgbl::rec_gbl_set_sevr_msg(
                &mut self.common,
                recgbl::alarm_status::SCAN_ALARM,
                AlarmSeverity::Invalid,
                "Async in progress",
            );
            let _ = recgbl::rec_gbl_reset_alarms(&mut self.common);
            // Per-field C masks (recGbl.c:201-220): this guard only
            // runs on a fresh SCAN_ALARM/INVALID raise, so sevr AND
            // stat both moved — SEVR posts DBE_VALUE, STAT/AMSG post
            // the shared `stat_mask` = DBE_ALARM|DBE_VALUE, VAL posts
            // DBE_VALUE|DBE_LOG plus `val_mask` = DBE_ALARM.
            let stat_mask = EventMask::ALARM | EventMask::VALUE;
            let mut changed_fields = Vec::new();
            if let Some(val) = self.record.val() {
                changed_fields.push((
                    "VAL".to_string(),
                    val,
                    EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
                ));
            }
            changed_fields.push((
                "SEVR".to_string(),
                EpicsValue::Short(self.common.sevr as i16),
                EventMask::VALUE,
            ));
            changed_fields.push((
                "STAT".to_string(),
                EpicsValue::Short(self.common.stat as i16),
                stat_mask,
            ));
            // AMSG carries "Async in progress" alongside the STAT
            // transition (C recGbl.c posts STAT and AMSG together
            // when any alarm field moved).
            changed_fields.push((
                "AMSG".to_string(),
                EpicsValue::String(self.common.amsg.clone().into()),
                stat_mask,
            ));
            return Ok((ProcessSnapshot { changed_fields }, Vec::new()));
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

        // Call subroutine if registered (for sub/aSub records). Single owner
        // shared with the main engine path — see `run_registered_subroutine`.
        self.run_registered_subroutine()?;
        // Soft-Channel input records must skip the RVAL->VAL convert
        // (C `devAiSoft.c` `read_ai` returns 2 = "don't convert" for
        // every Soft-Channel input record, incl. one with a constant /
        // unset INP). Without this, `process_local` on a soft input
        // with a preset VAL — e.g. NaN — would run `convert()` and
        // clobber it, after which the UDF check below would see a
        // defined value and wrongly clear UDF. The
        // `processing.rs` link path already does this; `process_local`
        // is the separate foreign-call path (`db.process_record`) and
        // needs the same skip. "Raw Soft Channel" has a distinct DTYP
        // so it is excluded by `is_soft` and still runs convert.
        //
        // Gated on `soft_channel_skips_convert()` — identical to the
        // `processing.rs` link path — so this only suppresses the
        // `RVAL → VAL` convert step. `set_device_did_compute` is an
        // overloaded hook: `ai/bi/mbbi/mbbi_direct` read it as
        // "skip convert" (override true), but `epid` reads it as
        // "skip the whole built-in PID compute" (keeps default false).
        // Without this gate, a Soft-Channel `epid` driven through
        // `process_local` (`db.process_record`, e.g. QSRV group proc
        // members) would skip `do_pid()` entirely — the regression
        // d1032fe5 fixed on the `processing.rs` path only.
        {
            let is_soft = self.common.dtyp.is_empty() || self.common.dtyp == "Soft Channel";
            let is_output = self.record.can_device_write();
            if is_soft && !is_output && self.record.soft_channel_skips_convert() {
                self.record.set_device_did_compute(true);
            }
        }
        // Push framework-owned common state (UDF/PHAS/TSE/TSEL) so the
        // record's process() can see it — same as the processing.rs link
        // path. `process_local` is the foreign-call path
        // (`db.process_record`); without this a record driven through it
        // (e.g. QSRV group-process members) would not see UDF/TSE.
        {
            let ctx = self.common.process_context();
            self.record.set_process_context(&ctx);
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
            // mirror C db_post_events(precord, NULL, DBE_PROPERTY) after record processing.
            let fields: Vec<String> = self.subscribers.keys().cloned().collect();
            for f in fields {
                self.notify_field_with_origin(&f, crate::server::recgbl::EventMask::PROPERTY, 0);
            }
        }

        if process_result == RecordProcessResult::AsyncPending {
            // Async: PACT stays set, no further processing this cycle
            // Don't clear processing flag (guard won't run — we leak it intentionally)
            std::mem::forget(_guard);
            return Ok((
                ProcessSnapshot {
                    changed_fields: Vec::new(),
                },
                Vec::new(),
            ));
        }
        if let RecordProcessResult::AsyncPendingNotify(fields) = process_result {
            // Intermediate notification (e.g. DMOV=0 at move start).
            // Unlike AsyncPending, we DO release the processing flag so
            // subsequent I/O Intr cycles can continue processing normally.
            self.common.time = crate::runtime::general_time::get_current();
            // Filter out fields that haven't actually changed, and update
            // MLST/last_posted for those that have. Each intermediate
            // post carries DBE_VALUE|DBE_LOG — C motor's mid-move
            // `db_post_events` calls use `DBE_VAL_LOG`
            // (motorRecord.cc:2606 DMOV, and every other do_work post);
            // no alarm transition ran on this pending pass.
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
                    changed_fields.push((name, val, EventMask::VALUE | EventMask::LOG));
                }
            }
            // _guard drops here, clearing the processing flag
            return Ok((ProcessSnapshot { changed_fields }, Vec::new()));
        }
        if process_result == RecordProcessResult::CompleteNoEmit {
            // The record accumulated this cycle without emitting (compress
            // `status == 1`). C `compressRecord.c:365` runs the completion
            // epilogue (udf clear, timestamp, monitor, FLNK) only on an emit
            // cycle (`if (status != 1)`), so a non-emitting cycle must publish
            // nothing — skip the epilogue and return an empty snapshot, exactly
            // as the production engine path does in `processing.rs`. This keeps
            // the emit-gate uniform across both process-dispatch paths so the
            // invariant holds by construction, not by "process_local never
            // produces it". CompleteNoEmit is synchronous (PACT already
            // cleared); the `_guard` drops here, clearing the processing flag.
            return Ok((
                ProcessSnapshot {
                    changed_fields: Vec::new(),
                },
                Vec::new(),
            ));
        }

        // `CompleteDeferOutput` (swait ODLY delay-start) is NOT special-cased
        // here: it deliberately shares the Complete value-side snapshot builder
        // below. C `swaitRecord.c::process` posts the value side (`monitor()`,
        // line 475) on the delaying cycle, so building the snapshot now is the
        // correct, parity-matching behavior — unlike `CompleteNoEmit` above,
        // whose fall-through would wrongly emit. The variant's *other* halves —
        // holding PACT across the delay and deferring OUT/OEVT/FLNK to the
        // `ReprocessAfter` continuation — are the engine path's responsibility
        // (`processing.rs::process_record_with_links_inner`); `process_local` is
        // a body-only test helper that dispatches no FLNK/output and no
        // `ProcessAction`, and no test drives a swait ODLY record through it. So
        // the invariant still holds by construction across both dispatch paths:
        // both publish the value side here, both leave the output side to the
        // engine.

        // UDF update before alarm evaluation — C parity (see
        // `processing.rs`). A NaN / undefined value keeps UDF true so
        // `recGblCheckUDF` raises UDF_ALARM this cycle instead of the
        // record reporting a stale/garbage value with no alarm.
        if self.record.clears_udf() {
            self.common.udf = self.record.value_is_undefined();
        }
        // Per-record alarm hook (C `checkAlarms()`).
        self.record.check_alarms(&mut self.common);

        // Evaluate alarms (accumulates into nsta/nsev)
        self.evaluate_alarms();

        // Transfer nsta/nsev → sevr/stat, detect alarm change
        let alarm_result = recgbl::rec_gbl_reset_alarms(&mut self.common);

        self.common.time = crate::runtime::general_time::get_current();
        // UDF already updated above — do not clear unconditionally.

        // Deadband check for VAL monitor filtering
        let (include_val, include_archive) = self.check_deadband_ext();
        // C `recGblResetAlarms` `val_mask = DBE_ALARM`
        // (recGbl.c:194/203/212): every monitored-value post this cycle
        // carries DBE_ALARM when the severity/status OR the alarm
        // message moved — same parity rule as the `processing.rs`
        // paths.
        let alarm_bits = if alarm_result.alarm_changed || alarm_result.amsg_changed {
            EventMask::ALARM
        } else {
            EventMask::NONE
        };

        // Build snapshot
        let mut changed_fields = Vec::new();
        // Same deadband-field routing and per-field mask as the
        // `processing.rs` paths: the tracked field posts the classes
        // that actually fired (MDEL → DBE_VALUE, ADEL → DBE_LOG, alarm
        // movement → DBE_ALARM); a non-primary deadband field (motor
        // RBV — C motor `monitor()`, motorRecord.cc:3468-3507) leaves
        // VAL to the generic change-detection loop below.
        let deadband_field = self.record.monitor_deadband_field();
        // The mask every change-detected aux field posts with — owned by
        // `AuxPostMask`, the same resolver the `processing.rs` paths use, so
        // this builder cannot drift from them on what mask a field carries.
        let aux_post = AuxPostMask::of(self.record.as_ref());
        // The deadband field's post — mask owned by `deadband_post`, the single
        // assembler C's `db_post_events(&prec->val, monitor_mask)` maps to.
        let deadband = self.deadband_post(alarm_bits, include_val, include_archive);
        let deadband_mask = deadband.mask;
        if let Some((field, value)) = deadband.field {
            changed_fields.push((field, value, deadband_mask));
        }
        // C `recGblResetAlarms` (recGbl.c:201-220) posts each alarm
        // field with its OWN per-field mask, not one record-wide mask:
        //   * SEVR — DBE_VALUE, ONLY on a sevr change.
        //   * STAT — DBE_ALARM (sevr change) | DBE_VALUE (stat change).
        //   * ACKS — DBE_VALUE, only when an alarm field moved.
        // Pushing SEVR/STAT into `changed_fields` collapses them onto
        // the single record-wide `event_mask` (which carries ALARM on
        // `alarm_changed`): a DBE_VALUE-only `.SEVR` subscriber would
        // miss a stat-only-driven sevr change, and a DBE_ALARM-only
        // `.SEVR` subscriber would be wrongly notified. Post them via
        // `notify_field` with their individual masks instead — exactly
        // as the `processing.rs` link path does.
        let sevr_changed = self.common.sevr != alarm_result.prev_sevr;
        let stat_changed = self.common.stat != alarm_result.prev_stat;
        let stat_mask = {
            let mut m = EventMask::NONE;
            // C `recGblResetAlarms` carries DBE_ALARM on the STAT/AMSG
            // posts whenever the severity OR the alarm message moved —
            // not on a severity change alone. Aligning with the
            // `processing.rs` link path (and `complete_async_record`).
            if sevr_changed || alarm_result.amsg_changed {
                m |= EventMask::ALARM;
            }
            if stat_changed {
                m |= EventMask::VALUE;
            }
            m
        };
        let mut alarm_posts: Vec<(&'static str, EventMask)> = Vec::new();
        if sevr_changed {
            alarm_posts.push(("SEVR", EventMask::VALUE));
        }
        if !stat_mask.is_empty() {
            alarm_posts.push(("STAT", stat_mask));
            // AMSG shares STAT's mask — C posts it alongside STAT when
            // any alarm field moved.
            alarm_posts.push(("AMSG", stat_mask));
        }
        // C parity (recGbl.c:216): ACKS is posted (DBE_VALUE) only when
        // an alarm field moved (`stat_mask != 0`) AND it was raised.
        if alarm_result.acks_changed && !stat_mask.is_empty() {
            alarm_posts.push(("ACKS", EventMask::VALUE));
        }

        // The cycle's subscriber posts — assembled by the single owner
        // `collect_subscriber_posts`, shared with every `processing.rs` path.
        changed_fields.extend(self.collect_subscriber_posts(
            deadband_field,
            deadband_mask,
            alarm_bits,
            aux_post,
            include_val,
        ));
        // C waveform/aai/aao `monitor()` posts HASH with a literal
        // `DBE_VALUE` only on a content-hash change (waveformRecord.c:
        // 317-319), independent of the VAL post mask. `array_hash_changed`
        // was set by `check_deadband_ext` this cycle.
        if self.array_hash_changed {
            if let Some(h) = self.resolve_field("HASH") {
                changed_fields.push(("HASH".to_string(), h, EventMask::VALUE));
            }
        }

        // Post UDF on the snapshot whenever any monitor event fires this
        // cycle, carrying the union of the cycle's posted classes —
        // mirrors the two `processing.rs` UDF pushes. C
        // `recGblResetAlarms` / `recGblCheckUDF` (recGbl.c) keep UDF
        // current every process cycle, and `db_post_events` delivers
        // `.UDF` alongside the record-wide post. `process_local` is the
        // foreign-process path (`db.process_record`, e.g. QSRV
        // group-process members); without this push a UDF change here is
        // never delivered to `.UDF` subscribers — the `sub_updates` loop
        // above deliberately excludes UDF to avoid a double-post, so the
        // push must be here.
        let cycle_mask = changed_fields
            .iter()
            .fold(EventMask::NONE, |m, (_, _, fm)| m | *fm);
        if !cycle_mask.is_empty() {
            changed_fields.push((
                "UDF".to_string(),
                EpicsValue::Char(if self.common.udf { 1 } else { 0 }),
                cycle_mask,
            ));
        }

        Ok((ProcessSnapshot { changed_fields }, alarm_posts))
    }

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

    /// Check MDEL/ADEL deadbands for VAL monitor/archive filtering.
    /// Returns `(monitor_trigger, archive_trigger)`.
    ///
    /// Updates `MLST`/`ALST` (record-owned) and the `CommonFields`
    /// `mlst/alst` shadow when a trigger fires. Records without
    /// MDEL/ADEL (e.g. motor) default to deadband=0 (any actual
    /// change triggers).
    ///
    /// Delegates per-axis deadband comparison to the free function
    /// [`check_deadband`] below — see that function's docstring for
    /// the four-quadrant NaN/infinity rule mirroring C
    /// `recGblCheckDeadband` (recGbl.c:345-370).
    ///
    /// **C-parity design note**: the Rust port uses `NaN` as the
    /// "never posted" sentinel for `MLST`/`ALST`. C achieves the
    /// same first-publish guarantee by allocating MLST/ALST in
    /// BSS-zeroed storage with a value of 0.0 that the C code is
    /// allowed to match against — but the first observed value is
    /// not necessarily 0.0, and the C rule "MLST==0 means never
    /// posted" relies on the deadband comparison `abs(val - 0.0)`
    /// firing on any non-zero first value. NaN is strictly more
    /// correct for the Rust port because a legitimate first
    /// `val=0.0` still fires on `NaN.is_nan() → true`. This
    /// sentinel-as-design is intentional, documented inside
    /// [`check_deadband`] (the `oldval.is_nan() → return true` short
    /// circuit). It is NOT a deviation inherited from an earlier
    /// silent compromise — `record_tests.rs::deadband_*` pins both
    /// the NaN-sentinel behaviour and the C four-quadrant transitions.
    /// The single owner of the deadband field's monitor post — C `monitor()`'s
    /// `db_post_events(&prec->val, monitor_mask)`, the one post every record
    /// makes for the value it deadbands.
    ///
    /// [`Self::check_deadband_ext`] decides WHETHER the MDEL/ADEL classes fired;
    /// this decides what the resulting post looks like, and it is the only place
    /// that assembles that mask. The three `processing.rs` snapshot builders and
    /// the `notify_monitors` path all route through here, so a record's mask rule
    /// cannot hold on one processing path and not another.
    ///
    /// Two record hooks strip C's `DBE_LOG` from the post:
    ///
    /// * [`Record::value_only_change_fields`] — C posts a literal `DBE_VALUE`
    ///   (scaler VAL, scalerRecord.c:478).
    /// * [`Record::fields_posted_with_monitor_mask`] — C posts
    ///   `monitor_mask | DBE_VALUE` (event VAL, eventRecord.c:163). `monitor_mask`
    ///   there is `recGblResetAlarms`'s return, i.e. the alarm bits alone, so the
    ///   post carries `DBE_VALUE` (+ `DBE_ALARM` when the alarm moved) and never
    ///   the archive `DBE_LOG` — an event's VAL reaches a `DBE_LOG` archiver on
    ///   no cycle at all.
    ///
    /// [`DeadbandPost::field`] is `None` when no class fired, i.e. when C's
    /// `if (monitor_mask)` guard would skip the post.
    pub(crate) fn deadband_post(
        &self,
        alarm_bits: EventMask,
        include_val: bool,
        include_archive: bool,
    ) -> DeadbandPost {
        let field = self.record.monitor_deadband_field();
        let log_suppressed = self.record.value_only_change_fields().contains(&field)
            || self
                .record
                .fields_posted_with_monitor_mask()
                .contains(&field);

        let mut mask = alarm_bits;
        if include_val {
            mask |= EventMask::VALUE;
        }
        if include_archive && !log_suppressed {
            mask |= EventMask::LOG;
        }

        let value = if mask.is_empty() {
            None
        } else if field == "VAL" {
            self.record.val()
        } else {
            self.resolve_field(field)
        };
        DeadbandPost {
            mask,
            field: value.map(|v| (field.to_string(), v)),
        }
    }

    pub fn check_deadband_ext(&mut self) -> (bool, bool) {
        // C waveform/aai/aao `monitor()` (waveformRecord.c:291-326) replaces
        // the analog MDEL/ADEL deadband with the MPST/APST "Always vs On
        // Change" mechanism: the record hashes its array content and posts
        // `DBE_VALUE`/`DBE_LOG` either always or only when the hash changed,
        // and posts `HASH` (`DBE_VALUE`) on a hash change. The record owns
        // the hash compute + `HASH` update; `array_hash_changed` carries the
        // event to the snapshot builders, which post `HASH` (the field is
        // excluded from the generic change-detection loop via
        // `event_posted_fields`).
        if let Some(post) = self.record.array_monitor_post() {
            self.array_hash_changed = post.hash_changed;
            return (post.post_value, post.post_archive);
        }
        self.array_hash_changed = false;

        // The deadband is evaluated against `monitor_deadband_value()`,
        // not `val()` directly: a record whose monitored quantity is
        // not its primary value (e.g. the motor record, VAL=setpoint /
        // RBV=readback — C `monitor()` deadbands RBV) overrides that
        // hook. Default is `val()`, so other records are unaffected.
        let val = match self
            .record
            .monitor_deadband_value()
            .and_then(|v| v.to_f64())
        {
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

        let monitor_trigger = check_deadband(val, mlst, mdel);
        let archive_trigger = check_deadband(val, alst, adel);

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
    fn make_monitor_snapshot(
        &self,
        field: &str,
        value: EpicsValue,
    ) -> super::super::snapshot::Snapshot {
        let mut snap = super::super::snapshot::Snapshot::new(
            value,
            self.common.stat,
            self.common.sevr as u16,
            self.common.time,
        );
        // Carry the record's `utag` into the monitor update's
        // `timeStamp.userTag`, same as the GET path
        // (`snapshot_for_field`) and pvxs `iocsource.cpp:245`. Narrows
        // the 64-bit `epicsUTag` to the int32 wire field by low-32-bit
        // truncation.
        snap.user_tag = self.common.utag as i32;
        let meta = self.cached_metadata();
        snap.display = meta.display;
        snap.control = meta.control;
        snap.enums = meta.enums;
        // Per-field RSET metadata, same as the GET path
        // (`snapshot_for_field`) — a monitor update for VELO must carry
        // VELO's limits, not the record-level VAL limits.
        self.apply_field_metadata_override(field, &mut snap);
        // A monitored DBF_MENU field carries the same DBR_ENUM value and
        // choice labels as the GET path, so a `camonitor`/`pvmonitor`
        // update shows the menu label, not a bare index.
        self.attach_menu_enum(field, &mut snap);
        snap
    }

    /// Apply a record's per-field metadata override (C RSET
    /// `get_units`/`get_precision`/`get_graphic_double`/
    /// `get_control_double`/`get_alarm_double`, all keyed by field)
    /// over the cached record-level metadata. Shared by the GET and
    /// monitor snapshot builders. Computed live on every call — never
    /// cached — so overrides derived from fields outside the
    /// `is_metadata_field` set cannot go stale.
    fn apply_field_metadata_override(
        &self,
        field: &str,
        snap: &mut super::super::snapshot::Snapshot,
    ) {
        let Some(ov) = self.record.field_metadata_override(field) else {
            return;
        };
        if ov.units.is_some()
            || ov.precision.is_some()
            || ov.disp_limits.is_some()
            || ov.alarm_limits.is_some()
        {
            let d = snap.display.get_or_insert_with(Default::default);
            if let Some(units) = ov.units {
                d.units = units;
            }
            if let Some(precision) = ov.precision {
                d.precision = precision;
            }
            if let Some((upper, lower)) = ov.disp_limits {
                d.upper_disp_limit = upper;
                d.lower_disp_limit = lower;
            }
            if let Some((hihi, high, low, lolo)) = ov.alarm_limits {
                d.upper_alarm_limit = hihi;
                d.upper_warning_limit = high;
                d.lower_warning_limit = low;
                d.lower_alarm_limit = lolo;
            }
        }
        if let Some((upper, lower)) = ov.ctrl_limits {
            let c = snap.control.get_or_insert_with(Default::default);
            c.upper_ctrl_limit = upper;
            c.lower_ctrl_limit = lower;
        }
    }

    /// Notify subscribers from a snapshot (call outside lock).
    /// Each entry carries its own posting mask: only subscribers whose
    /// mask intersects that field's mask are notified, and the
    /// delivered [`MonitorEvent`] reports exactly that field's classes
    /// (C `db_post_events(prec, &field, mask)` per-field granularity).
    pub fn notify_from_snapshot(&self, snapshot: &ProcessSnapshot) {
        use crate::server::database::filters::FilteredMonitorEvent;
        use crate::server::recgbl::EventMask;

        for (field, value, posting_mask) in &snapshot.changed_fields {
            let posting_mask = *posting_mask;
            if let Some(subs) = self.subscribers.get(field) {
                // Build a full snapshot once per field (with display metadata)
                let mon_snap = self.make_monitor_snapshot(field, value.clone());
                for sub in subs {
                    // Paused subscriber (`db_event_disable`): suppress at
                    // the source — no delivery, no coalesce.
                    if !sub.active {
                        continue;
                    }
                    let sub_mask = EventMask::from_bits(sub.mask);
                    // Only send when posting mask intersects subscriber mask.
                    // Empty posting mask means nothing changed — skip.
                    if !posting_mask.is_empty() && sub_mask.intersects(posting_mask) {
                        let event = MonitorEvent {
                            snapshot: mon_snap.clone(),
                            origin: 0,
                            mask: posting_mask,
                        };
                        // Server-side filter chain (3.15.7). Empty chain
                        // is identity, so no behaviour change for the
                        // common no-filter case.
                        let filtered = if sub.filters.is_empty() {
                            Some(event)
                        } else {
                            sub.filters
                                .apply(FilteredMonitorEvent::new(event))
                                .map(|fe| fe.event)
                        };
                        let Some(event) = filtered else {
                            continue;
                        };
                        // C `db_queue_event_log`: append, or replace this
                        // monitor's last queued entry in place when the queue
                        // is in flow control or nearly full. The queue owns
                        // that decision and counts the displaced value.
                        sub.post(event);
                    }
                }
            }
        }
    }

    /// Notify subscribers of a specific field, filtering by event mask.
    pub fn notify_field(&self, field: &str, mask: crate::server::recgbl::EventMask) {
        self.notify_field_with_origin(field, mask, 0);
    }

    /// C `db_post_events(precord, NULL, DBE_ALARM)`: post a record-wide
    /// alarm event. Delivers to every subscriber on any field whose mask
    /// includes DBE_ALARM, each carrying its own monitored field's current
    /// value (the per-field `notify_field` already filters by mask
    /// intersection). Used by the alarm-acknowledge (ACKT/ACKS) put path so
    /// an alarm-mask monitor on any field observes the acknowledgement.
    pub fn notify_record_alarm(&self) {
        let fields: Vec<String> = self.subscribers.keys().cloned().collect();
        for field in fields {
            self.notify_field(&field, crate::server::recgbl::EventMask::ALARM);
        }
    }

    /// Notify subscribers with an origin tag for self-write filtering.
    pub fn notify_field_with_origin(
        &self,
        field: &str,
        mask: crate::server::recgbl::EventMask,
        origin: u64,
    ) {
        use crate::server::database::filters::FilteredMonitorEvent;
        if let Some(subs) = self.subscribers.get(field) {
            if let Some(value) = self.resolve_field(field) {
                let mon_snap = self.make_monitor_snapshot(field, value);
                for sub in subs {
                    // Paused subscriber (`db_event_disable`): suppress at
                    // the source — no delivery, no coalesce.
                    if !sub.active {
                        continue;
                    }
                    let sub_mask = crate::server::recgbl::EventMask::from_bits(sub.mask);
                    if mask.is_empty() || sub_mask.intersects(mask) {
                        let event = MonitorEvent {
                            snapshot: mon_snap.clone(),
                            origin,
                            mask,
                        };
                        // Server-side filter chain (3.15.7). Empty
                        // chain (the default for every subscriber
                        // until a `.{filter:opts}` PV-name suffix
                        // parser wires one in) is the identity, so
                        // existing subscribers see no behaviour
                        // change. A filter returning `None` silences
                        // this event for this subscriber only.
                        let filtered = if sub.filters.is_empty() {
                            Some(event)
                        } else {
                            sub.filters
                                .apply(FilteredMonitorEvent::new(event))
                                .map(|fe| fe.event)
                        };
                        let Some(event) = filtered else {
                            continue;
                        };
                        // Same single post owner as the snapshot path.
                        sub.post(event);
                    }
                }
            }
        }
    }

    /// Add a subscriber for a specific field. Returns `None` when the
    /// per-field subscriber cap (`EPICS_CAS_MAX_SUBSCRIBERS_PER_PV`)
    /// is reached. the parallel cap on `ProcessVariable`
    /// defends against a misbehaving client opening many
    /// MONITOR ops against one shared PV; the same defence is needed
    /// for record fields, which the CA server's
    /// `ChannelTarget::RecordField` path lands on.
    pub fn add_subscriber(
        &mut self,
        field: &str,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<EventReader> {
        self.add_subscriber_on(&EventUser::new(), field, sid, data_type, mask)
    }

    /// Add a field subscriber whose events queue on `user`'s event queue —
    /// C `db_add_event` with the circuit's `event_user` as context. Every
    /// subscription on one CA circuit shares that queue and therefore its
    /// `nDuplicates`, so a duplicate queued for one of them releases the
    /// EVENTS_OFF drain for all of them (`dbEvent.c:947`). In-process consumers
    /// use [`Self::add_subscriber`], which gives each its own `event_user`.
    pub fn add_subscriber_on(
        &mut self,
        user: &EventUser,
        field: &str,
        sid: u32,
        data_type: DbFieldType,
        mask: u16,
    ) -> Option<EventReader> {
        let cap = crate::server::pv::max_subscribers_per_pv();
        let field_str = field.to_string();
        let bucket = self.subscribers.entry(field_str.clone()).or_default();
        // Reap rows whose consumer is gone before
        // counting against the cap. A record field whose value
        // never changes (e.g. a quasi-static catalog field) never
        // triggers `notify_field_with_origin`'s retain-filter, so
        // a long-lived subscribe-disconnect storm could pin the
        // bucket at `cap` worth of dead rows and lock out
        // genuine new subscribers.
        bucket.retain(|s| !s.is_closed());
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
        let (sink, reader) = crate::server::event_queue::attach(user, sid);
        bucket.push(Subscriber {
            sid,
            data_type,
            mask,
            sink,
            filters: crate::server::database::filters::FilterChain::new(),
            active: true,
        });
        // Initialize last_posted with current value so the first process cycle
        // doesn't treat it as "changed" (the initial value is already sent
        // to the client as part of EVENT_ADD response).
        if !self.last_posted.contains_key(&field_str) {
            if let Some(val) = self.resolve_field(&field_str) {
                self.last_posted.insert(field_str, val);
            }
        }
        Some(reader)
    }

    /// Attach a filter to the most recently added subscriber for
    /// `field`. Returns `false` when no subscriber exists yet on that
    /// field (call `add_subscriber` first). The CA / PVA channel-name
    /// parsers will use this once `.{filter:opts}` syntax is wired.
    /// Tests can also use it directly to compose filter chains.
    pub fn attach_filter_to_last_subscriber(
        &mut self,
        field: &str,
        filter: std::sync::Arc<dyn crate::server::database::filters::SubscriptionFilter>,
    ) -> bool {
        if let Some(bucket) = self.subscribers.get_mut(field) {
            if let Some(sub) = bucket.last_mut() {
                sub.filters.push(filter);
                return true;
            }
        }
        false
    }

    /// Remove a subscriber by subscription ID from all fields.
    pub fn remove_subscriber(&mut self, sid: u32) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| s.sid != sid);
        }
    }

    /// Pause / resume one subscriber's event flow at the source
    /// (`db_event_disable` / `db_event_enable`). `active == false`
    /// suppresses every subsequent post to this subscriber, so the record stops
    /// doing per-event work for it. Entries already queued stay queued and are
    /// still delivered, exactly as in C: `db_event_disable` only unlinks the
    /// subscription from the record's monitor list (`dbEvent.c:521-533`) and
    /// never reaches into the event queue. No-op if no subscriber has this
    /// `sid`. The caller holds the record write lock, so this is exclusive with
    /// the read-locked post paths that consult `Subscriber::active`.
    pub fn set_subscriber_active(&mut self, sid: u32, active: bool) {
        for subs in self.subscribers.values_mut() {
            for sub in subs.iter_mut() {
                if sub.sid == sid {
                    sub.active = active;
                }
            }
        }
    }

    /// Clean up subscriber rows whose consumer is gone.
    pub fn cleanup_subscribers(&mut self) {
        for subs in self.subscribers.values_mut() {
            subs.retain(|s| !s.is_closed());
        }
    }
}

/// C `recGblCheckDeadband` parity (recGbl.c:345-370). The four branches
/// the C path enumerates:
///
/// 1. Both `newval` and `oldval` finite: `delta = |old - new|`, fire when
///    `delta > deadband`.
/// 2. Exactly one of {newval, oldval} is NaN, the other not — OR exactly
///    one is +/-inf, the other not: `delta = +inf`, always fires.
/// 3. Both infinite with opposite signs: `delta = +inf`, always fires.
/// 4. Otherwise (e.g. both NaN, both same-signed infinity): no fire.
///
/// `oldval = NaN` is treated as "never posted" and fires (matches the
/// `mlst.is_nan() → trigger` short-circuit the Rust port already had).
/// `deadband < 0` fires unconditionally (matches `delta > deadband`
/// with a negative deadband — same effect on every numeric value).
pub(crate) fn check_deadband(newval: f64, oldval: f64, deadband: f64) -> bool {
    // Fire unconditionally when no prior posting has happened. C achieves
    // the same effect through the field being default-initialised to a
    // sentinel; Rust uses NaN-as-sentinel.
    if oldval.is_nan() {
        return true;
    }
    // Negative deadband short-circuits — any value passes.
    if deadband < 0.0 {
        return true;
    }
    let new_finite = newval.is_finite();
    let old_finite = oldval.is_finite();
    if new_finite && old_finite {
        return (newval - oldval).abs() > deadband;
    }
    // From here on, at least one of the two is not finite. We've already
    // ruled out oldval=NaN above, so any newval=NaN here is the "newval
    // went NaN while oldval was finite/inf" case — must fire (C case 2).
    if newval.is_nan() {
        return true;
    }
    // Exactly one infinite, the other finite: C case 2 → fire.
    if new_finite != old_finite {
        return true;
    }
    // Both infinite. Opposite signs → fire (C case 3); same sign → no
    // fire (C path leaves delta=0 and the `delta > deadband` check fails
    // for any non-negative deadband).
    newval != oldval
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

    /// a record-field monitor whose event queue has run short of room
    /// replaces its last queued entry in place (C `db_queue_event_log`,
    /// `dbEvent.c:812-820`), and the displaced value — which the consumer never
    /// observed — must be counted in the shared `dropped_monitor_events()`
    /// counter (C `nreplace`), the same accounting a `ProcessVariable` post
    /// uses. Before the fix the record-field path overwrote its coalesce slot
    /// without counting, hiding slow-consumer loss on the path most CA/PVA
    /// database monitors use. The counter is process-global, so the assertion is
    /// a strict monotonic increase (robust under parallel tests); the
    /// revert-verify runs this test in isolation.
    #[test]
    fn bfr10_record_field_overflow_counts_dropped_event() {
        use crate::server::event_queue::{event_que_size, events_per_que};
        use crate::server::pv::dropped_monitor_events;
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        // Keep the reader alive and do NOT drain, so the ring fills to the
        // replace threshold and later posts displace the tail entry.
        let _reader = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added");
        let before = dropped_monitor_events();
        let posts = event_que_size() - events_per_que() + 10;
        for _ in 0..posts {
            inst.notify_field_with_origin("VAL", EventMask::VALUE, 0);
        }
        let after = dropped_monitor_events();
        assert!(
            after > before,
            "a post that replaces an unobserved queued entry must record a \
             dropped monitor event (before={before}, after={after})"
        );
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
    fn q_form_info_tag_sets_display_form_index() {
        // pvxs maps the `Q:form` info tag to `display.form.index` for the
        // VAL field (iocsource.cpp:42-62). "Hex" is slot 4 of the
        // seven-entry menu (Default/String/Binary/Decimal/Hex/...).
        let mut inst = ai_instance();
        inst.set_info("Q:form", "Hex");
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let display = snap.display.expect("ai snapshot must have display");
        assert_eq!(display.form, 4, "Q:form=Hex -> display.form index 4");
    }

    #[test]
    fn q_form_absent_or_unknown_leaves_form_default() {
        // No `Q:form` tag -> form stays 0 (Default).
        let inst = ai_instance();
        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.display.expect("ai display").form, 0);

        // Unrecognised tag -> pvxs leaves the index untouched (0).
        let mut inst2 = ai_instance();
        inst2.set_info("Q:form", "Nonsense");
        let snap2 = inst2.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap2.display.expect("ai display").form, 0);
    }

    /// `info(Q:time:tag)` resolves to pvxs's `nsecMask`
    /// (`ioc/typeutils.cpp:79-88`). The prefix test there is a byte-exact
    /// `strncmp("nsec:lsb:", 9)` and the digit count is fed straight to
    /// `(uint64_t(1u)<<dig)-1u` — no case folding, no whitespace tolerance
    /// around the prefix, and no bounds clamp. Each boundary gets a case.
    #[test]
    fn qtime_nsec_mask_matches_pvxs_updatensecmask() {
        let cases: &[(&str, u64)] = &[
            // parses: `epicsParseInt32` skips whitespace around the digits
            // and accepts a sign.
            ("nsec:lsb:20", (1 << 20) - 1),
            ("nsec:lsb:1", 1),
            ("nsec:lsb: 4 ", 0xF),
            ("nsec:lsb:+4", 0xF),
            // no clamp: 31 is the mask pvxs actually serves (the old Rust
            // `(1..=30)` guard dropped it), and 0 is pvxs's "off" mask.
            ("nsec:lsb:31", 0x7FFF_FFFF),
            ("nsec:lsb:0", 0),
            // `strncmp` is byte-exact: case-folded or whitespace-split
            // prefixes do not match, so pvxs leaves `nsecMask` at 0.
            ("NSEC:LSB:4", 0),
            ("Nsec:Lsb:4", 0),
            ("nsec: lsb: 4", 0),
            (" nsec:lsb:4", 0),
            // `epicsParseInt32` failures: no conversion, extraneous trailing
            // bytes, overflow past epicsInt32.
            ("nsec:lsb:", 0),
            ("nsec:lsb:abc", 0),
            ("nsec:lsb:4x", 0),
            ("nsec:lsb:4 5", 0),
            ("nsec:lsb:99999999999999999999", 0),
            ("nsec:lsb:2147483648", 0),
        ];
        for (tag, want) in cases {
            let mut inst = ai_instance();
            inst.set_info("Q:time:tag", *tag);
            assert_eq!(
                inst.qtime_nsec_mask(),
                *want,
                "info(Q:time:tag, {tag:?}) must resolve to nsecMask {want:#x}"
            );
        }
        // Tag absent entirely → pvxs never enters the `if(auto val = ...)`
        // body and `nsecMask` stays 0.
        assert_eq!(ai_instance().qtime_nsec_mask(), 0);
    }

    /// End-to-end on the snapshot: `nsec:lsb:31` publishes
    /// `nanoseconds & ~mask` (0, since nanoseconds < 1e9 < 2^31) and
    /// `userTag = nanoseconds & mask` (pvxs `iocsource.cpp:239-248`). The
    /// old `(1..=30)` clamp served the raw nanoseconds and the record's
    /// utag instead.
    #[test]
    fn qtime_nsec_lsb_31_is_served_not_ignored() {
        use std::time::{Duration, SystemTime};
        let mut inst = ai_instance();
        inst.common.time = SystemTime::UNIX_EPOCH + Duration::new(42, 123_456_789);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "nsec:lsb:31");

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(snap.user_tag, 123_456_789);
        assert_eq!(snap.timestamp.subsec_nanos(), 0);
        assert_eq!(snap.timestamp.unix_secs(), 42);
    }

    /// The mirror boundary: a tag pvxs's `strncmp` rejects must leave the
    /// timestamp and the record's own utag alone. The old case-insensitive
    /// split matched `NSEC:LSB:4` and masked the wire timestamp pvxs serves
    /// unmasked.
    #[test]
    fn qtime_uppercase_tag_leaves_timestamp_untouched() {
        use std::time::{Duration, SystemTime};
        let mut inst = ai_instance();
        inst.common.time = SystemTime::UNIX_EPOCH + Duration::new(42, 123_456_789);
        inst.common.utag = 5;
        inst.set_info("Q:time:tag", "NSEC:LSB:4");

        let snap = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(
            snap.user_tag, 5,
            "record utag must survive a non-matching tag"
        );
        assert_eq!(snap.timestamp.subsec_nanos(), 123_456_789);
    }

    /// the served `timeStamp.userTag` defaults to the record's `utag`
    /// (pvxs `iocsource.cpp:245`), on both the GET (`snapshot_for_field`)
    /// and MONITOR (`make_monitor_snapshot`) paths. Pre-fix both hard-set
    /// it to 0, dropping the record's tag. A bit-31 utag also pins the
    /// `u64 -> i32` narrowing: the low 32 bits' pattern is preserved
    /// (no clamp), matching pvxs assigning `epicsUTag` into the `Int32`
    /// wire field.
    #[test]
    fn snapshot_serves_record_utag_as_timestamp_usertag() {
        let mut inst = ai_instance();
        // no `info(Q:time:tag, ...)` on this record, so the nsec-LSB
        // override never fires and the utag default is what is served.
        inst.common.utag = 0x9000_0000;
        let want = 0x9000_0000u32 as i32;

        let get = inst.snapshot_for_field("VAL").unwrap();
        assert_eq!(
            get.user_tag, want,
            "GET path must serve the record's utag as timeStamp.userTag"
        );

        let mon = inst.make_monitor_snapshot("VAL", EpicsValue::Double(1.0));
        assert_eq!(
            mon.user_tag, want,
            "MONITOR path must carry the record's utag too"
        );
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

    /// epics-base faac1df1 — `notify_field_written_if_changed` must
    /// SKIP the cache invalidation when the metadata field's value
    /// didn't actually change. Otherwise a stream of idempotent puts
    /// from a CSS panel binds DBE_PROPERTY subscribers to bogus
    /// "property changed" events on every cycle.
    #[test]
    fn notify_field_written_if_changed_skips_when_unchanged() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Capture prev, do a no-op put, then notify — cache must remain.
        let prev = inst.record.get_field("EGU");
        let _ = inst.record.put_field("EGU", prev.clone().unwrap());
        inst.notify_field_written_if_changed("EGU", prev.as_ref());
        assert!(
            inst.metadata_cache.lock().unwrap().is_some(),
            "no-op put must not invalidate the metadata cache"
        );
    }

    /// And when the value DID change, the cache must invalidate.
    #[test]
    fn notify_field_written_if_changed_invalidates_on_real_change() {
        let mut inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        let prev = inst.record.get_field("EGU");
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref());
        assert!(
            inst.metadata_cache.lock().unwrap().is_none(),
            "real metadata change must invalidate cache"
        );
    }

    /// Non-metadata fields don't carry property semantics — the
    /// `if_changed` variant must never invalidate for them, matching
    /// the existing `notify_field_written` short-circuit.
    #[test]
    fn notify_field_written_if_changed_skips_non_metadata_field() {
        let inst = ai_instance();
        let _ = inst.snapshot_for_field("VAL");
        assert!(inst.metadata_cache.lock().unwrap().is_some());
        // VAL is not in is_metadata_field set — must be skipped even
        // with a changed value.
        inst.notify_field_written_if_changed("VAL", None);
        assert!(inst.metadata_cache.lock().unwrap().is_some());
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
        let snap = inst.make_monitor_snapshot("VAL", EpicsValue::Double(42.0));
        assert!(snap.display.is_some());
        assert!(inst.metadata_cache.lock().unwrap().is_some());

        // Subsequent call hits cache
        let snap2 = inst.make_monitor_snapshot("VAL", EpicsValue::Double(43.0));
        let d1 = snap.display.unwrap();
        let d2 = snap2.display.unwrap();
        assert_eq!(d1.units, d2.units);
        assert_eq!(d1.precision, d2.precision);
    }

    /// Stub record with a per-field metadata override on SPD only —
    /// models a C RSET whose get_units/get_graphic_double key on
    /// dbGetFieldIndex (e.g. motorRecord.cc:3156-3361).
    struct PerFieldMetaRecord;

    impl Record for PerFieldMetaRecord {
        fn record_type(&self) -> &'static str {
            "ai" // record-level metadata populates from EGU/PREC/HOPR/LOPR
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" | "SPD" => Some(EpicsValue::Double(1.0)),
                "EGU" => Some(EpicsValue::String("mm".into())),
                "PREC" => Some(EpicsValue::Short(3)),
                "HOPR" => Some(EpicsValue::Double(100.0)),
                "LOPR" => Some(EpicsValue::Double(-100.0)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        fn field_metadata_override(
            &self,
            field: &str,
        ) -> Option<crate::server::record::FieldMetadataOverride> {
            if field != "SPD" {
                return None;
            }
            Some(crate::server::record::FieldMetadataOverride {
                units: Some("mm/sec".into()),
                precision: Some(1),
                disp_limits: Some((5.0, 0.5)),
                ctrl_limits: Some((4.0, 1.0)),
                alarm_limits: Some((9.0, 8.0, -8.0, -9.0)),
            })
        }
    }

    #[test]
    fn field_metadata_override_applies_on_get_and_monitor_paths() {
        let inst = RecordInstance::new("PFM".to_string(), PerFieldMetaRecord);

        // VAL: no override — record-level metadata serves it.
        let snap = inst.snapshot_for_field("VAL").unwrap();
        let d = snap.display.unwrap();
        assert_eq!(d.units, "mm");
        assert_eq!(d.precision, 3);
        assert_eq!(d.upper_disp_limit, 100.0);

        // SPD via the GET path: every member patched over the cache.
        let snap = inst.snapshot_for_field("SPD").unwrap();
        let d = snap.display.unwrap();
        assert_eq!(d.units, "mm/sec");
        assert_eq!(d.precision, 1);
        assert_eq!((d.upper_disp_limit, d.lower_disp_limit), (5.0, 0.5));
        assert_eq!(
            (
                d.upper_alarm_limit,
                d.upper_warning_limit,
                d.lower_warning_limit,
                d.lower_alarm_limit
            ),
            (9.0, 8.0, -8.0, -9.0)
        );
        let c = snap.control.unwrap();
        assert_eq!((c.upper_ctrl_limit, c.lower_ctrl_limit), (4.0, 1.0));

        // SPD via the monitor path: identical override.
        let snap = inst.make_monitor_snapshot("SPD", EpicsValue::Double(2.0));
        let d = snap.display.unwrap();
        assert_eq!(d.units, "mm/sec");
        assert_eq!((d.upper_disp_limit, d.lower_disp_limit), (5.0, 0.5));
        let c = snap.control.unwrap();
        assert_eq!((c.upper_ctrl_limit, c.lower_ctrl_limit), (4.0, 1.0));
    }

    /// Stub modelling the motor monitor() shape (C motorRecord.cc:
    /// 3468-3507): VAL is a setpoint, the MDEL/ADEL deadband tracks
    /// the RBV readback, which advances on every process.
    struct ReadbackDeadbandRecord {
        val: f64,
        rbv: f64,
        deadband: f64,
    }

    impl Record for ReadbackDeadbandRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            self.rbv += 30.0;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "RBV" => Some(EpicsValue::Double(self.rbv)),
                "MDEL" | "ADEL" => Some(EpicsValue::Double(self.deadband)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("VAL", EpicsValue::Double(v)) => {
                    self.val = v;
                    Ok(())
                }
                ("MDEL", EpicsValue::Double(v)) => {
                    self.deadband = v;
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        fn monitor_deadband_value(&self) -> Option<EpicsValue> {
            Some(EpicsValue::Double(self.rbv))
        }
        fn monitor_deadband_field(&self) -> &'static str {
            "RBV"
        }
    }

    /// C motor monitor() parity: MDEL/ADEL throttle the deadband
    /// field's (RBV) delivery; VAL posts only when the setpoint
    /// actually changed — not on every readback poll.
    #[test]
    fn deadband_field_routes_readback_and_val_posts_only_on_change() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "RDB".to_string(),
            ReadbackDeadbandRecord {
                val: 5.0,
                rbv: 0.0,
                deadband: 10.0,
            },
        );
        let _val_rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("VAL subscriber");
        let _rbv_rx = inst
            .add_subscriber(
                "RBV",
                2,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("RBV subscriber");
        let names = |snap: &ProcessSnapshot| {
            snap.changed_fields
                .iter()
                .map(|(n, _, _)| n.clone())
                .collect::<Vec<_>>()
        };

        // Cycle 1 (first publish): RBV fires via the deadband trigger
        // (MLST starts at the NaN never-posted sentinel). VAL must NOT
        // post: `add_subscriber` seeded `last_posted` with the current
        // value (the initial value already went out with EVENT_ADD), and
        // C monitor() posts VAL only when MARKED(M_VAL) — nothing marked
        // it.
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.contains(&"RBV".to_string()), "{n:?}");
        assert!(
            !n.contains(&"VAL".to_string()),
            "VAL unchanged since subscribe must not post: {n:?}"
        );

        // Cycle 2: RBV moved past MDEL, VAL unchanged → RBV posted,
        // VAL not re-posted.
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(n.contains(&"RBV".to_string()), "RBV crossed MDEL: {n:?}");
        assert!(
            !n.contains(&"VAL".to_string()),
            "unchanged VAL must not post: {n:?}"
        );

        // Cycle 3: widen the deadband — RBV moves within it → throttled.
        let _ = inst.record.put_field("MDEL", EpicsValue::Double(1000.0));
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(
            !n.contains(&"RBV".to_string()),
            "MDEL must throttle RBV: {n:?}"
        );

        // Cycle 4: setpoint moves while RBV stays inside the deadband →
        // VAL posts via change detection, RBV stays throttled.
        let _ = inst.record.put_field("VAL", EpicsValue::Double(42.0));
        let (snap, _) = inst.process_local().unwrap();
        let n = names(&snap);
        assert!(
            n.contains(&"VAL".to_string()),
            "changed VAL must post: {n:?}"
        );
        assert!(
            !n.contains(&"RBV".to_string()),
            "MDEL must throttle RBV: {n:?}"
        );
    }

    /// Record that names DIFF in `force_posted_fields` (the motor's C
    /// `process_motor_info` unconditional `MARK(M_DIFF)`) while keeping
    /// every value constant — a settled axis parked at a fixed non-zero
    /// following error. VAL is a control: not force-listed, so it must
    /// fall back to change-detection.
    struct ForcePostRecord {
        diff: f64,
        val: f64,
    }

    impl Record for ForcePostRecord {
        fn record_type(&self) -> &'static str {
            "ai"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            // Values never change — the readback already matches; only the
            // unconditional MARK should keep DIFF flowing.
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "DIFF" => Some(EpicsValue::Double(self.diff)),
                "VAL" => Some(EpicsValue::Double(self.val)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(CaError::FieldNotFound(name.to_string()))
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        fn force_posted_fields(&self) -> &'static [&'static str] {
            &["DIFF"]
        }
    }

    /// C motorRecord parity: `process_motor_info` MARKs M_DIFF/M_RDIF every
    /// CALLBACK_DATA pass and `monitor()` posts them with `DBE_VAL_LOG`
    /// regardless of change, so a force-posted field re-posts on an
    /// otherwise-idle cycle while an unchanged non-force field does not.
    #[test]
    fn force_posted_field_reposts_unchanged_value_each_cycle() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "FP".to_string(),
            ForcePostRecord {
                diff: 2.5,
                val: 1.0,
            },
        );
        let _diff_rx = inst
            .add_subscriber(
                "DIFF",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("DIFF subscriber");
        let _val_rx = inst
            .add_subscriber(
                "VAL",
                2,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("VAL subscriber");
        let names = |snap: &ProcessSnapshot| {
            snap.changed_fields
                .iter()
                .map(|(n, _, _)| n.clone())
                .collect::<Vec<_>>()
        };

        // Cycle 1 (first publish): both DIFF and VAL post — last_posted is
        // empty so change-detection treats every subscribed field as new.
        let (snap1, _) = inst.process_local().unwrap();
        assert!(
            names(&snap1).contains(&"DIFF".to_string()),
            "DIFF posts on first publish: {:?}",
            names(&snap1)
        );

        // Cycle 2: nothing changed. VAL (not force-listed) must NOT re-post;
        // DIFF (force-listed) MUST re-post — the C unconditional MARK +
        // DBE_VAL_LOG. This is the divergence MOT-1 closes.
        let (snap2, _) = inst.process_local().unwrap();
        assert!(
            names(&snap2).contains(&"DIFF".to_string()),
            "force-posted DIFF must re-post when unchanged: {:?}",
            names(&snap2)
        );
        assert!(
            !names(&snap2).contains(&"VAL".to_string()),
            "an unchanged non-force field must not re-post: {:?}",
            names(&snap2)
        );
        // The forced re-post carries DBE_VALUE|DBE_LOG (no alarm bits this
        // cycle), matching C `monitor_mask | DBE_VAL_LOG` with monitor_mask=0.
        let diff_mask = snap2
            .changed_fields
            .iter()
            .find(|(n, _, _)| n == "DIFF")
            .map(|(_, _, m)| *m)
            .expect("DIFF post present");
        assert_eq!(
            diff_mask.bits(),
            (EventMask::VALUE | EventMask::LOG).bits(),
            "forced re-post mask is DBE_VAL_LOG"
        );
    }

    /// Record that names S1 in `log_swept_fields` (the scaler's idle
    /// `monitor()` DBE_LOG sweep) while keeping every value constant. S2
    /// is a control: subscribed but NOT swept, so an unchanged S2 must
    /// not re-post. Neither field is the primary `VAL`, so the default
    /// deadband field resolves to nothing and does not confound the test.
    struct LogSweepRecord {
        s1: i32,
        s2: i32,
    }

    impl Record for LogSweepRecord {
        fn record_type(&self) -> &'static str {
            "scaler"
        }
        fn process(&mut self) -> CaResult<crate::server::record::ProcessOutcome> {
            // Counts never change — only the unconditional idle LOG sweep
            // should keep S1 flowing to a DBE_LOG (archiver) subscriber.
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "S1" => Some(EpicsValue::Long(self.s1)),
                "S2" => Some(EpicsValue::Long(self.s2)),
                _ => None,
            }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
            match (name, value) {
                ("S1", EpicsValue::Long(v)) => {
                    self.s1 = v;
                    Ok(())
                }
                ("S2", EpicsValue::Long(v)) => {
                    self.s2 = v;
                    Ok(())
                }
                _ => Err(CaError::FieldNotFound(name.to_string())),
            }
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] {
            &[]
        }
        fn log_swept_fields(&self) -> &'static [&'static str] {
            &["S1"]
        }
    }

    /// C scalerRecord.c:770-787 `monitor()` sweeps each active channel
    /// with a literal `DBE_LOG` on every idle process: an UNCHANGED swept
    /// field re-posts with `DBE_LOG` ONLY, while a CHANGED swept field is
    /// delivered once by change-detection with `DBE_VALUE|DBE_LOG` (NOT
    /// double-posted by the sweep). A non-swept field never re-posts when
    /// unchanged. `add_subscriber` seeds `last_posted` with the current
    /// value (the initial value goes out via EVENT_ADD), so a freshly
    /// subscribed unchanged field already takes the sweep path on cycle 1.
    #[test]
    fn log_swept_field_reposts_unchanged_with_log_mask_only() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new("SW".to_string(), LogSweepRecord { s1: 7, s2: 9 });
        let _s1_rx = inst
            .add_subscriber(
                "S1",
                1,
                crate::types::DbFieldType::Long,
                EventMask::LOG.bits(),
            )
            .expect("S1 subscriber");
        let _s2_rx = inst
            .add_subscriber(
                "S2",
                2,
                crate::types::DbFieldType::Long,
                EventMask::VALUE.bits(),
            )
            .expect("S2 subscriber");
        let names = |snap: &ProcessSnapshot| {
            snap.changed_fields
                .iter()
                .map(|(n, _, _)| n.clone())
                .collect::<Vec<_>>()
        };
        let count_of = |snap: &ProcessSnapshot, f: &str| {
            snap.changed_fields
                .iter()
                .filter(|(n, _, _)| n == f)
                .count()
        };
        let mask_of = |snap: &ProcessSnapshot, f: &str| {
            snap.changed_fields
                .iter()
                .find(|(n, _, _)| n == f)
                .map(|(_, _, m)| *m)
        };

        // Cycle 1: nothing changed since subscribe. S1 (swept) re-posts
        // with DBE_LOG ONLY; S2 (not swept) must NOT re-post.
        let (snap1, _) = inst.process_local().unwrap();
        assert!(
            names(&snap1).contains(&"S1".to_string()),
            "log-swept S1 must re-post when unchanged: {:?}",
            names(&snap1)
        );
        assert!(
            !names(&snap1).contains(&"S2".to_string()),
            "unchanged non-swept S2 must not re-post: {:?}",
            names(&snap1)
        );
        assert_eq!(
            mask_of(&snap1, "S1").unwrap().bits(),
            EventMask::LOG.bits(),
            "idle sweep posts DBE_LOG only (no DBE_VALUE)"
        );

        // Cycle 2: S1's count changed. Change-detection delivers it ONCE
        // with DBE_VALUE|DBE_LOG; the sweep does NOT add a second post.
        inst.record.put_field("S1", EpicsValue::Long(8)).unwrap();
        let (snap2, _) = inst.process_local().unwrap();
        assert_eq!(
            count_of(&snap2, "S1"),
            1,
            "a changed swept field posts exactly once (no double-post): {:?}",
            snap2.changed_fields
        );
        assert_eq!(
            mask_of(&snap2, "S1").unwrap().bits(),
            (EventMask::VALUE | EventMask::LOG).bits(),
            "a changed swept field posts VALUE|LOG via change-detection"
        );

        // Cycle 3: unchanged again — back to the DBE_LOG-only sweep.
        let (snap3, _) = inst.process_local().unwrap();
        assert_eq!(
            mask_of(&snap3, "S1").unwrap().bits(),
            EventMask::LOG.bits(),
            "unchanged-again S1 returns to the DBE_LOG-only sweep"
        );
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
            self.egu = "kV".into();
            self.took_change = true;
            Ok(crate::server::record::ProcessOutcome::complete())
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name {
                "VAL" => Some(EpicsValue::Double(self.val)),
                "EGU" => Some(EpicsValue::String(self.egu.clone().into())),
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
                    self.egu = s.as_str_lossy().into_owned();
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

    // ── Regression: DBE_PROPERTY event delivery boundaries ──────────────

    /// motor `prop(YES)` fields (motorRecord.dbd 154/161/289/361/368)
    /// are property-class: a changed write must post DBE_PROPERTY
    /// (C dbAccess.c dbPut, `pfldDes->prop`). They feed the
    /// live-computed `field_metadata_override`, not the cache, but the
    /// posting gate is this same set.
    #[test]
    fn motor_prop_yes_fields_are_property_class() {
        for f in ["VBAS", "VMAX", "MRES", "DHLM", "DLLM"] {
            assert!(is_metadata_field(f), "{f} must be property-class");
        }
    }

    /// Boundary 1: metadata field written with a CHANGED value, subscriber
    /// mask includes PROPERTY → subscriber receives an event.
    /// Mirrors C dbAccess.c:1396-1397 `db_post_events(precord,NULL,DBE_PROPERTY)`.
    #[test]
    fn r47_property_event_delivered_on_changed_metadata() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");

        let prev = inst.record.get_field("EGU"); // "degC"
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref());

        assert!(
            rx.try_recv().is_ok(),
            "PROPERTY subscriber must receive event when metadata field changes"
        );
    }

    /// Boundary 2: same metadata field written with the SAME value → NO event.
    /// Matches C suppression at dbAccess.c:1379-1383 and the `prev != now` gate.
    #[test]
    fn r47_no_event_on_unchanged_metadata() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");

        let prev = inst.record.get_field("EGU"); // "degC"
        // Write the same value — no change
        let _ = inst.record.put_field("EGU", prev.clone().unwrap());
        inst.notify_field_written_if_changed("EGU", prev.as_ref());

        assert!(
            rx.try_recv().is_err(),
            "PROPERTY subscriber must NOT receive event when metadata value is unchanged"
        );
    }

    /// Boundary 3: VALUE-only subscriber (no PROPERTY bit) receives NO event
    /// from a metadata write, even when the field value changed.
    #[test]
    fn r47_value_only_subscriber_no_event_on_metadata_write() {
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added");

        let prev = inst.record.get_field("EGU"); // "degC"
        let _ = inst
            .record
            .put_field("EGU", EpicsValue::String("kPa".into()));
        inst.notify_field_written_if_changed("EGU", prev.as_ref());

        assert!(
            rx.try_recv().is_err(),
            "VALUE-only subscriber must NOT receive event from a metadata write"
        );
    }

    /// Boundary 4 (took_metadata_change path): PROPERTY subscriber receives
    /// event after process_local() when the record reports a metadata change.
    #[test]
    fn r47_process_local_property_event_on_took_metadata_change() {
        use crate::server::recgbl::EventMask;
        let mut inst = RecordInstance::new(
            "MUT2".to_string(),
            MutatingMetaRecord {
                val: 1.0,
                egu: "V".to_string(),
                took_change: false,
            },
        );
        let mut rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::PROPERTY.bits(),
            )
            .expect("subscriber added");

        // process() sets took_change = true and updates egu to "kV"
        let _ = inst.process_local();

        assert!(
            rx.try_recv().is_ok(),
            "PROPERTY subscriber must receive event after process_local reports took_metadata_change"
        );
    }
}

#[cfg(test)]
mod aftc_filter_tests {
    //! Tests for the shared AFTC alarm-range filter
    //! (`records::alarm_filter::aftc_filter`) as driven by
    //! `evaluate_analog_alarm`. Pure-function tests: no record instance
    //! needed — the filter is a stateless transform of (raw_alarm, aftc,
    //! afvl_in, t_last, t_now). Algorithm provenance: 2009 EPICS
    //! Codeathon (epics-base `824d37811`), C `aiRecord.c:355-401`.

    use crate::server::records::alarm_filter::aftc_filter;
    use std::time::{Duration, SystemTime};

    fn at(secs: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs)
    }

    #[test]
    fn disabled_when_aftc_le_zero() {
        // aftc=0 means filter disabled — pass-through.
        let (out, afvl) = aftc_filter(2, 0.0, 0.0, at(0.0), at(1.0));
        assert_eq!(out, 2);
        assert_eq!(afvl, 0.0);
    }

    #[test]
    fn initial_sample_seeds_state_unchanged_alarm() {
        // afvl=0 means first sample after enable — alarm passes through
        // and accumulator seeds with the raw severity.
        let (out, afvl) = aftc_filter(2, 3.0, 0.0, at(0.0), at(0.5));
        assert_eq!(out, 2);
        assert_eq!(afvl, 2.0);
    }

    #[test]
    fn raises_alarm_only_after_full_time_constant() {
        // Single-step heuristic: with `aftc = 3s` and `dt = 0.1s`, alpha
        // ≈ 0.967, so a one-shot raw_alarm=2 against afvl=0.0 should not
        // produce alarm=2 yet — the filter must hold off until the
        // accumulator crosses the threshold.
        // Seed with afvl=0.01 (tiny prior, simulating "almost no alarm
        // yet"); the filter must keep alarm at 0 after one short tick.
        let (out, afvl) = aftc_filter(2, 3.0, 0.01, at(0.0), at(0.1));
        assert_eq!(out, 0, "filter should suppress alarm rise on a 0.1s tick");
        assert!(afvl > 0.0 && afvl < 2.0);
    }

    #[test]
    fn dt_zero_is_no_op() {
        // Two evaluations at the same instant produce no filter advance.
        let (out, afvl) = aftc_filter(2, 3.0, 1.5, at(0.0), at(0.0));
        assert_eq!(out, 1); // floor(|1.5|) = 1
        assert_eq!(afvl, 1.5);
    }

    #[test]
    fn long_steady_state_converges_to_alarm() {
        // After many steps with raw_alarm=2 and dt much smaller than aftc,
        // the accumulator must converge towards 2.
        let aftc = 1.0;
        let mut afvl = 0.0;
        let mut last = at(0.0);
        let mut alarm = 0;
        for i in 1..=100 {
            let now = at(i as f64 * 0.05);
            let (out, new_afvl) = aftc_filter(2, aftc, afvl, last, now);
            alarm = out;
            afvl = new_afvl;
            last = now;
        }
        assert_eq!(
            alarm, 2,
            "after 5 s of steady raw=2 with aftc=1 s, output must reach 2"
        );
        assert!(afvl.abs() >= 1.99 && afvl.abs() <= 2.0);
    }
}

#[cfg(test)]
mod check_deadband_tests {
    use super::check_deadband;

    /// Sentinel: `oldval=NaN` means "no prior posting", always fire.
    #[test]
    fn nan_old_value_fires() {
        assert!(check_deadband(0.0, f64::NAN, 1.0));
        assert!(check_deadband(f64::NAN, f64::NAN, 1.0));
    }

    /// C path: `delta > deadband` with both finite. delta within deadband
    /// must NOT fire.
    #[test]
    fn within_finite_deadband_does_not_fire() {
        assert!(!check_deadband(10.0, 10.5, 1.0));
        assert!(!check_deadband(10.0, 9.5, 1.0));
        // Boundary: `delta == deadband` is NOT strictly greater.
        assert!(!check_deadband(10.0, 11.0, 1.0));
    }

    /// `delta > deadband` with both finite, beyond → fire.
    #[test]
    fn beyond_finite_deadband_fires() {
        assert!(check_deadband(10.0, 12.0, 1.0));
    }

    /// Negative deadband acts as "always fire" (C `delta > deadband` is
    /// trivially true for any non-negative delta).
    #[test]
    fn negative_deadband_fires() {
        assert!(check_deadband(10.0, 10.0, -1.0));
    }

    /// C parity bug fix (recGbl.c:355-358): exactly one of {newval,
    /// oldval} is NaN — fire. Rust port previously short-circuited only
    /// on `oldval=NaN`; `newval=NaN` with `oldval=finite` produced
    /// `(NaN - finite).abs() = NaN`, `NaN > deadband = false` →
    /// silently dropped the NaN transition. End effect: a record that
    /// went UDF (e.g. divide-by-zero in calc) never posted the change
    /// to monitors, leaving every camonitor seeing the last valid value.
    #[test]
    fn newval_nan_with_finite_oldval_fires() {
        assert!(check_deadband(f64::NAN, 10.0, 1.0));
    }

    /// C path case 2 (recGbl.c:355): exactly one infinite, the other
    /// finite — fire.
    #[test]
    fn one_finite_one_infinite_fires() {
        assert!(check_deadband(f64::INFINITY, 10.0, 1.0));
        assert!(check_deadband(10.0, f64::INFINITY, 1.0));
        assert!(check_deadband(f64::NEG_INFINITY, 10.0, 1.0));
    }

    /// C path case 3 (recGbl.c:360-362): both infinite with opposite
    /// signs — fire.
    #[test]
    fn opposite_signed_infinities_fire() {
        assert!(check_deadband(f64::INFINITY, f64::NEG_INFINITY, 1.0));
        assert!(check_deadband(f64::NEG_INFINITY, f64::INFINITY, 1.0));
    }

    /// Same-signed infinity → no fire (C path leaves `delta = 0`,
    /// `0 > deadband` is false for any non-negative deadband).
    #[test]
    fn same_signed_infinity_does_not_fire() {
        assert!(!check_deadband(f64::INFINITY, f64::INFINITY, 1.0));
        assert!(!check_deadband(f64::NEG_INFINITY, f64::NEG_INFINITY, 1.0));
    }
}

#[cfg(test)]
mod common_field_dbload_tests {
    use super::*;
    use crate::server::records::ai::AiRecord;

    /// The db loader feeds every common field to `put_common_field` as an
    /// `EpicsValue::String`. Each numeric/menu common field directive must
    /// take effect at load — both the integer form (`field(PHAS, "1")`) and
    /// the menu-label form (`field(PRIO, "HIGH")`, `field(DISS, "MAJOR")`) —
    /// rather than being silently dropped because the arm matched only its
    /// typed variant. One assertion per affected common-field arm.
    #[test]
    fn db_loaded_string_common_fields_take_effect() {
        let mut inst = RecordInstance::new("REC".to_string(), AiRecord::default());
        let put = |inst: &mut RecordInstance, f: &str, v: &str| {
            inst.put_common_field_db_load(f, EpicsValue::String(v.into()))
                .unwrap_or_else(|e| panic!("put_common_field_db_load({f}, {v:?}) failed: {e}"));
        };

        // Integer-valued directives.
        put(&mut inst, "PHAS", "1");
        assert_eq!(inst.common.phas, 1, "field(PHAS, \"1\")");
        put(&mut inst, "TSE", "-2");
        assert_eq!(inst.common.tse, -2, "field(TSE, \"-2\")");
        put(&mut inst, "DISV", "1");
        assert_eq!(inst.common.disv, 1, "field(DISV, \"1\")");
        put(&mut inst, "DISA", "1");
        assert_eq!(inst.common.disa, 1, "field(DISA, \"1\")");
        put(&mut inst, "LCNT", "3");
        assert_eq!(inst.common.lcnt, 3, "field(LCNT, \"3\")");
        put(&mut inst, "DISP", "1");
        assert!(inst.common.disp, "field(DISP, \"1\")");
        put(&mut inst, "UDF", "0");
        assert!(!inst.common.udf, "field(UDF, \"0\")");

        // Menu-label directives (resolved via the one menu converter).
        put(&mut inst, "PRIO", "HIGH");
        assert_eq!(inst.common.prio, 2, "field(PRIO, \"HIGH\")");
        put(&mut inst, "DISS", "MAJOR");
        assert_eq!(
            inst.common.diss,
            AlarmSeverity::Major,
            "field(DISS, \"MAJOR\")"
        );
        put(&mut inst, "UDFS", "NO_ALARM");
        assert_eq!(
            inst.common.udfs,
            AlarmSeverity::NoAlarm,
            "field(UDFS, \"NO_ALARM\")"
        );
        put(&mut inst, "ACKT", "NO");
        assert!(!inst.common.ackt, "field(ACKT, \"NO\")");

        // Numeric form of a menu field still works (field(PRIO, "0")).
        put(&mut inst, "PRIO", "0");
        assert_eq!(inst.common.prio, 0, "field(PRIO, \"0\")");

        // A String-typed common field is untouched by the coercion.
        put(&mut inst, "DESC", "a description");
        assert_eq!(inst.common.desc.as_str_lossy().as_ref(), "a description");
    }
}
