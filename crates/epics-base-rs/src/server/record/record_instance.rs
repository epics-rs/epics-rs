use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::runtime::sync::mpsc;

use crate::error::{CaError, CaResult};
use crate::server::pv::{MonitorEvent, Subscriber};
use crate::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::alarm::{AlarmSeverity, AnalogAlarmConfig};
use super::common_fields::CommonFields;
use super::link::{ParsedLink, parse_link_v2, parse_output_link_v2};
use super::record_trait::{
    CommonFieldPutResult, ProcessSnapshot, Record, RecordProcessResult, SubroutineFn,
};
use super::scan::ScanType;

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
/// Currently uncovered (because they are not yet populated by any
/// `populate_*` function): `DESC` (would map to `display.description`
/// — populate hook missing), `Q:form` info tag (would map to
/// `display.form`). Add to this set if/when those are wired up.
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
            // C parity: every record type whose dbd carries
            // HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV gets an analog-alarm
            // config slot. Previously calc / calcout were missing —
            // their put_field for those fields silently no-op'd
            // because `self.common.analog_alarm` was None at the
            // mutation site. Confirmed via
            // calcRecord.dbd.pod:716-744 (HIHI..LLSV) and
            // calcoutRecord.dbd.pod:1103+ (same).
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" | "calc" | "calcout" => {
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
            notify: None,
            last_posted: HashMap::new(),
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

        // Common-field enum mapping (e.g. .SCAN choices) is field-specific
        // and not part of the per-record cache.
        self.populate_common_enum_info(field, &mut snap);

        // DBF_MENU field (a shared menu such as `OMSL`/`HHSV`/`SIMM`/... or
        // a record-specific menu such as `sel.SELM`): carry the menu index
        // as DBR_ENUM and attach its `menu()` choice labels. See
        // `attach_menu_enum`. This overrides any record VAL enum table
        // copied from the metadata cache above, because a menu field
        // carries its own menu's choices, not the record's VAL state
        // strings.
        self.attach_menu_enum(field, &mut snap);

        // apply `info(Q:time:tag, "nsec:lsb:N")` — pvxs
        // typeutils.cpp:79 splits the low N bits of the timestamp's
        // nanoseconds into `timeStamp.userTag` and clears those bits
        // from `nanoseconds`. Standard pvxs convention is `nsec:lsb:N`
        // with N in 1..=30; values outside that range are ignored to
        // match pvxs's bounds clamp. The split is applied to both
        // `snap.timestamp` and `snap.user_tag` so downstream encoders
        // (NTScalar `timeStamp`, QSRV groups via `+nsecmask`) all see
        // the same shape.
        if let Some(n) = self.parse_qtime_tag_nsec_lsb() {
            crate::server::snapshot::apply_nsec_lsb_split(&mut snap, n);
        }

        Some(snap)
    }

    /// Parse `info(Q:time:tag, "nsec:lsb:N")` and return `N`.
    /// Returns `None` when the info tag is absent or malformed (pvxs
    /// silently ignores bad values; we match that by returning None
    /// so the timestamp is emitted unchanged).
    fn parse_qtime_tag_nsec_lsb(&self) -> Option<u8> {
        let raw = self.get_info("Q:time:tag")?;
        // Accept `nsec:lsb:N` with arbitrary whitespace and case.
        let mut parts = raw.split(':');
        let key = parts.next()?.trim();
        let suffix = parts.next()?.trim();
        let n = parts.next()?.trim();
        if !key.eq_ignore_ascii_case("nsec") || !suffix.eq_ignore_ascii_case("lsb") {
            return None;
        }
        let n: u32 = n.parse().ok()?;
        // pvxs clamps to [1, 30]; values outside leave the timestamp
        // alone (the userTag is meaningful only with a non-trivial
        // mask, and >30 would consume all nanoseconds).
        if (1..=30).contains(&n) {
            Some(n as u8)
        } else {
            None
        }
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
            "ACKT" => Some(EpicsValue::Char(if self.common.ackt { 1 } else { 0 })),
            "UDF" => Some(EpicsValue::Char(if self.common.udf { 1 } else { 0 })),
            "UDFS" => Some(EpicsValue::Short(self.common.udfs as i16)),
            "SCAN" => Some(EpicsValue::Enum(self.common.scan as u16)),
            "SSCN" => Some(EpicsValue::Enum(self.common.sscn as u16)),
            "PINI" => Some(EpicsValue::Char(if self.common.pini { 1 } else { 0 })),
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
            "SCAN" => {
                let old_scan = self.common.scan;
                let new_scan = match &value {
                    EpicsValue::Short(v) => ScanType::from_u16(*v as u16),
                    EpicsValue::Enum(v) => ScanType::from_u16(*v),
                    EpicsValue::String(s) => ScanType::from_str(s.as_str_lossy().as_ref())?,
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
                    EpicsValue::String(s) => ScanType::from_str(s.as_str_lossy().as_ref())?,
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
                    self.common.flnk = s.as_str_lossy().into_owned();
                    self.parsed_flnk = parse_link_v2(&self.common.flnk);
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
                    let _ = self.record.special(&name, true);
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
                // C UTAG is DBF_UINT64 — accept any integer-shaped
                // value and store the unsigned 64-bit tag.
                match value {
                    EpicsValue::UInt64(v) => self.common.utag = v,
                    EpicsValue::Int64(v) => self.common.utag = v as u64,
                    EpicsValue::Long(v) => self.common.utag = v as u64,
                    EpicsValue::Short(v) => self.common.utag = v as u64,
                    EpicsValue::Enum(v) => self.common.utag = v as u64,
                    EpicsValue::Char(v) => self.common.utag = v as u64,
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
                if self.record.record_type() == "swait" {
                    if let EpicsValue::String(s) = value {
                        self.common.out = s.as_str_lossy().into_owned();
                        // Bare OUT link is NPP — see the "OUT" arm.
                        self.parsed_out = parse_output_link_v2(&self.common.out);
                    }
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
            "NAME" => Some(EpicsValue::String(self.name.clone().into())),
            "RTYP" => Some(EpicsValue::String(
                self.record.record_type().to_string().into(),
            )),
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
                recgbl::rec_gbl_set_sevr_msg(
                    &mut self.common,
                    alarm_status::CALC_ALARM,
                    crate::server::record::AlarmSeverity::Invalid,
                    "CALC expression evaluation failed",
                );
            }
        }

        match rtype {
            "ai" | "ao" | "longin" | "longout" | "int64in" | "int64out" | "calc" | "calcout" => {
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
            self.common.lcnt = self.common.lcnt.saturating_add(1);
            if self.common.lcnt >= LCNT_ALARM_THRESHOLD {
                self.common.sevr = AlarmSeverity::Invalid;
                self.common.stat = recgbl::alarm_status::SCAN_ALARM;
                // Post the SCAN_ALARM transition so subscribers see it.
                // The synchronous link path (`process_record_with_links_inner`,
                // mirroring C `dbAccess.c:559-583`) posts VAL with
                // DBE_VALUE|DBE_LOG|DBE_ALARM when the LCNT guard raises
                // SCAN_ALARM/INVALID. The pre-fix branch here flipped
                // sevr/stat but returned an empty snapshot, so a
                // `process_local`-driven reentrant record went INVALID
                // silently — no monitor ever reached the operator.
                let mut changed_fields = Vec::new();
                if let Some(val) = self.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
                changed_fields.push((
                    "SEVR".to_string(),
                    EpicsValue::Short(self.common.sevr as i16),
                ));
                changed_fields.push((
                    "STAT".to_string(),
                    EpicsValue::Short(self.common.stat as i16),
                ));
                return Ok((
                    ProcessSnapshot {
                        changed_fields,
                        event_mask: EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
                    },
                    Vec::new(),
                ));
            }
            return Ok((
                ProcessSnapshot {
                    changed_fields: Vec::new(),
                    event_mask: EventMask::NONE,
                },
                Vec::new(),
            ));
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
                    event_mask: EventMask::NONE,
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
            return Ok((
                ProcessSnapshot {
                    changed_fields,
                    event_mask,
                },
                Vec::new(),
            ));
        }

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
        // Carry DBE_ALARM on the record-wide event mask whenever the
        // severity/status OR the alarm message moved — aligning with
        // the `processing.rs` link path (`event_mask |= ALARM` on
        // `alarm_changed || amsg_changed`). The pre-fix branch checked
        // only `alarm_changed`, so a put that changed AMSG without
        // moving SEVR/STAT posted VAL without the DBE_ALARM bit.
        if alarm_result.alarm_changed || alarm_result.amsg_changed {
            event_mask |= EventMask::ALARM;
        }

        // Build snapshot
        let mut changed_fields = Vec::new();
        // Same deadband-field routing as the `processing.rs` paths: the
        // deadband triggers deliver the field the deadband tracks; a
        // non-primary deadband field (motor RBV — C motor `monitor()`,
        // motorRecord.cc:3468-3507) leaves VAL to the generic
        // change-detection loop below.
        let deadband_field = self.record.monitor_deadband_field();
        if deadband_field == "VAL" {
            if include_val {
                if let Some(val) = self.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
            }
        } else if include_val || include_archive {
            if let Some(val) = self.resolve_field(deadband_field) {
                changed_fields.push((deadband_field.to_string(), val));
            }
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

        // Post UDF on the snapshot whenever any monitor event fires this
        // cycle — mirrors the two `processing.rs` UDF pushes
        // (`database/processing.rs:1327` and `:1948`,
        // `if !event_mask.is_empty()`). C `recGblResetAlarms` /
        // `recGblCheckUDF` (recGbl.c) keep UDF current every process
        // cycle, and `db_post_events` delivers `.UDF` alongside the
        // record-wide post. `process_local` is the foreign-process path
        // (`db.process_record`, e.g. QSRV group-process members); without
        // this push a UDF change here is never delivered to `.UDF`
        // subscribers — the `sub_updates` loop below deliberately excludes
        // UDF to avoid a double-post, so the push must be here.
        if !event_mask.is_empty() {
            changed_fields.push((
                "UDF".to_string(),
                EpicsValue::Char(if self.common.udf { 1 } else { 0 }),
            ));
        }

        // Add subscribed fields that actually changed since last notification.
        // Exclude {deadband-field}/SEVR/STAT/AMSG/UDF — all five are already
        // emitted by this path (the deadband field, default VAL, in
        // `changed_fields`, SEVR/STAT/AMSG via `alarm_posts`, UDF via the
        // explicit UDF push above). Mirrors the two `processing.rs`
        // snapshot gates, which exclude the same five; excluding only the
        // first three would double-post AMSG and UDF.
        let mut sub_updates: Vec<(String, EpicsValue)> = Vec::new();
        for (field, subs) in &self.subscribers {
            if !subs.is_empty()
                && field != deadband_field
                && field != "SEVR"
                && field != "STAT"
                && field != "AMSG"
                && field != "UDF"
            {
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

        Ok((
            ProcessSnapshot {
                changed_fields,
                event_mask,
            },
            alarm_posts,
        ))
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
    pub fn check_deadband_ext(&mut self) -> (bool, bool) {
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
    /// Uses event_mask to filter: only notify subscribers whose mask intersects.
    pub fn notify_from_snapshot(&self, snapshot: &ProcessSnapshot) {
        use crate::server::database::filters::FilteredMonitorEvent;
        use crate::server::recgbl::EventMask;
        let posting_mask = snapshot.event_mask;

        for (field, value) in &snapshot.changed_fields {
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
                        };
                        // Server-side filter chain (3.15.7). Empty chain
                        // is identity, so no behaviour change for the
                        // common no-filter case.
                        let filtered = if sub.filters.is_empty() {
                            Some(event)
                        } else {
                            sub.filters
                                .apply(FilteredMonitorEvent::new(event, posting_mask))
                                .map(|fe| fe.event)
                        };
                        let Some(event) = filtered else {
                            continue;
                        };
                        if sub.tx.try_send(event.clone()).is_err() {
                            // route the coalesce overwrite through
                            // the single owner so a record-field monitor
                            // value lost to a slow consumer is counted in
                            // `dropped_monitor_events()`, exactly like a
                            // `ProcessVariable` overflow.
                            sub.coalesce_overflow(event);
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
                                .apply(FilteredMonitorEvent::new(event, mask))
                                .map(|fe| fe.event)
                        };
                        let Some(event) = filtered else {
                            continue;
                        };
                        if sub.tx.try_send(event.clone()).is_err() {
                            // same single coalesce-overflow owner
                            // as the snapshot path — record-field loss to
                            // a slow consumer must be counted, not silently
                            // overwritten.
                            sub.coalesce_overflow(event);
                        }
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
    ) -> Option<mpsc::Receiver<MonitorEvent>> {
        let cap = crate::server::pv::max_subscribers_per_pv();
        let field_str = field.to_string();
        let bucket = self.subscribers.entry(field_str.clone()).or_default();
        // Reap dead Senders before
        // counting against the cap. A record field whose value
        // never changes (e.g. a quasi-static catalog field) never
        // triggers `notify_field_with_origin`'s retain-filter, so
        // a long-lived subscribe-disconnect storm could pin the
        // bucket at `cap` worth of closed Senders and lock out
        // genuine new subscribers.
        bucket.retain(|s| !s.tx.is_closed());
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
        Some(rx)
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
    /// suppresses every subsequent post to this subscriber AND drops any
    /// pending coalesced overflow, so a resumed monitor restarts from the
    /// source-side edge rather than replaying a value captured while it
    /// was paused. No-op if no subscriber has this `sid`. The caller holds
    /// the record write lock, so this is exclusive with the read-locked
    /// post paths that consult `Subscriber::active`.
    pub fn set_subscriber_active(&mut self, sid: u32, active: bool) {
        for subs in self.subscribers.values_mut() {
            for sub in subs.iter_mut() {
                if sub.sid == sid {
                    sub.active = active;
                    if !active && let Ok(mut slot) = sub.coalesced.lock() {
                        *slot = None;
                    }
                }
            }
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

    /// a record-field monitor whose bounded queue is full and
    /// whose coalesce slot already holds an unobserved value must count
    /// the displaced value in the shared `dropped_monitor_events()`
    /// counter — the same accounting a `ProcessVariable` overflow uses.
    /// Before the fix the record-field path overwrote the slot without
    /// counting, hiding slow-consumer loss on the path most CA/PVA
    /// database monitors use. The counter is process-global, so the
    /// assertion is a strict monotonic increase (robust under parallel
    /// tests); the revert-verify runs this test in isolation.
    #[test]
    fn bfr10_record_field_overflow_counts_dropped_event() {
        use crate::server::pv::dropped_monitor_events;
        use crate::server::recgbl::EventMask;
        let mut inst = ai_instance();
        // Keep `rx` alive (do NOT drain) so the bounded 64-deep queue
        // fills, then the coalesce slot, before overflow replacement.
        let _rx = inst
            .add_subscriber(
                "VAL",
                1,
                crate::types::DbFieldType::Double,
                EventMask::VALUE.bits(),
            )
            .expect("subscriber added");
        let before = dropped_monitor_events();
        // 64 sends fill the queue; the 65th fills the (empty) coalesce
        // slot; each send after that overwrites an UNOBSERVED slot value
        // and must be counted as a dropped monitor event.
        for _ in 0..70 {
            inst.notify_field_with_origin("VAL", EventMask::VALUE, 0);
        }
        let after = dropped_monitor_events();
        assert!(
            after > before,
            "record-field overflow onto an occupied coalesce slot must \
             record a dropped monitor event (before={before}, after={after})"
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
                .map(|(n, _)| n.clone())
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
