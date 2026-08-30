pub(crate) mod breakpoint;
pub mod db_access;
mod field_io;
pub mod filters;
mod link_put_queue;
mod link_set;
mod links;
mod processing;
mod record_lock;
pub(crate) mod scan_index;
mod snapshot;

pub use field_io::ProcessMode;
pub use link_set::{
    DynLinkSet, LinkBacking, LinkDbfType, LinkDiagnostics, LinkMetadata, LinkPutOp, LinkSet,
    LinkSetRegistry, PutAdmission, RemoteAlarm,
};
pub use processing::{AsyncDbHandle, AsyncToken};
pub use record_lock::{LockSetInfo, LockSetReport, ManyRecordWriteGuard, RecordWriteGuard};

use crate::error::{CaError, CaResult};
use arc_swap::{ArcSwap, ArcSwapOption};
use snapshot::SnapshotCell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::server::pv::ProcessVariable;
use crate::server::record::{Record, RecordInstance};
use crate::types::EpicsValue;

/// What a `.db` definition carries into the creation sink alongside the record
/// itself: the `dbCommon` fields `db_loader::apply_fields` could not route to
/// the record's own `field_list`, and the record's `info(...)` tags.
///
/// It exists so that [`PvDatabase::add_loaded_record`] receives a record's
/// COMPLETE loaded state in one call. A caller cannot add the record and then
/// apply its `.db` fields, because the sink runs C's `iocInit` passes — whose
/// result depends on those fields — before the record is reachable at all.
#[derive(Default, Debug, Clone)]
pub struct RecordLoad {
    /// `dbCommon` fields, in `.db` file order (a later `field(UDF,…)` wins).
    pub common_fields: Vec<(String, EpicsValue)>,
    /// `info(key, "value")` tags.
    pub info_tags: Vec<(String, String)>,
}

impl RecordLoad {
    /// The common fields alone — the shape every `.db` loader path produces
    /// from [`crate::server::db_loader::apply_fields`].
    pub fn from_common_fields(common_fields: Vec<(String, EpicsValue)>) -> Self {
        Self {
            common_fields,
            info_tags: Vec::new(),
        }
    }
}

/// Parse a PV name into (base_name, field_name).
/// "TEMP.EGU" → ("TEMP", "EGU")
/// "TEMP"     → ("TEMP", "VAL")
///
/// The split is on the LAST `.`, where C `dbFindRecordPart`
/// (`dbChannel.c:180-200`) takes the FIRST one via `strchr`. That difference
/// is unobservable, and it is the record-name grammar that makes it so: C
/// `dbRecordNameValidate` (`dbLexRoutines.c:1085-1119`) aborts the load with
/// `yyerrorAbort` on a `.` anywhere in a record or alias name, and the port
/// refuses the same names, so the base half can never itself contain a dot
/// and the two splits can only differ on a name that no IOC can hold.
///
/// Measured, not reasoned: a `.db` naming records `OTHER:PV` and `A.B` is
/// refused at load by both, so the `A.B.C` case cannot be built at all. The
/// one shape that does load — `OTHER:PV` present, a link reading
/// `OTHER:PV.1:X` — hangs C's `iocInit` (reproduced three times on softIoc
/// R7.0.10-146, always at "Starting iocInit"), while the port loads and
/// alarms; that is an upstream defect the port has no path to.
///
/// This is the channel-name half of the rule. Link text is split by
/// [`crate::server::record::DbLink`], which follows C exactly and cuts at the
/// first `.` before requiring the remainder to be a field identifier.
pub fn parse_pv_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((base, field)) => (base, field),
        None => (name, "VAL"),
    }
}

/// C `dbIsValueField` (`dbAccess.c:463-469`): is this field the record
/// type's *value* field?
///
/// A record type's value field is the one the DBD names `VAL` — the DBD
/// parser records exactly that field's index as `indvalFlddes`
/// (`dbLexRoutines.c:777-780`), which is what `dbIsValueField` compares
/// against. Metadata that C/pvxs apply "to VAL only" (e.g. QSRV's
/// `Q:form` → `display.form.index`, `iocsource.cpp:53`) key on this
/// predicate, so it lives beside [`parse_pv_name`], whose `"REC"` → `VAL`
/// default is the other half of the same rule.
pub fn is_value_field(field: &str) -> bool {
    field.eq_ignore_ascii_case("VAL")
}

pub(crate) use tsel_stamp::TselStamp;

/// C `recGblGetTimeStampSimm` (`recGbl.c:310-343`), whole and inseparable.
///
/// C's one function is two steps: resolve `TSEL` (`:314-322`), then turn `TSE`
/// into `TIME` (`:323-343`). The port cannot do them in one call — the link
/// read needs the database and must not hold the record's own data lock, while
/// the store needs the lock — so it splits them across
/// [`PvDatabase::read_tsel`] and [`TselStamp::stamp`]. The split is what this
/// module exists to bound: `apply_timestamp` is private to it, so the ONLY way
/// to reach the `TSE`→`TIME` half is to hold a [`TselStamp`], and the only way
/// to hold one is to have read `TSEL` — at the stamp point, which is where C
/// reads it. Resolving `TSEL` once at the head of the cycle instead handed a
/// `.TIME` TSEL the source's *pre-cycle* stamp even when the record's own
/// `INPn PP` had just reprocessed that source (`calcRecord.c:120-127` runs
/// `fetch_values` first), and let the device-time override at the stamp point
/// win over a TSEL C gives priority to.
mod tsel_stamp {
    use std::time::SystemTime;

    use crate::server::record::CommonFields;

    /// The TSEL half of C `recGblGetTimeStampSimm`, read but not yet stored.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(crate) enum TselStamp {
        /// No `TSEL`, a constant one — `if (!dbLinkIsConstant(&prec->tsel))`
        /// (`recGbl.c:315`) skips the whole block — or a read that delivered
        /// nothing. `TSE` keeps its own value.
        None,
        /// `DBLINK_FLAG_TSELisTIME` (`recGbl.c:316-321`): the source's
        /// time+utag, copied over the record's own. C `return`s here, so this
        /// arm also states that no `TSE` load and no event lookup follows —
        /// and that `TSE` itself is not touched. There is no assignment to
        /// `prec->tse` anywhere in epics-base; the flag that suppresses the
        /// lookup lives in `plink->flags`, not in a field clients can read.
        Time(SystemTime, u64),
        /// Every other `TSEL` (`recGbl.c:322`): `dbGetLink(&prec->tsel,
        /// DBR_SHORT, &prec->tse, 0, 0)`.
        Tse(i16),
    }

    impl TselStamp {
        /// C `recGblGetTimeStampSimm`'s body: store what `TSEL` yielded, then
        /// resolve `TSE`→`TIME`. `is_soft` indicates a Soft Channel device
        /// type.
        pub(crate) fn stamp(self, name: &str, common: &mut CommonFields, is_soft: bool) {
            match self {
                TselStamp::Time(time, utag) => {
                    common.time = time;
                    common.utag = utag;
                    // C's `return` (`recGbl.c:321`), which is the whole reason
                    // the event lookup does not run. Writing `TSE = -2` to get
                    // the same effect made the record report a value its `.db`
                    // never declared, and overwrote one that did.
                    return;
                }
                TselStamp::None => {}
                TselStamp::Tse(tse) => common.tse = tse,
            }
            apply_timestamp(name, common, is_soft);
        }
    }

    /// Apply timestamp to a record based on its TSE field.
    /// `is_soft` indicates a Soft Channel device type.
    ///
    /// The second half of C `recGblGetTimeStampSimm` (recGbl.c:324-342). The
    /// TSE constants are defined in `epicsTime.h:102-104`:
    ///
    ///   - `epicsTimeEventCurrentTime = 0` → wall-clock now
    ///   - `epicsTimeEventBestTime    = -1` → generalTime BestTime providers
    ///   - `epicsTimeEventDeviceTime  = -2` → device support already set time
    ///   - `1..` → event-number providers
    ///
    /// Every non-`-2` case goes through one C call, `epicsTimeGetEvent(tse)`,
    /// which delegates to `epicsTimeGetCurrent` for `tse==0` and to
    /// `generalTimeGetEventPriority` otherwise. Only `-2` (device time)
    /// is left untouched because the device support has already written
    /// the timestamp before `recGblGetTimeStamp` is called.
    ///
    /// A TSE C rejects — anything below `epicsTimeEventBestTime`, or any event
    /// number with no provider to answer it — is not a stamp: C writes nothing
    /// into `precord->time` and errlogs, so a misconfigured record holds its
    /// stale stamp rather than timestamping as if healthy.
    fn apply_timestamp(name: &str, common: &mut CommonFields, _is_soft: bool) {
        // Single owner of TSE -> TIME resolution; device support that must
        // format the record's resolved time during `read()` routes through the
        // same helper so the two never drift (see `recgbl::get_time_stamp`).
        // For TSE=-2 the helper returns `common.time` unchanged, preserving the
        // device-time "leave it alone" semantics.
        match crate::server::recgbl::get_time_stamp(common.tse, common.time) {
            Some(t) => common.time = t,
            None => crate::runtime::log::errlog_printf(&format!(
                "recGblGetTimeStampSimm: epicsTimeGetEvent failed, {name}.TSE = {}\n",
                common.tse
            )),
        }
    }
}

/// Unified entry in the PV database.
pub enum PvEntry {
    Simple(Arc<ProcessVariable>),
    Record(Arc<parking_lot::RwLock<RecordInstance>>),
}

/// Callback for resolving external PV names (CA/PVA links).
/// Returns the *cached* value of the external PV, or `None` if unavailable.
///
/// **Sync**, for the same reason [`LinkSet::get_cached_value`] is: this runs
/// on the record-processing thread with the record's L1 gate held, and C's
/// `dbCaGetLink` likewise only reads `pca->pgetNative` under `pca->lock` —
/// it never waits for the wire (`dbCa.c:419-506`). A resolver that cannot
/// answer from cache must stage the open on its own executor and return
/// `None` (C's `!pca->isConnected` arm, `dbCa.c:430-435`).
pub type ExternalPvResolver = Arc<dyn Fn(&str) -> Option<EpicsValue> + Send + Sync>;

/// Async hook invoked by [`PvDatabase::has_name`] when a name is not yet
/// in the database. Used by the CA gateway and similar proxy components
/// to lazily populate PVs on first search.
///
/// The resolver should:
/// 1. Determine whether the name should be served (e.g., check ACL)
/// 2. Take whatever action is needed to make `has_name` return true on
///    a subsequent call (e.g., subscribe to an upstream IOC and call
///    `add_pv` with a placeholder value)
/// 3. Return `true` if the name is now resolvable, `false` otherwise
///
/// Returning `true` causes `has_name` to re-check the database. The
/// resolver may take some time (TCP search, upstream connect handshake);
/// the caller (UDP search responder, TCP CREATE_CHANNEL handler) will
/// `.await` it.
/// The second argument is the downstream client's socket address when
/// the lookup originates from a CA/PVA search or channel-create on
/// behalf of an identified peer (`None` for host-less internal lookups:
/// preload, iocsh, link processing). the CA gateway needs
/// this to evaluate `.pvlist` `DENY FROM host` rules at search time, the
/// way C ca-gateway's `pvExistTest` passes the client host to
/// `gateAs::findEntry`.
pub type SearchResolver = Arc<
    dyn Fn(
            String,
            Option<std::net::SocketAddr>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Per-request admission gate for an **already-registered** simple PV.
///
/// A plain IOC's simple PVs are authoritative: once registered they
/// exist unconditionally, so no gate is installed and the cached-PV
/// short-circuit in [`PvDatabase::find_entry_from`] /
/// [`PvDatabase::has_name_from`] is unchanged. A CA gateway is
/// different — its shadow PVs are projections of an upstream that can be
/// host-denied for a given requester or disconnected — so it installs a
/// gate that the lookup path consults *before* returning a cached simple
/// PV. Returning `false` makes the database answer "does not exist" for
/// that requester, exactly as C ca-gateway's `pvExistTest` returns
/// `pverDoesNotExistHere` for a host-denied or disconnected PV
/// (`gateServer.cc:1516-1637`) — without removing the PV object, so its
/// cached value stays available for diagnostics and re-admission.
///
/// The first argument is the filter-suffix-stripped record path (the
/// same key the simple-PV map and the gateway cache use); the second is
/// the requesting peer (`None` for host-less internal lookups). The gate
/// governs **only** simple PVs — records and aliases are never
/// gateway-managed and bypass it.
pub type ExistenceGate = Arc<
    dyn Fn(
            String,
            Option<std::net::SocketAddr>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Internal state of [`PvDatabase`].
///
/// # Invariant — alias-aware lookup (epics-base PR #336)
///
/// **MUST**: every record-name lookup that originates from an
/// external API (CA/PVA server, link processing, iocsh, bridge
/// providers) MUST go through [`PvDatabase::get_record`] /
/// [`PvDatabase::find_entry`] / [`PvDatabase::has_name`], never
/// `inner.records.read().await.get(...)` directly.
///
/// **MUST NOT**: a function that takes an arbitrary record-name
/// `&str` and reads `inner.records` directly, unless one of:
/// - the function is itself an alias-management primitive
///   (`add_record`, `remove_record`, `add_alias`,
///   `find_entry_no_resolve`, `has_name_no_resolve`,
///   `get_record_no_resolve`, `all_record_names`), OR
/// - the name has been normalised to canonical earlier in the
///   same scope (the `let canonical_owned; let name: &str = ...`
///   pattern in `process_record_with_links_inner` /
///   `complete_async_record_inner` / `put_record_field_from_ca` /
///   `put_pv`).
///
/// **Owner/Gate:** `PvDatabase::get_record` (alias-aware path).
///
/// New code that adds a record-name entry point should call
/// `get_record` first OR run the canonical-normalisation snippet
/// at function entry. Direct `inner.records` access is reserved
/// for the alias-management primitives listed above.
/// One CP/CPP edge in the `PvDatabaseInner::cp_links` index: the record
/// to (re)process when the source record changes.
///
/// `passive_only` distinguishes CPP from CP. C adds the `CA_DBPROCESS`
/// action for a CP link unconditionally, but for a CPP link only when the
/// link-holding record's `SCAN` is Passive (`dbCa.c:825`, `:959`, `:1034`). CP
/// edges clear the flag; CPP edges set it, and `dispatch_cp_targets`
/// honours it.
#[derive(Clone, Debug)]
pub struct CpTarget {
    pub record: String,
    pub passive_only: bool,
}

/// The scan index — one independently locked bucket per [`crate::server::record::ScanList`].
///
/// C's shape (`dbScan.c`): `scan_list` carries its own `epicsMutexId lock`
/// (`:75`) and `scanList` / `addToList` / `deleteFromList` take only that
/// one list's lock, so two periodic rates never wait on each other. The port
/// used to hold a single `RwLock` over the whole `ScanList → bucket` map,
/// which is coarser than C on the highest-contention path in the database.
///
/// There is no lock over the table itself, and that is structural rather than
/// an optimisation: the set of scan lists is fixed by `menuScan`
/// ([`crate::server::record::ScanList::count`]), so the table is fully populated at construction and
/// never mutated. A bucket is reached by [`crate::server::record::ScanList::slot`], a total index —
/// there is no absent-bucket case for a caller to handle, and therefore no
/// path on which a lookup could take the wrong lock.
///
/// A scan list's sort key — C's feed order into `addToList`, spelled out.
///
/// `buildScanLists` (`dbScan.c:1054-1076`) feeds `scanAdd` **record-type-major**:
/// the outer loop walks `pdbbase->recordTypeList`, which is DBD load order, and
/// the inner loop walks that type's instances in `.db` load order. `addToList`
/// (`:1085-1091`) appends after the last element whose `phas <=` the new
/// record's, so within one PHAS the list is a stable FIFO over exactly that
/// feed order. A key ordered by `.db` load order alone inverts, by one whole
/// scan cycle, every same-PHAS reader/writer pair whose declaration order
/// contradicts DBD order.
///
/// A struct rather than a tuple because the field order IS the sort rule: the
/// derived `Ord` reads top to bottom, and a positional tuple gave the type
/// ordinal and the load-order sequence the same shape.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct ScanKey {
    phas: i16,
    /// Position in [`RECORD_TYPE_ORDER`](crate::server::record::dbd_generated::RECORD_TYPE_ORDER). A record type no
    /// vendored `.dbd` declares sorts after every one that is declared, which
    /// is where C puts it too — a module `.dbd` is included after `base.dbd`,
    /// so its types join `recordTypeList` behind base's.
    record_type: u32,
    load_order: u64,
    name: String,
}

impl ScanKey {
    fn new(phas: i16, record_type: &str, load_order: u64, name: &str) -> Self {
        use crate::server::record::dbd_generated::RECORD_TYPE_ORDER;
        Self {
            phas,
            record_type: RECORD_TYPE_ORDER
                .iter()
                .position(|t| *t == record_type)
                .unwrap_or(RECORD_TYPE_ORDER.len()) as u32,
            load_order,
            name: name.to_string(),
        }
    }
}

/// One node of the list C's `dbFirstRecord` / `dbNextRecord` pair walks.
///
/// C keeps records and aliases in a single per-record-type `recList`:
/// `dbCreateAlias` `ellAdd`s the alias node into the very list the records
/// live in (`dbStaticLib.c:1703`). A walk that wants records only therefore
/// has to SAY so — `if (dbIsAlias(pdbentry)) continue`, as `dbjlr`
/// (`dbJLink.c:520`) and `dbcar` (`dbCaTest.c:85`) do — and a walk that says
/// nothing lists both, as `dbl` (`dbTest.c:180-185`) and `dbglob`
/// (`:325-333`) do.
///
/// This port holds the two in separate maps, so "record or alias" was a
/// question each caller answered for itself, and every caller that answered
/// it by walking `all_record_names` alone answered it wrong for aliases with
/// no way to notice. [`PvDatabase::all_db_nodes`] reassembles C's one list;
/// `alias_of` is `dbIsAlias`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbNode {
    /// The name this node is reached by — an alias node carries the ALIAS
    /// name, which is what C prints from `dbGetRecordName`.
    pub name: String,
    /// The record an alias node stands for, or `None` for a record node.
    pub alias_of: Option<String>,
}

struct PvDatabaseInner {
    /// The simple-PV directory — C's `dbPvdLib.c` process-variable directory,
    /// whose per-bucket `epicsMutexId lock` (`:30`, created `:119`) is taken
    /// for both `dbPvdFind` (`:123-136`) and `dbPvdAdd` (`:150-162`). C has no
    /// reader-writer primitive anywhere in the IOC
    /// (`rg pthread_rwlock epics-base/modules/` → zero hits), so a PI mutex is
    /// not a demotion against C — it *is* C's construction.
    ///
    /// **Every reader MUST bind the lookup result in a statement of its own**
    /// (`let pv = …lock().get(name).cloned();`) rather than reading the map in
    /// an `if let` scrutinee. The guard is `!Send`, and an `if let` scrutinee
    /// temporary lives to the end of the `if let` *body* — so the scrutinee
    /// form keeps the guard alive across any `.await` the body makes and turns
    /// the enclosing `async fn` into a `!Send` future at its `tokio::spawn`
    /// site. The rule is uniform across all 17 read sites, not applied only to
    /// the ones that await today, so a new `.await` in an existing body cannot
    /// re-open it.
    simple_pvs:
        crate::runtime::sync::PriorityInheritanceMutex<HashMap<String, Arc<ProcessVariable>>>,
    /// Wake-up for servers holding channels on this database: something was
    /// removed, re-check what you serve. Carries no name — the receiver
    /// tests the target it already holds
    /// ([`ProcessVariable::is_destroyed`] /
    /// [`RecordInstance::is_destroyed`]), so an alias, a `.FIELD` suffix or
    /// a `.{filter}` suffix on the client's channel name cannot make the
    /// match miss the way a name comparison would.
    pv_destroyed_tx: tokio::sync::broadcast::Sender<()>,
    records: parking_lot::RwLock<HashMap<String, Arc<parking_lot::RwLock<RecordInstance>>>>,
    /// Scan index: maps scan list → sorted set of [`ScanKey`].
    ///
    /// C parity (`dbScan.c:1052-1095`): `buildScanLists` walks record types in
    /// DBD load order and, within each, that type's instances in `.db` load
    /// order; `addToList` inserts each after the last element with
    /// `phas <= precord->phas`, so within one PHAS the list is a stable FIFO
    /// over that feed order. Both halves of the feed order are in the key —
    /// the record-type ordinal first, the `.db` load sequence second. The
    /// record name is only a final tiebreak and never decides the order of two
    /// real records.
    /// Keyed by [`crate::server::record::ScanList`], not `ScanType`: a `Passive` or illegal SCAN names
    /// no list (C `scanAdd`, dbScan.c:241-251) and so cannot be a key at all.
    ///
    /// **One lock per scan list, not one lock over the index.** C has one
    /// `epicsMutexId lock` per `scan_list` (`dbScan.c:75`, created `:527`,
    /// `:604`, `:908`) and never serialises two rates against each other. A
    /// single map-wide lock would serialise the seven periodic threads (bands
    /// 60–66) that C runs independently, on the one path where both ends of
    /// the contention pair are banded. See [`scan_index::ScanIndex`].
    scan_index: scan_index::ScanIndex,
    /// Per-record load-order sequence number, assigned monotonically
    /// at `add_record`. Used as the secondary scan-index sort key so
    /// same-PHAS records preserve database load order. Survives a
    /// `remove_record` + re-`add_record` (the re-add gets a fresh,
    /// higher sequence — matching a fresh `.db` reload).
    ///
    /// Read-modify-write cell (`add_loaded_record` inserts, `remove_record`
    /// removes), so it is a [`SnapshotCell`], not a bare `ArcSwap`: the
    /// writer gate is what makes insert-then-publish atomic. Both writers
    /// also hold [`Self::registration_mutex`] today, but the gate keeps the
    /// RMW correct without depending on that — L46's type changes in step 4.
    load_order: SnapshotCell<HashMap<String, u64>>,
    /// Monotonic counter feeding `load_order`.
    load_order_counter: std::sync::atomic::AtomicU64,
    /// CP/CPP link index: maps source_record → target edges to process when
    /// the source changes. Each edge carries the CP-vs-CPP distinction (see
    /// [`CpTarget`]).
    ///
    /// Read-modify-write cell with **two writers that share no other gate**:
    /// `register_cp_link` (`links.rs:2918`) takes no
    /// [`Self::registration_mutex`], `remove_record` (`mod.rs:2056`) does. The
    /// `RwLock`'s write exclusion was the only thing serialising them, so the
    /// [`SnapshotCell`] writer gate here is required, not defensive.
    cp_links: SnapshotCell<HashMap<String, Vec<CpTarget>>>,
    /// External (CA/PVA) CP/CPP link index: maps the *external PV name*
    /// (the cross-IOC source, e.g. `OTHER:PV` from `INP="OTHER:PV CP"`)
    /// → holder edges to process when that remote PV changes. The local
    /// [`Self::cp_links`] index is keyed by a local source RECORD that
    /// processes here; a cross-IOC source never processes locally, so its
    /// only trigger is the calink/pvalink CA monitor callback, which calls
    /// [`PvDatabase::dispatch_external_cp_targets`]. Parity with C
    /// `dbCa.c:958-962` `eventCallback` adding `CA_DBPROCESS`.
    ///
    /// Read-modify-write cell; sole writer `register_external_cp_link`
    /// (`links.rs:2968`) merges into an existing edge list, so concurrent
    /// registrations need the [`SnapshotCell`] writer gate.
    external_cp_links: SnapshotCell<HashMap<String, Vec<CpTarget>>>,
    /// Alias map: alternate-name → real-record-name. Mirrors epics-base
    /// PR #336 (alias name validation + parsing). `find_entry` and
    /// related lookups consult this map after the canonical record
    /// table so an alias resolves transparently to its target.
    aliases: parking_lot::RwLock<HashMap<String, String>>,
    /// Single gate that serializes
    /// every `add_pv` / `add_pv_with_hook` / `add_record` /
    /// `add_alias` / `remove_record` / `remove_simple_pv` /
    /// `remove_alias`. Without this, the per-method write-lock
    /// orders (`simple_pvs` first vs. `records` first vs.
    /// `aliases` first) could deadlock under concurrent registrations,
    /// and `add_record`'s post-insert `scan_index.write()` had a
    /// TOCTOU window where `remove_record` could land between the
    /// records map insert and the scan-index insert and leave a
    /// phantom scan entry.
    ///
    /// Holding this mutex makes the cross-namespace `check_name_free`
    /// peek atomic with the target-map insert, eliminates the
    /// scan-index race, and lets `remove_*` purge dangling aliases
    /// without a second pass.
    ///
    /// This gate is taken
    /// **inside** the L1 record-gate window on the SCAN-put path
    /// (`scan_index.rs:122`, reached from `field_io.rs`'s `update_scan_index`
    /// calls), so it converts with L8a/L8b rather than after them — leaving it
    /// async while the locks nested under it are blocking is the worst of both.
    /// The acquisition-order MUST rule that governs the nesting is in
    /// `record_lock.rs`'s module doc.
    ///
    /// **No holder may `.await` while holding it.** The guard is `!Send`, so
    /// the compiler enforces this at every `tokio::spawn` site; the eight
    /// holders were audited before the conversion and the only suspension
    /// points any of them had were acquisitions of `simple_pvs` and
    /// `scan_index`, both blocking now.
    ///
    /// Acquired ONLY through [`PvDatabase::lock_registration`], never
    /// directly — that funnel is what turns a re-entrant take into a named
    /// panic instead of a parked thread. See [`RegistrationGate`].
    registration_mutex: crate::runtime::sync::PriorityInheritanceMutex<()>,
    /// The IOC lifecycle phase — the port's `iocInit` boundary. See
    /// [`DbInitPhase`], [`PvDatabase::begin_load`],
    /// [`PvDatabase::schedule_record_init`] and [`PvDatabase::ioc_init`].
    init_phase: std::sync::Mutex<DbInitPhase>,
    /// Record inits parked because the record they classify is not registered
    /// yet — see [`PvDatabase::schedule_record_init`]. Keyed by record name and
    /// released by [`PvDatabase::add_loaded_record`] the moment that name lands
    /// in `records`, which is what makes "the init observes a registered
    /// record" hold by construction rather than by scheduler timing.
    record_init_waiting: std::sync::Mutex<HashMap<String, Vec<RecordInit>>>,
    /// C `plink->text != NULL` for the one case the port's link storage cannot
    /// tell apart on its own: a link field the `.db` assigned the EMPTY string.
    ///
    /// `dbInitRecordLinks` checks a link only when the load gave it text
    /// (`if (!plink->text) continue;`, `dbStaticLib.c:2213`), so
    /// `field(INP,"")` on an `INST_IO` device is refused while the same record
    /// with no `INP` line at all is not — and this port keeps link text in the
    /// field itself, where those two are the same empty string. Only `INP` and
    /// `OUT` can be refused while empty (every other link field expects
    /// `CONSTANT`, which the empty text already is), and the loader always
    /// routes those two through `RecordLoad::common_fields`, so recording the
    /// empty assignments here makes the distinction exact rather than
    /// approximate. Consumed and cleared by
    /// [`PvDatabase::db_init_record_links`], the way C frees the text.
    empty_link_assignments: std::sync::Mutex<HashMap<String, Vec<String>>>,
    /// Lines queued by the iocsh `afterIocRunning <command>` directive
    /// (epics-base PR #558). Drained by the IOC application after PINI
    /// completes, then re-executed through a fresh IocShell so the
    /// commands run with the database in its post-init state.
    after_ioc_running: std::sync::Mutex<Vec<String>>,
    /// Optional resolver for external PVs (ca://, pva:// links).
    ///
    /// Whole-value replace: the only writer stores a complete new value
    /// ([`PvDatabase::set_external_resolver`]), so an
    /// [`ArcSwapOption`] store IS the mutation and no writer gate is
    /// needed. Readers take the `Arc` with no lock at all.
    external_resolver: ArcSwapOption<ExternalPvResolver>,
    /// Optional async resolver invoked on `has_name` misses (e.g. CA gateway).
    ///
    /// Whole-value replace, as [`Self::external_resolver`] (§3 row L8g).
    search_resolver: ArcSwapOption<SearchResolver>,
    /// Optional per-request gate consulted before a *cached* simple PV is
    /// advertised as existing (e.g. CA gateway host/state admission). See
    /// [`ExistenceGate`]. `None` for a plain IOC (short-circuit unchanged).
    ///
    /// Whole-value replace, as [`Self::external_resolver`] (§3 row L8h).
    existence_gate: ArcSwapOption<ExistenceGate>,
    /// The database debugger, installed by the first `dbb` and removed when
    /// its last breakpoint goes — C's `lset_stack_count`, which `dbProcess`
    /// tests before it calls either breakpoint hook (`dbAccess.c:504`,
    /// `:614`).
    ///
    /// Whole-value replace, as [`Self::external_resolver`]: a database nobody
    /// is debugging pays one relaxed atomic load per processed record where C
    /// pays one comparison, and the mechanism stays testable and removable
    /// instead of welded into `process_record_with_links_body` as a pair of
    /// `if` branches.
    breakpoints: ArcSwapOption<breakpoint::BreakpointTable>,
    /// Per-scheme link sets — pluggable backends for `pva://` /
    /// `ca://` link resolution. Consulted before the legacy
    /// [`ExternalPvResolver`] in `resolve_external_pv`.
    /// Mirrors the C-EPICS lset abstraction.
    ///
    /// Read-modify-write cell (`register_link_set` inserts one scheme into
    /// the existing registry), so it takes the [`SnapshotCell`] writer gate.
    /// Every reader either resolves one scheme inside a single expression or
    /// collects the lsets and drops the registry **before** awaiting — the
    /// deliberate discipline documented at `links.rs:951-952` — so a coherent
    /// snapshot is what the read paths already assumed.
    link_sets: SnapshotCell<link_set::LinkSetRegistry>,
    /// Pending external OUT-link writes — the `dbCa` `workList` analogue.
    /// Record processing stages a write here and returns; the queue's single
    /// owner task performs the `ca://`/`pva://` network write off the
    /// record's advisory write gate, exactly as `dbCaTask` does
    /// (`dbCa.c:1093-1260`). See [`link_put_queue`].
    link_puts: Arc<link_put_queue::LinkPutQueue>,
    /// True once the ScanScheduler has been started for this DB.
    /// Prevents duplicate scan tasks when multiple protocol servers (CA + PVA)
    /// both try to start scanning on the same DB.
    scan_started: std::sync::atomic::AtomicBool,
    /// True once PINI processing has completed. Non-owner schedulers await
    /// this before running their hooks, preserving the "PINI before hooks"
    /// ordering contract.
    pini_done: std::sync::atomic::AtomicBool,
    /// Fired by the scan owner after PINI completes. Non-owners register
    /// interest on this before re-checking `pini_done` to avoid missing the
    /// signal (`notify_waiters` does not store a permit).
    pini_notify: tokio::sync::Notify,
    /// Per-record advisory write gates — the Rust
    /// counterpart of the C-EPICS `dbScanLock` / `dbLocker`
    /// machinery. Every plain CA/PVA write, the QSRV atomic group
    /// PUT/GET, and the pvalink atomic scan-on-update epoch all
    /// acquire these gates, so no two of them can interleave on a
    /// shared record. See [`record_lock`].
    record_locks: record_lock::RecordLockRegistry,
    /// Per-record-type attributes — C `dbRecordType::attributeList`.
    ///
    /// C materialises the list once the `.dbd` is read: `dbReadCOM`'s tail
    /// (`dbLexRoutines.c:311-331`) gives every record type `RTYP` = its own
    /// name and `VERS` = `"none specified"`, and `dbPutRecordAttribute`
    /// (`dbStaticLib.c:1232-1277`) adds or overwrites from there. Both maps
    /// are `BTreeMap`s because C keeps the inner list in `strcmp` order and
    /// walks it in that order in `dbGetAttributePart` and
    /// `dbDumpRecordType`; sorting is the container's job here rather than an
    /// insertion dance.
    ///
    /// A leaf lock: no other lock is taken while it is held, and it is taken
    /// while a record read guard is live (`PvDatabase::get_pv`), never the
    /// reverse.
    record_attributes: crate::runtime::sync::PriorityInheritanceMutex<
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    >,
    /// Subroutine functions by name, retained at runtime so the processing
    /// path can re-resolve an aSub's subroutine when its name changes
    /// (C `aSubRecord.c::fetch_values` `registryFunctionFind`, LFLG=READ /
    /// SUBL). Populated once at iocInit from the IocApp/IocBuilder registry;
    /// read-only thereafter.
    ///
    /// Whole-registry replace ([`PvDatabase::install_subroutine_registry`]),
    /// so an [`ArcSwap`] store IS the mutation and no writer gate is needed
    /// (§3 row L8j). `OnceLock` was rejected: install is a `pub async fn` that
    /// tests and a second `iocInit` may call again, and `OnceLock` would
    /// silently drop the second registry instead of replacing it.
    subroutine_registry: ArcSwap<HashMap<String, Arc<crate::server::record::SubroutineFn>>>,
    /// C's process-global device support table, seen through
    /// `dbDTYPtoDevSup`: the lookup `doInitRecord0` makes to fill
    /// `precord->dset` BEFORE it calls `prset->init_record(precord, 0)`
    /// (`iocInit.c:530-536`).
    ///
    /// It lives on the DATABASE, not on the builder that collected the
    /// factories, because the creation sink is the only place that can bind a
    /// dset at C's position — ahead of the record's own init passes. While the
    /// two builders each held their own copy, the bind could only happen after
    /// the whole database had been built, so every record type's `init_record`
    /// ran with `dset == NULL` invisible to it and executed the tail C's
    /// `if (!pdset) return S_dev_noDSET` skips.
    ///
    /// `None` until a builder installs one — a database assembled by hand
    /// (unit tests, `PvDatabase::new`) registers no device support at all,
    /// which is C's empty `devList` and gives the same answer.
    device_support_resolver: ArcSwapOption<crate::server::ioc_app::DeviceSupportResolver>,
    /// Breakpoint tables by name (C `bptList`), shared by every db-load path so
    /// `ai`/`ao` records with `LINR >= 3` resolve their linearisation table. An
    /// `Arc` snapshot is installed on each record at creation; the master grows
    /// (copy-on-write via [`PvDatabase::add_breaktables`]) as `dbLoadRecords`
    /// loads more `breaktable(...)` definitions, so build-time and runtime
    /// loads share one registry.
    ///
    /// Read-modify-write cell (`add_breaktables` clones the registry, inserts
    /// and republishes), so it takes the [`SnapshotCell`] writer gate. The
    /// value was already `Arc`-shared, so the cell replaces the outer lock
    /// with nothing at all on the read side.
    breaktable_registry: SnapshotCell<crate::server::cvt_bpt::BreakTableRegistry>,
}

thread_local! {
    /// Set for exactly as long as this thread holds L46. Read only by
    /// [`PvDatabase::lock_registration`].
    static REGISTRATION_GATE_HELD: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// RAII guard for L46, `PvDatabaseInner::registration_mutex`.
///
/// L46 is a `PriorityInheritanceMutex` and is therefore NOT reentrant: a
/// thread that takes it twice parks on itself forever. That makes the
/// caller-side rule a MUST, and it is the half the lock-order table in
/// `super::record_lock` did not state —
/// [`PvDatabase::update_scan_index`] is the single owner of a scan-index
/// transition and takes L46 **itself**, so no caller may hold L46 across a
/// call to it. Releasing early is also what C does: `iterateRecords`
/// (`iocInit.c:562-586`) is a separate pass over an already-built database,
/// holding no registration lock at all.
///
/// A violation used to surface as a hung thread, which reads as a flaky
/// timeout and costs a bisect to attribute. This guard makes it surface as a
/// panic naming both the holder and the re-entrant site.
#[must_use = "L46 is released as soon as the guard is dropped"]
pub(crate) struct RegistrationGate<'a> {
    _guard: crate::runtime::sync::PriorityInheritanceMutexGuard<'a, ()>,
}

impl Drop for RegistrationGate<'_> {
    fn drop(&mut self) {
        REGISTRATION_GATE_HELD.with(|h| h.set(None));
    }
}

/// Database of all process variables hosted by this server.
#[derive(Clone)]
pub struct PvDatabase {
    inner: Arc<PvDatabaseInner>,
}

/// A record initialisation owed to `iocInit` — the port's `init_record`
/// tail. Built by a record's `refresh_link_status` and handed to
/// [`PvDatabase::schedule_record_init`].
type RecordInit = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

/// The IOC lifecycle phase, and with it the answer to "may a record's links be
/// classified against the database as it stands right now?".
///
/// C runs `init_record` — where a record classifies its links (`checkLinks`,
/// `dbNameToAddr`) — from `iocInit`, i.e. after EVERY `dbLoadRecords` block
/// has been read. A forward reference across two `dbLoadRecords` calls in one
/// `st.cmd` is therefore a LOCAL link, deterministically, and the classified
/// value is final the moment `iocInit` returns (`dbgf` is refused before it).
///
/// The boundary is `iocInit`, NOT a load group: gating on the load group left
/// the multi-`dbLoadRecords` case every real `st.cmd` uses racing 9-in-15
/// (R18-92). So the phase here is an ioc-lifecycle state.
///
/// # The lifecycle is ONE-WAY: `Unloaded → Loading → Running`
///
/// R18-92 modelled it with two states, `Loading` and `Complete`, where
/// `Complete` meant BOTH "never loaded" and "iocInit has run" — so `begin_load`
/// needed a `Complete → Loading` arm to open the phase at all, and that arm ran
/// on a post-iocInit load too. One `dbLoadRecords` typed after `iocInit` then
/// re-armed the queue that only `ioc_init` drains, and every later
/// classification — including every runtime `special()` link re-point — was
/// pushed into a `Vec` nothing polls (R19-62, measured: `iocInit;
/// dbLoadRecords(b.db); dbpf CO.INPA "9.5"` froze `CO.INAV` at 0).
///
/// Splitting the two meanings is what closes it: `Loading` is now produced ONLY
/// from `Unloaded`, so no function in the crate can transition backwards out of
/// `Running`. The one-way-ness is a property of the transitions that exist, not
/// of a runtime check.
enum DbInitPhase {
    /// No load has begun. `iocInit` is owed nothing, so a classification runs
    /// immediately — a programmatically built or unit-test database.
    Unloaded,
    /// Between the first `dbLoadRecords`/builder load and `iocInit`; holds the
    /// classifications owed, in issue order. A half-built database is never
    /// observed, because no classification code runs against one.
    Loading(Vec<RecordInit>),
    /// Inside [`PvDatabase::ioc_init`], for the length of
    /// [`PvDatabase::db_init_record_links`]. That pass rewrites link text, so
    /// the classifications it triggers belong to THIS build and must be
    /// awaited by the barrier — spawning them would let `ioc_init` return
    /// while a record's `INAV`/`OUTV` still described text the pass replaced.
    /// Also the barrier's own claim: a second `ioc_init` sees a phase that is
    /// neither `Unloaded` nor `Loading` and returns, and a `dbLoadRecords`
    /// racing the barrier is refused exactly as a post-`iocInit` load is.
    Initialising(Vec<RecordInit>),
    /// `iocInit` has run: the database is final and every link status is
    /// classified. A classification issued now runs immediately, which is what a
    /// runtime re-point (`special()` on a link field) needs. TERMINAL — nothing
    /// re-opens the load phase.
    Running,
}

/// [`PvDatabase::begin_load`] was called on a database whose `iocInit` has
/// already run — C's `getIocState() != iocVoid` (R19-63).
///
/// The `Display` text is C's `errSymMsg(S_dbLib_postInitRecRegister)` verbatim
/// (`dbStaticLib.h:269`), which is what `dbCreateRecord` prints:
///
/// ```text
/// epics> dbCreateRecord(pdbbase,"ai","NEWREC")
/// ERROR: 33554463 IOC already initialized - No new records can be added
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IocAlreadyInitialized;

impl std::fmt::Display for IocAlreadyInitialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IOC already initialized - No new records can be added")
    }
}

impl std::error::Error for IocAlreadyInitialized {}

/// Which record kind a SELM link selection is being computed for.
/// The Specified/Mask base differs between record types in C, so the
/// shared selector must know the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SelmKind {
    /// `fanout` / `seq`: Specified index is `SELN + OFFS` (0-based over
    /// LNK0..LNKF / group 0..15); Mask is shifted by `SHFT`.
    /// Mirrors `fanoutRecord.c:106-141` and `seqRecord.c:147-178`.
    FanoutSeq,
    /// `dfanout`: Specified index is `SELN - 1` (1-based, `SELN==0`
    /// means "drive nothing", `SELN > OUT_ARG_MAX` is invalid); Mask
    /// has NO `SHFT` and `SELN==0` means "no output".
    /// Mirrors `dfanoutRecord.c:308-339`.
    Dfanout,
}

/// Result of resolving a SELM/SELN selection.
#[derive(Clone, Debug, Default)]
pub(crate) struct SelmResult {
    /// 0-based link indices to drive (into the LNK0../OUTA.. array).
    pub indices: Vec<usize>,
    /// `Some` when C would raise an alarm for an out-of-range
    /// `SELN`/`OFFS`/`SHFT`. C uses `recGblSetSevr(prec, SOFT_ALARM,
    /// INVALID_ALARM)` in every such path.
    pub alarm: Option<(u16, crate::server::record::AlarmSeverity)>,
}

/// Convert a link value to `epicsUInt16` with C `dbGetLink(.., DBR_USHORT,
/// ..)` semantics, for the fanout/dfanout/seq `SELL`→`SELN` read — so a
/// constant, DB, CA, or PVA link source all convert by the one rule C applies
/// through `dbFastGetConvertRoutine`.
///
/// # The source type decides the rule, because in C it decides the routine
///
/// `dbFastGetConvertRoutine` is a 2-D table indexed by *both* the source DBF
/// and the destination DBR (`dbConvert.c:1571-1638`): a `DBF_LONG` source
/// reaches `getLongUshort`, a `DBF_DOUBLE` source reaches `getDoubleUshort`.
/// They are different functions, and C gives them different semantics:
///
/// * **Integer source** — `(epicsUInt16)(epicsInt32)v`. Conversion of an
///   out-of-range *integer* to an unsigned type is **defined** in C
///   (C17 6.3.1.3p2: reduce modulo `USHRT_MAX + 1`). Every compiler and
///   every target agrees, so this is a real contract and the port keeps it:
///   `SELL` pointing at a `DBF_LONG` field holding `-1` gives `SELN = 65535`.
/// * **Float source** — `(epicsUInt16)d`. Conversion of an out-of-range
///   *float* is **undefined** (C17 6.3.1.4p1), so compiled C is not
///   single-valued: x86-64 wraps, aarch64 saturates. What the port does about
///   that is [`crate::types::c_cast`]'s call — the single owner of the policy —
///   and deliberately not restated here.
///
/// Both rules already live in [`EpicsValue::convert_to`], the single
/// value-coercion owner: it takes the integer view (`as_int_i64`) when the
/// source has one and falls back to `c_cast` only for a genuine float. So this
/// is a thin projection onto that owner, NOT a second conversion table.
///
/// The previous revision called `c_cast::f64_to_u16(value.to_f64())` directly,
/// bypassing the owner — which silently applied the float rule to integer
/// sources too, losing the one wrap C actually defines.
pub(crate) fn dbr_ushort_cast(value: &EpicsValue) -> u16 {
    match value.convert_to(crate::types::DbFieldType::UShort) {
        EpicsValue::UShort(v) => v,
        // A link that delivers an array converts element-wise; C's
        // `dbGetLink(.., &prec->seln, 0, 0)` requests ONE element, so SELN
        // takes the first (an empty array leaves it 0).
        EpicsValue::UShortArray(v) => v.first().copied().unwrap_or(0),
        // `convert_to(UShort)` returns no other variant.
        _ => 0,
    }
}

/// Select which link indices are active based on SELM/SELN, applying
/// the record-type-specific `OFFS`/`SHFT` bias.
///
/// SELM: 0 = All, 1 = Specified, 2 = Mask. `count` is the number of
/// link slots (16 for fanout/dfanout/seq).
///
/// `seln` is the native `DBF_USHORT` value: C declares `SELN` as
/// `epicsUInt16`, so every comparison below is unsigned, matching C's
/// selection arithmetic — never `-1`. What an out-of-range `SELL` converts
/// *to* is [`dbr_ushort_cast`]'s decision, not this function's.
///
/// C references:
/// * fanout — `fanoutRecord.c:106-141`
/// * dfanout — `dfanoutRecord.c:308-339`
/// * seq — `seqRecord.c:147-178`
pub(crate) fn select_link_indices_ex(
    kind: SelmKind,
    selm: i16,
    seln: u16,
    offs: i16,
    shft: i16,
    count: usize,
) -> SelmResult {
    use crate::server::recgbl::alarm_status::SOFT_ALARM;
    use crate::server::record::AlarmSeverity;

    let invalid = || SelmResult {
        indices: Vec::new(),
        alarm: Some((SOFT_ALARM, AlarmSeverity::Invalid)),
    };
    let ok = |indices: Vec<usize>| SelmResult {
        indices,
        alarm: None,
    };

    match selm {
        // All — every slot.
        0 => ok((0..count).collect()),
        // Specified.
        1 => match kind {
            SelmKind::FanoutSeq => {
                // C: `i = seln + offs;` with `seln` unsigned (epicsUInt16),
                // 0-based; `i<0 || i>=NLINKS` → INVALID. So `SELN=65535`
                // (from `SELL=-1`) yields `i>=NLINKS` → INVALID, never
                // drives link 0.
                let i = seln as i32 + offs as i32;
                if i < 0 || i >= count as i32 {
                    invalid()
                } else {
                    ok(vec![i as usize])
                }
            }
            SelmKind::Dfanout => {
                // C `dfanoutRecord.c:315-320`: `if (prec->seln > OUT_ARG_MAX)`
                // with `seln` unsigned → INVALID; `seln == 0` → no output;
                // otherwise drive `seln - 1`. OFFS is not a dfanout field.
                // `SELL=-1` → `SELN=65535` > count → INVALID (the signed
                // read used to see `-1`, take the `<= 0` branch, and drive
                // nothing with no alarm).
                let seln_i = seln as i32;
                if seln_i > count as i32 {
                    invalid()
                } else if seln_i == 0 {
                    ok(Vec::new())
                } else {
                    ok(vec![(seln_i - 1) as usize])
                }
            }
        },
        // Mask.
        2 => {
            let mask: u32 = match kind {
                SelmKind::FanoutSeq => {
                    // C: SHFT shift first, with `shft` range-checked to [-15,15].
                    if !(-15..=15).contains(&shft) {
                        return invalid();
                    }
                    let raw = seln as u32;
                    if shft >= 0 {
                        raw >> shft
                    } else {
                        raw << (-shft)
                    }
                }
                // dfanout Mask has no SHFT.
                SelmKind::Dfanout => seln as u32,
            };
            ok((0..count).filter(|i| mask & (1 << i) != 0).collect())
        }
        // Any other SELM value → C `default:` raises INVALID.
        _ => invalid(),
    }
}

/// C `MAX_STRING_SIZE` is 40 and `dbPutRecordAttribute` NUL-terminates at
/// `[MAX_STRING_SIZE-1]`, so an attribute value keeps 39 characters.
const MAX_ATTRIBUTE_LEN: usize = 39;

/// Why a `dbPutAttribute` was refused, carrying the C status the shell
/// reports and the `errSymLookup` text `errMessage` prints in front of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordAttributeError {
    /// C `S_db_badField` (`dbAccess.c:443-445`) — no attribute name was
    /// given. C tests `!name`; an iocsh argument that is absent and one that
    /// is `""` are the same absent argument here.
    BadField,
    /// C `S_dbLib_recordTypeNotFound`, from `dbFindRecordType`
    /// (`dbAccess.c:453`) or `dbPutRecordAttribute`'s own `!precordType`
    /// guard (`dbStaticLib.c:1240`).
    RecordTypeNotFound,
}

impl RecordAttributeError {
    /// The `errMdef.h` status number, as a C console prints it.
    #[must_use]
    pub const fn status(self) -> u32 {
        match self {
            // `M_dbAccess` is `511 << 16`, `S_db_badField` is `|15`.
            Self::BadField => (511 << 16) | 15,
            // `M_dbLib` is `512 << 16`, `S_dbLib_recordTypeNotFound` is `|1`.
            Self::RecordTypeNotFound => (512 << 16) | 1,
        }
    }

    /// The `errSymLookup` text `errMessage` prefixes its own message with.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::BadField => "Illegal field value",
            Self::RecordTypeNotFound => "Record Type does not exist",
        }
    }
}

/// C `dbReadCOM`'s tail (`dbLexRoutines.c:311-331`): once the `.dbd` is read,
/// every record type it declared carries `RTYP` = its own name and `VERS` =
/// `"none specified"`. The port's `.dbd` is the generated table, which is
/// complete before the first `dbLoadRecords`, so the seed happens at
/// construction instead of after a read.
fn seeded_record_attributes()
-> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    crate::server::record::dbd_generated::RECORD_TYPES
        .iter()
        .map(|t| {
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert("RTYP".to_string(), (*t).to_string());
            attrs.insert("VERS".to_string(), "none specified".to_string());
            ((*t).to_string(), attrs)
        })
        .collect()
}

impl PvDatabase {
    /// Acquire L46, `registration_mutex` — the ONE acquisition site.
    ///
    /// `site` names the acquiring function and appears in the panic message
    /// when the rule below is broken, so the report identifies the violator
    /// without a debugger.
    ///
    /// # Panics
    ///
    /// If this thread already holds L46. That is not a defensive check
    /// against an impossible input: L46 is a `PriorityInheritanceMutex`, so
    /// the second acquisition would park the thread on itself and never
    /// return. The panic replaces a hang, which is the worst failure shape
    /// available — it reaches CI as a timeout, and a timeout reads as a load
    /// flake rather than as the ordering bug it is.
    pub(crate) fn lock_registration(&self, site: &'static str) -> RegistrationGate<'_> {
        if let Some(holder) = REGISTRATION_GATE_HELD.with(|h| h.get()) {
            panic!(
                "L46 registration_mutex is not reentrant: `{site}` took it while \
                 this thread still holds it from `{holder}`. `update_scan_index` \
                 takes L46 itself and is the single owner of a scan-index \
                 transition, so a caller must DROP its registration gate before \
                 reaching it — see `RegistrationGate`."
            );
        }
        let guard = self.inner.registration_mutex.lock();
        REGISTRATION_GATE_HELD.with(|h| h.set(Some(site)));
        RegistrationGate { _guard: guard }
    }

    pub fn new() -> Self {
        Self {
            inner: Arc::new(PvDatabaseInner {
                simple_pvs: crate::runtime::sync::PriorityInheritanceMutex::new(HashMap::new()),
                pv_destroyed_tx: tokio::sync::broadcast::channel(16).0,
                external_resolver: ArcSwapOption::empty(),
                breakpoints: ArcSwapOption::empty(),
                search_resolver: ArcSwapOption::empty(),
                existence_gate: ArcSwapOption::empty(),
                link_sets: SnapshotCell::new(link_set::LinkSetRegistry::new()),
                link_puts: Arc::new(link_put_queue::LinkPutQueue::default()),
                records: parking_lot::RwLock::new(HashMap::new()),
                scan_index: scan_index::ScanIndex::new(),
                load_order: SnapshotCell::new(HashMap::new()),
                load_order_counter: std::sync::atomic::AtomicU64::new(0),
                cp_links: SnapshotCell::new(HashMap::new()),
                external_cp_links: SnapshotCell::new(HashMap::new()),
                aliases: parking_lot::RwLock::new(HashMap::new()),
                registration_mutex: crate::runtime::sync::PriorityInheritanceMutex::new(()),
                init_phase: std::sync::Mutex::new(DbInitPhase::Unloaded),
                record_init_waiting: std::sync::Mutex::new(HashMap::new()),
                empty_link_assignments: std::sync::Mutex::new(HashMap::new()),
                after_ioc_running: std::sync::Mutex::new(Vec::new()),
                scan_started: std::sync::atomic::AtomicBool::new(false),
                pini_done: std::sync::atomic::AtomicBool::new(false),
                pini_notify: tokio::sync::Notify::new(),
                record_locks: record_lock::RecordLockRegistry::default(),
                record_attributes: crate::runtime::sync::PriorityInheritanceMutex::new(
                    seeded_record_attributes(),
                ),
                subroutine_registry: ArcSwap::from_pointee(HashMap::new()),
                device_support_resolver: ArcSwapOption::empty(),
                breaktable_registry: SnapshotCell::new(
                    crate::server::cvt_bpt::BreakTableRegistry::new(),
                ),
            }),
        }
    }

    /// C `dbPutAttribute` (`dbAccess.c:436-460`) minus its shell wrapper:
    /// set or create one attribute of one record type.
    ///
    /// C's `!pdbbase` arm is unreachable here — the record-type set is the
    /// generated `dbd_generated::RECORD_TYPES` table, which exists before any
    /// database is loaded — so `S_db_notFound` has no site.
    ///
    /// `name` and `value` are `Option` because C's are `const char *` that
    /// iocsh passes as NULL for an argument the operator omitted, and the two
    /// NULLs mean different things: a missing name is `S_db_badField`
    /// (`dbAccess.c:443-445`) while a missing value is `""`
    /// (`:446-447`). An argument given as `""` is NOT missing — C's
    /// `dbPutRecordAttribute` happily creates an attribute whose name is the
    /// empty string, measured on `softIoc` R7.0.10-146.
    ///
    /// Truncation is C's: `strncpy(pattribute->value, value, MAX_STRING_SIZE)`
    /// followed by `value[MAX_STRING_SIZE-1] = 0` keeps 39 characters, not 40.
    pub fn put_record_type_attribute(
        &self,
        record_type: &str,
        name: Option<&str>,
        value: Option<&str>,
    ) -> Result<(), RecordAttributeError> {
        let Some(name) = name else {
            return Err(RecordAttributeError::BadField);
        };
        let value = value.unwrap_or("");
        if !crate::server::record::dbd_generated::RECORD_TYPES.contains(&record_type) {
            return Err(RecordAttributeError::RecordTypeNotFound);
        }
        // C truncates by byte (`strncpy` then `[MAX_STRING_SIZE-1] = 0`),
        // which can leave a partial UTF-8 sequence; truncating by character
        // keeps the same 39 for every name iocsh can pass and never produces
        // a value that is not a string.
        let truncated: String = value.chars().take(MAX_ATTRIBUTE_LEN).collect();
        self.inner
            .record_attributes
            .lock()
            .entry(record_type.to_string())
            .or_default()
            .insert(name.to_string(), truncated);
        Ok(())
    }

    /// C `dbGetAttributePart` (`dbStaticLib.c:1279-1315`) for a whole name —
    /// the fallback `pvNameLookup` takes when `dbFindFieldPart` answers
    /// `S_dbLib_fieldNotFound` (`dbChannel.c:326-327`).
    ///
    /// The caller owes the shadowing test: C reaches this only after the
    /// record type's declared field list has missed, so a type that declares
    /// a field of the same name (`motor.VERS`) hides the attribute.
    pub fn record_type_attribute(&self, record_type: &str, name: &str) -> Option<String> {
        self.inner
            .record_attributes
            .lock()
            .get(record_type)?
            .get(name)
            .cloned()
    }

    /// One record type's whole attribute list in C's `strcmp` order — the
    /// order `dbPutRecordAttribute` inserts in and `dbDumpRecordType` prints.
    pub fn record_type_attributes(&self, record_type: &str) -> Vec<(String, String)> {
        self.inner
            .record_attributes
            .lock()
            .get(record_type)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Merge `tables` into the shared breakpoint-table registry (C `bptList`
    /// accumulation across `dbLoadDatabase`/`dbLoadRecords`) and return the new
    /// snapshot. Copy-on-write: a new merged registry replaces the old one.
    ///
    /// `add_breaktables` is the single registry-mutation owner, so it also
    /// restores the invariant *every record can resolve against the current
    /// registry* on mutation: the new snapshot is re-installed into every
    /// existing record. That covers a record created before its table was
    /// loaded (an inline record added before `dbLoadRecords`, or a merge-reload
    /// that repoints `LINR` to a table loaded in the same command) — neither
    /// of which goes back through `add_record`'s install. `install_*` is a
    /// no-op for non-ai/ao records and resets the cached table so the new
    /// registry wins. Returns the current snapshot unchanged when `tables` is
    /// empty (no mutation, so no re-install).
    pub async fn add_breaktables(
        &self,
        tables: Vec<crate::server::cvt_bpt::BrkTable>,
    ) -> Arc<crate::server::cvt_bpt::BreakTableRegistry> {
        // Hold the registration gate across the registry write AND the record
        // snapshot below so this mutation cannot interleave with `add_record`'s
        // [registry read -> records-map insert] — both are gated by the same
        // mutex. Without it a record created concurrently could read the
        // pre-mutation registry (miss the just-loaded table) while not yet
        // being in the records map for the re-install below, leaving a
        // table-not-found alarm until the next load / LINR put. `add_record`
        // holds this gate across its whole body (registry read + map insert),
        // so taking it here closes that TOCTOU window. No `add_breaktables`
        // caller already holds the gate, so this is reentrancy-safe.
        let _gate = self.lock_registration("add_breaktables");
        if tables.is_empty() {
            return self.inner.breaktable_registry.load_full();
        }
        let snapshot = self.inner.breaktable_registry.update(|next| {
            for table in tables {
                next.insert(table);
            }
        });
        // Re-install into existing records. Snapshot the instance handles
        // under a brief read, then release the map lock BEFORE taking any
        // per-record write lock — collect-then-act, keeping the invariant
        // "never hold the records-map lock across a per-record lock" uniform
        // across the codebase (a7f5a74f). This is defensive: no current path
        // takes the per-record lock then the records-map lock, so there is no
        // confirmed cycle; uniform order forecloses one. Same idiom as
        // `all_record_names`. (The registry write lock was released above.)
        let instances: Vec<_> = self.inner.records.read().values().cloned().collect();
        for inst in instances {
            inst.write()
                .record
                .install_breaktable_registry(snapshot.clone());
        }
        snapshot
    }

    /// Install the by-name subroutine registry, retained for runtime
    /// re-resolution (aSub LFLG=READ / SUBL). Called once at iocInit with the
    /// IocApp/IocBuilder registry. See `Self::find_subroutine_named`.
    pub async fn install_subroutine_registry(
        &self,
        registry: HashMap<String, Arc<crate::server::record::SubroutineFn>>,
    ) {
        self.inner.subroutine_registry.store(Arc::new(registry));
    }

    /// Install the process-wide device support table — C's registrars filling
    /// `devList` before `iocInit`. Every record created after this line binds
    /// its dset from it at creation, which is C's `doInitRecord0` order; a
    /// record created BEFORE it keeps no device support, exactly as a C record
    /// loaded before its `device()` lines would.
    pub fn install_device_support_resolver(
        &self,
        resolver: crate::server::ioc_app::DeviceSupportResolver,
    ) {
        self.inner
            .device_support_resolver
            .store(Some(Arc::new(resolver)));
    }

    /// How many records hold device support — the `iocInit: N records, M with
    /// device support` count. Derived from the records themselves rather than
    /// tallied by a wiring pass, because there is no longer a wiring pass:
    /// the bind happens at each record's creation.
    pub(crate) async fn records_with_device_support(&self) -> usize {
        let names = self.all_record_names().await;
        names
            .iter()
            .filter_map(|name| self.get_record(name))
            .filter(|rec| rec.read().device.is_some())
            .count()
    }

    /// Look up a registered subroutine by name. The processing path uses this
    /// to re-resolve an aSub's subroutine when SNAM changes (C `fetch_values`
    /// `registryFunctionFind`). `None` when the name is not registered, which
    /// the caller treats as C's `S_db_BadSub` (skip running the subroutine).
    pub(crate) fn find_subroutine_named(
        &self,
        name: &str,
    ) -> Option<Arc<crate::server::record::SubroutineFn>> {
        self.inner.subroutine_registry.load().get(name).cloned()
    }

    /// Every registered subroutine, as `(name, entry address)`, unordered —
    /// the enumeration behind `registryDump`, whose C counterpart walks the
    /// one gpHash every registry shares (`registry.c:86-91` calling
    /// `gphDump`). [`Self::find_subroutine_named`] is the by-name half.
    ///
    /// C has a SECOND list of the same subroutines and this is deliberately
    /// not it. `registryFunctionAdd` puts a name and a function pointer in
    /// the runtime gpHash (`registryFunction.c:22-26`), while the `.dbd`
    /// parser's `dbFunction` (`dbLexRoutines.c:926-947`) appends the name
    /// alone to `pdbbase->functionList`, an `ELLLIST` of `dbText` that
    /// `dbWriteFunctionFP`
    /// (`dbStaticLib.c:1121-1134`, reached from `dbDumpFunction` at
    /// `:3515-3522`) walks in declaration order. The port collapsed both onto
    /// `subroutine_registry`, so the two views come off one map: this one
    /// carries the addresses the gpHash has and the names in no order,
    /// `registered_subroutine_names` carries the names alone. That is two
    /// accessors for two C structures, not two spellings of one — and the
    /// sort there stands in for a declaration order a `HashMap` cannot keep.
    pub(crate) fn subroutine_entries(&self) -> Vec<(String, usize)> {
        self.inner
            .subroutine_registry
            .load()
            .iter()
            .map(|(name, func)| (name.clone(), Arc::as_ptr(func) as *const () as usize))
            .collect()
    }

    /// Atomically claim the right to start the scan scheduler for this DB.
    /// Returns `true` on the first call, `false` on subsequent calls.
    /// Used by `ScanScheduler::run` to prevent duplicate scan tasks
    /// when multiple protocol servers (CA + PVA) both try to start scanning.
    pub fn try_claim_scan_start(&self) -> bool {
        self.inner
            .scan_started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// Mark PINI processing complete. Wakes any non-owner scan schedulers
    /// that were waiting before running their hooks.
    pub fn mark_pini_done(&self) {
        self.inner
            .pini_done
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.pini_notify.notify_waiters();
    }

    /// True once the PINI=YES pass has completed for this database —
    /// published by [`Self::mark_pini_done`]. The scan owner reads this
    /// to keep the pass exactly-once (C `initialProcess`, iocInit.c:653
    /// runs once, inside iocBuild): when the IOC init path already ran
    /// PINI, the owner skips its own pass instead of re-processing every
    /// PINI record.
    pub fn pini_done(&self) -> bool {
        self.inner
            .pini_done
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Wait until the scan owner has completed PINI processing.
    /// Returns immediately if PINI has already completed.
    pub async fn wait_for_pini(&self) {
        if self
            .inner
            .pini_done
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Register interest BEFORE re-checking the flag to avoid missing a
        // signal that arrives between the load and the await — `notify_waiters`
        // does not store a permit for late subscribers.
        let notified = self.inner.pini_notify.notified();
        if self
            .inner
            .pini_done
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        notified.await;
    }

    /// Install an async resolver invoked when [`PvDatabase::has_name`]
    /// fails to find a name. Used by proxy/gateway implementations to
    /// lazily populate PVs on first search.
    pub async fn set_search_resolver(&self, resolver: SearchResolver) {
        self.inner.search_resolver.store(Some(Arc::new(resolver)));
    }

    /// Remove the previously installed search resolver, if any.
    pub async fn clear_search_resolver(&self) {
        self.inner.search_resolver.store(None);
    }

    /// Install the per-request existence gate (see [`ExistenceGate`]).
    /// Replaces any previously installed gate. Used by the CA gateway so
    /// a cached shadow PV re-runs host/state admission per request.
    pub async fn set_existence_gate(&self, gate: ExistenceGate) {
        self.inner.existence_gate.store(Some(Arc::new(gate)));
    }

    /// Remove the previously installed existence gate, if any.
    pub async fn clear_existence_gate(&self) {
        self.inner.existence_gate.store(None);
    }

    /// True when a cached simple PV named `name` must be treated as
    /// non-existent for `peer` because the installed [`ExistenceGate`]
    /// denied it. Always `false` when no gate is installed (a plain IOC)
    /// or when `name` does not resolve to a simple PV — records and
    /// aliases are never gateway-managed and bypass the gate.
    ///
    /// The single consultation point for the gate, shared by
    /// [`Self::find_entry_from`] and [`Self::has_name_from`] so the
    /// "cached simple PV ⇒ exists" short-circuit is closed uniformly on
    /// both the create and search paths.
    async fn simple_pv_gate_denies(&self, name: &str, peer: Option<std::net::SocketAddr>) -> bool {
        let Some(gate) = self.inner.existence_gate.load_full() else {
            return false;
        };
        let gate = (*gate).clone();
        // Strip the channel-filter suffix exactly as the lookups do
        // (CA-FR-8) so the gate sees the same record-path key the
        // simple-PV map and the gateway cache are keyed on.
        let record_path = filters::split_channel_name(name).record_path;
        // Own statement: the `!Send` guard must be down before the gate's
        // `.await` below (see the `simple_pvs` field doc).
        let known = self
            .inner
            .simple_pvs
            .lock()
            .contains_key(record_path.as_str());
        if !known {
            return false;
        }
        !gate(record_path, peer).await
    }

    /// Set an external PV resolver for CA/PVA link resolution.
    /// The resolver is called synchronously from link reads.
    pub async fn set_external_resolver(&self, resolver: ExternalPvResolver) {
        self.inner.external_resolver.store(Some(Arc::new(resolver)));
    }

    /// The database debugger, or `None` when nothing is being debugged.
    ///
    /// The hot-path read behind both breakpoint hooks: C's
    /// `if (lset_stack_count)` guard in `dbProcess`.
    pub(crate) fn breakpoints(&self) -> Option<Arc<breakpoint::BreakpointTable>> {
        self.inner.breakpoints.load_full()
    }

    /// The debugger, installing it on first use — C `dbBkptInit()`
    /// (`dbBkpt.c:254-260`), which `iocInit` calls unconditionally and which
    /// creates the stack semaphore once.
    ///
    /// Owner rule: this is the ONLY place the observer is installed, so a
    /// second `dbb` cannot replace a table that already holds breakpoints and
    /// strand a parked continuation thread behind an unreachable stack.
    pub(crate) fn breakpoints_or_install(&self) -> Arc<breakpoint::BreakpointTable> {
        if let Some(existing) = self.inner.breakpoints.load_full() {
            return existing;
        }
        let fresh = Arc::new(breakpoint::BreakpointTable::new());
        // `compare_and_swap` against the empty state: two concurrent first
        // `dbb`s must agree on one table, and the loser's must be dropped
        // rather than stored.
        let prev = self.inner.breakpoints.compare_and_swap(
            &None::<Arc<breakpoint::BreakpointTable>>,
            Some(fresh.clone()),
        );
        match &*prev {
            Some(winner) => winner.clone(),
            None => fresh,
        }
    }

    /// Drop the debugger once its last lock set is gone, so the hot path goes
    /// back to a `None` load — C's `--lset_stack_count` reaching zero
    /// (`dbBkpt.c:619`).
    pub(crate) fn retire_breakpoints_if_idle(&self) {
        if self
            .inner
            .breakpoints
            .load()
            .as_ref()
            .is_some_and(|t| t.is_empty())
        {
            self.inner.breakpoints.store(None);
        }
    }

    /// Register a [`LinkSet`] under `scheme` (e.g. `"pva"` /
    /// `"ca"`). The lset is consulted for `ParsedLink::Pva` /
    /// `ParsedLink::Ca` link reads/writes before falling back to
    /// the legacy [`ExternalPvResolver`]. Subsequent calls for the
    /// same scheme replace the previous binding.
    pub async fn register_link_set(&self, scheme: &str, lset: link_set::DynLinkSet) {
        self.inner.link_sets.update(|r| r.register(scheme, lset));
    }

    /// Look up the lset for `scheme`, if any.
    pub async fn link_set(&self, scheme: &str) -> Option<link_set::DynLinkSet> {
        self.inner.link_sets.load().get(scheme)
    }

    /// Snapshot of every registered scheme name. Stable order for
    /// `dbpvxr` dumps.
    pub async fn registered_link_schemes(&self) -> Vec<String> {
        let mut s = self.inner.link_sets.load().schemes();
        s.sort();
        s
    }

    /// Wait for the CA links to local records to report
    /// `init_ready() == true` — connected, first monitor event cached,
    /// attribute fetch complete. Mirrors `dbCa: iocInit wait for local CA
    /// links to connect` (epics-base PR #768) as extended by #856's
    /// `testInitReady` all-conditions gate. The working set is
    /// exactly `Self::external_link_targets`: only the CA facility's
    /// local-target links — `pva://` links and non-local CA links connect
    /// in the background and are never waited on (pvxs parity).
    ///
    /// Polls every 100 ms. Returns:
    /// * `Ok(connected_count)` — the number of links that ended up
    ///   connected. May be smaller than the total when the timeout
    ///   expired before everyone was ready.
    /// * The total link count — i.e. the size of the working set
    ///   that was checked. `(connected, total)` lets the caller log
    ///   "M/N CA links connected".
    ///
    /// Pure no-op when no CA link set is registered, or when its
    /// `link_names()` has no local-target link yet (e.g. lazy-open lsets
    /// that haven't observed any record link — record processing creates
    /// the entries on first read, after iocInit returns).
    pub async fn wait_for_external_links(&self, timeout: std::time::Duration) -> (usize, usize) {
        // Collect (lset, name) pairs once. `link_names()` may grow
        // as record processing opens new links, but iocInit's wait
        // is bounded by the records loaded *before* Phase 3 — every
        // such link is already opened by the time wire_device_support
        // and setup_cp_links return.
        let targets = self.external_link_targets().await;
        let total = targets.len();
        if total == 0 {
            return (0, 0);
        }
        // `std::time::Instant`, not `crate::runtime::task::Instant`: the sleep
        // below is the background timer, which measures on std's clock on both
        // backends. Reading the deadline off tokio's clock made the two
        // disagree — under `start_paused` tokio's clock advances only when the
        // runtime decides to, while the timer thread follows the real one, so
        // the loop could sleep against one clock and expire against another.
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut connected = 0usize;
            for (lset, name) in &targets {
                // `init_ready`, not `is_connected`: C `testInitReady`
                // (`dbCa.c:835` at `ef4829829`, epics-base #856 — post-
                // `R7.0.10` and in no tag) releases iocInit only when the
                // link's monitor AND attribute-fetch actions have all
                // completed, not on the bare connection edge.
                if lset.init_ready(name) {
                    connected += 1;
                }
            }
            if connected == total {
                return (connected, total);
            }
            if std::time::Instant::now() >= deadline {
                return (connected, total);
            }
            crate::runtime::task::sleep_background(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Snapshot the `(lset, link_name)` pairs the iocInit external-link
    /// wait reasons over. Shared by [`Self::wait_for_external_links`] and
    /// [`Self::unconnected_external_links`] so both see the identical
    /// working set.
    ///
    /// C parity: the iocInit connection-wait is a property of the CA link
    /// facility (dbCa) alone. The whole connection-wait is post-`R7.0.10`
    /// and in no tag, so these numbers are `717d69e1f`'s, not the pin's —
    /// at the pin `dbCaRun` is `dbCa.c:357-363` and does not spin at all.
    /// `dbCaRun` (`dbCa.c:370-380`) spins at `:376-378` on
    /// `initOutstanding`, which `dbCaAddLinkCallbackOpt` increments
    /// (`dbCa.c:408-409`) for every link that arrives carrying the
    /// `DBCA_CALLBACK_INIT_WAITING` bit. `dbInitLink` passes that bit — as
    /// part of `DBCA_CALLBACK_INIT_START` (0xf, `dbCaPvt.h:118`) — only for
    /// a CA link whose target is a LOCAL record (dbLink.c:128, :130):
    ///   int isLocal = dbChannelTest(pvname) == 0;
    ///   dbCaAddLinkCallbackOpt(..., isLocal ? DBCA_CALLBACK_INIT_START : 0)
    ///
    /// **Unreleased upstream.** None of that exists at the reference pin.
    /// R7.0.10 carries the REVERT of the first attempt (3f382f6b6), so its
    /// `dbLink.c:128` is a bare `dbCaAddLink(NULL, plink, dbfType);` and its
    /// `dbCa.c` has no `initOutstanding` at all; the wait re-landed in
    /// 717d69e1f and ef4829829, which are in no tag. This function
    /// therefore tracks epics-base HEAD, not the pin — a C IOC built from
    /// R7.0.10 does not wait for local CA links.
    ///
    /// No other external facility waits: pvxs pvalink's `linkGlobal_t::init`
    /// (ioc/pvalink.cpp) only calls `chan->open()` per channel — it opens
    /// in the background and never blocks iocInit. So the wait targets
    /// exactly the CA link set's local-target links; a non-local CA link
    /// (e.g. areaDetector's `ShutterStatusEPICS_RBV.INP = "test CP MS"`
    /// placeholder) and every `pva://` link connect asynchronously and are
    /// never held by iocInit, like C.
    async fn external_link_targets(&self) -> Vec<(link_set::DynLinkSet, String)> {
        // Only the CA facility participates — look it up directly rather
        // than iterating every registered scheme. `has_name_no_resolve`
        // is the `dbChannelTest` twin (target is a local record).
        let Some(ca_lset) = self.inner.link_sets.load().get("ca") else {
            return Vec::new();
        };
        let mut targets: Vec<(link_set::DynLinkSet, String)> = Vec::new();
        for n in ca_lset.link_names() {
            if self.has_name_no_resolve(&n) {
                targets.push((ca_lset.clone(), n));
            }
        }
        targets
    }

    /// Names of the waited-on CA links (local-target, per
    /// `Self::external_link_targets`) that are opened but not yet
    /// connected. iocInit calls this after
    /// [`Self::wait_for_external_links`] times out so the
    /// "M/N connected" diagnostic can name the `N-M` it proceeded
    /// without, instead of leaving the operator to run `dbcar`.
    /// `pva://` links are not in this set — they never block iocInit.
    pub async fn unconnected_external_links(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (lset, name) in self.external_link_targets().await {
            // Same predicate as the wait loop, so the M/N accounting and
            // this diagnostic agree on which links held iocInit.
            if !lset.init_ready(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Every link field the record HAS, with its raw text and the link-field
    /// type it must be parsed as — the single owner of *which fields on a
    /// record are links*.
    ///
    /// Keyed on the `.dbd` declaration through
    /// [`crate::types::dbf_link_class`], which is the same rule
    /// `check_link_put` gates a put with, so "is this field a link" has one
    /// answer for every caller. It used to be three name lists —
    /// `COMMON_LINK_FIELDS`, `Record::multi_input_links` and
    /// `CP_INPUT_LINK_FIELDS` — and everything they did not spell was
    /// invisible: a `fanout`'s `LNK1..LNK6`, a `dfanout`'s `OUTA..OUTH`, an
    /// `ai`'s `SIML`/`SIOL`, an `aSub`'s `SUBL` and `OUTA..OUTU`, a `seq`'s
    /// `LNK1..LNKA`. Measured against softIoc R7.0.10 with `dblsr`, that cost
    /// five of seven probed record shapes their lock-set members — a `fanout`
    /// whose `LNK1` names a local record shared C's set with it and the port's
    /// did not.
    ///
    /// Unfiltered on purpose. [`Self::record_link_fields`] narrows this to the
    /// fields that carry a parseable link and maps them through the locality
    /// fallthrough, which is what its consumers want; C's `iocInit` pass
    /// ([`Self::db_init_record_links`]) needs the fields with no text too,
    /// because it types a link from the record's device support whether the
    /// `.db` spelled it or not (`dbStaticLib.c:2185-2212`).
    fn link_field_texts(
        inst: &RecordInstance,
    ) -> Vec<(String, String, crate::server::record::LinkFieldType)> {
        use crate::server::record::LinkFieldType;
        use crate::types::DbfLinkClass;
        let record_type = inst.record.record_type();
        let mut out: Vec<(String, String, LinkFieldType)> = Vec::new();
        // `dbCommon` first, then the record's own, which is C's `papFldDes`
        // order; a record type that redeclares a `dbCommon` field keeps the
        // first entry, since that is the one `declared_field` resolves.
        for desc in crate::server::record::declared_fields(record_type) {
            let ftype = match crate::types::dbf_link_class(record_type, desc.name) {
                Some(DbfLinkClass::InLink) => LinkFieldType::In,
                Some(DbfLinkClass::OutLink) => LinkFieldType::Out,
                Some(DbfLinkClass::FwdLink) => LinkFieldType::Fwd,
                None => continue,
            };
            if out.iter().any(|(had, _, _)| had == desc.name) {
                continue;
            }
            // `INP`/`OUT`/`TSEL`/`SDIS`/`FLNK` live on `CommonFields` as raw
            // String and are absent from `field_list()`, so the declaration
            // says they exist and only the instance can say what they hold.
            let raw = match inst.common_link_text(desc.name) {
                Some(raw) => raw.to_string(),
                None => match inst.record.get_field(desc.name) {
                    Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
                    // Declared but not served by this instance's rset — a
                    // downstream type whose generated table outruns its
                    // `get_field`. C would have the `dbFldDes` and an empty
                    // link; the port has no text to offer, so the field is
                    // simply absent rather than reported as an empty link.
                    _ => continue,
                },
            };
            out.push((desc.name.to_string(), raw, ftype));
        }
        out
    }

    /// Enumerate every link-shaped field on `record_name`. Returns
    /// `(field_name, link_string, parsed)` tuples for fields whose
    /// raw value parses as a non-trivial link via
    /// [`crate::server::record::parse_link_v2`]. Used by `dbpvxr` to
    /// dump per-record link state without hardcoding the field-name
    /// list — works across record types as long as they expose link
    /// strings via [`Record::get_field`].
    ///
    /// `parsed` is the **post-`dbInitLink`** view, not the bare parse: each
    /// link is mapped through `db_init_link_locality`, so a
    /// `Db` link naming a record this IOC does not have is reported as the
    /// `Ca` link C's `dbDbInitLink` → `dbCaAddLink` fallthrough makes it
    /// (`dbLink.c:118-130`, `dbDbLink.c:94-96`). Every consumer — the CP
    /// setup, the init open pass, `dbcaxr`, the pvalink install scan — then
    /// sees one consistent answer to "is this link local or external"
    /// instead of each re-deriving it. `link_string` is still the verbatim
    /// field text.
    ///
    /// Each field is parsed for ITS OWN link-field type: C `dbPutFieldLink`
    /// passes `pfldDes->field_type` to `dbParseLink` (`dbAccess.c:1094`), which
    /// then masks the modifiers by that type (`dbStaticLib.c:2380-2391`). `OUT`
    /// is `DBF_OUTLINK`, so its CP/CPP is discarded here rather than reaching
    /// `setup_cp_links` — an `OUT` link must never be registered as a CP holder.
    ///
    /// Returns an empty Vec when the record doesn't exist.
    pub fn record_link_fields(
        &self,
        record_name: &str,
    ) -> Vec<(String, String, crate::server::record::ParsedLink)> {
        let rec = match self.get_record(record_name) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let mut out: Vec<(String, String, crate::server::record::ParsedLink)> =
            Self::link_field_texts(&rec.read())
                .into_iter()
                .filter(|(_, raw, _)| !raw.is_empty())
                .map(|(field, raw, ftype)| {
                    let parsed = crate::server::record::parse_link_field(&raw, ftype);
                    (field, raw, parsed)
                })
                .filter(|(_, _, parsed)| !matches!(parsed, crate::server::record::ParsedLink::None))
                .collect();
        // Apply C `dbInitLink`'s locality fallthrough once, here, so no
        // consumer re-derives it. Done after dropping the record-instance
        // guard: the locality query reads the database's record map, and
        // this is the only place that would otherwise hold an instance lock
        // across it.
        for entry in &mut out {
            entry.2 = self.db_init_link_locality(std::mem::replace(
                &mut entry.2,
                crate::server::record::ParsedLink::None,
            ));
        }
        out
    }

    /// Resolve an external PV name. Dispatches through the
    /// `(scheme, name)` lset if one is registered; otherwise falls
    /// back to the legacy [`ExternalPvResolver`] closure. `name`
    /// may be the bare PV name (in which case `pva://` is assumed
    /// when an lset is registered for that scheme) or a fully
    /// scheme-prefixed string.
    ///
    /// # Cached read, then a staged open — C `dbCaGetLink`
    ///
    /// This is the record-processing read, so it reads the lset's
    /// monitor-fed cache ([`LinkSet::get_cached_value`]) and never the
    /// network: C `dbCaGetLink` (`dbCa.c:419-506`) copies out of
    /// `pca->pgetNative`, which the CA monitor callback keeps fresh on the
    /// `dbCaTask`, and returns -1 while the link is down (`dbCa.c:430-435`).
    ///
    /// A miss stages the link's OPEN on the same work queue the OUT writes
    /// use — the `addAction(pca, CA_CONNECT)` C reaches through
    /// `dbCaAddLink` (`dbCa.c:397-401`), which is `dbCa.c:393` inside
    /// `dbCaAddLinkCallback` (`:373-395`) — and returns `None` for this
    /// cycle.
    /// C's open happens at record init rather than at
    /// first read, but in both designs the connect runs on the link task and
    /// the reading record takes LINK/INVALID until the cache is warm.
    pub(crate) fn resolve_external_pv(&self, name: &str) -> Option<EpicsValue> {
        // Try lsets first. We accept both "scheme://body" and the
        // bare body (stored in ParsedLink::Pva/Ca after the
        // dispatch in record/link.rs). `Any` tries every registered
        // lset in turn; the first one with a cached value wins.
        let (target, body) = Self::split_external_link_name(name);
        for lset in link_put_queue::resolve_lsets(&self.inner, &target) {
            if let Some(v) = lset.get_cached_value(body) {
                return Some(v);
            }
        }
        self.stage_external_link_open_by_name(name);
        // Fall through to legacy resolver, which is addressed with the
        // caller's string verbatim (scheme prefix and all).
        let resolver = self
            .inner
            .external_resolver
            .load_full()
            .map(|r| (*r).clone());
        match resolver {
            Some(r) => r(name),
            None => None,
        }
    }

    /// Split an external link's boundary name
    /// ([`crate::server::record::ParsedLink::external_pv_name`]) into the
    /// work-queue target plus the name the lset is addressed with.
    ///
    /// Single owner of the `ca://` / `pva://` prefix convention: the
    /// cache-miss stage in `resolve_external_pv` and the iocInit
    /// open pass ([`PvDatabase::setup_external_link_opens`]) must derive
    /// the same [`link_put_queue::LinkKey`] from the same link, or the
    /// queue's once-per-link open would fire twice under two spellings.
    fn split_external_link_name(name: &str) -> (link_put_queue::LinkTarget, &str) {
        if let Some(rest) = name.strip_prefix("pva://") {
            (link_put_queue::LinkTarget::Scheme("pva".to_string()), rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            (link_put_queue::LinkTarget::Scheme("ca".to_string()), rest)
        } else {
            (link_put_queue::LinkTarget::Any, name)
        }
    }

    /// Stage the open of the external link named `name` — the boundary
    /// form of [`Self::stage_external_link_open`], which splits the
    /// scheme prefix and applies the "an lset must exist" gate.
    ///
    /// The gate matters because the queue's open state is terminal: an
    /// open staged while no lset is registered would be serviced against
    /// an empty lset list and marked `Done`, burning the link's one and
    /// only connect. Returns true when this call is the one that staged
    /// it (false when already staged, or when no lset addresses it).
    pub(crate) fn stage_external_link_open_by_name(&self, name: &str) -> bool {
        let (target, body) = Self::split_external_link_name(name);
        if link_put_queue::resolve_lsets(&self.inner, &target).is_empty() {
            return false;
        }
        self.stage_external_link_open(target, body)
    }

    /// How many staged writes to the external link `link_name` a later write
    /// on the same link overwrote before it reached the wire — C's
    /// `pca->nNoWrite`, which `dbcar` prints beside the link
    /// (`dbCaTest.c:111-116`).
    ///
    /// `link_name` is the link string as a record carries it, with or
    /// without a `ca://` / `pva://` prefix; it is split by
    /// `Self::split_external_link_name`, the same owner of that convention
    /// the write path stages under, so the two cannot key the counter
    /// differently.
    ///
    /// C keeps this per `caLink`, i.e. per link FIELD: two records whose OUT
    /// links name one PV have two `caLink`s and two counters. This port keeps
    /// one queue entry per `(scheme, PV name)`, so both fields report the
    /// shared count — the same aliasing `LinkSet` already applies to the
    /// link's cache and connection state.
    pub fn external_link_puts_coalesced_for(&self, link_name: &str) -> u64 {
        let (target, body) = Self::split_external_link_name(link_name);
        self.inner
            .link_puts
            .coalesced_for(&link_put_queue::LinkKey {
                target,
                name: body.to_string(),
            })
    }

    /// C `dbCaLinkInit` (`dbCa.c:322-346`), which `iocBuild` calls before it
    /// announces `initHookAfterCaLinkInit`: the `dbCaLink` worker exists from
    /// init, not from the first external link put. Otherwise an IOC whose
    /// output links are all local has no `dbCaLink` task where C always has
    /// one — visible in `taskwdShow`.
    ///
    /// Returns false, having started nothing, when this database captured no
    /// reactor: the owner's network work has nowhere to run, which is the same
    /// condition [`Self::stage_external_link_open`] already refuses on.
    pub(crate) fn ca_link_init(&self) -> bool {
        if self.inner.link_puts.network().is_none() {
            return false;
        }
        self.inner
            .link_puts
            .ensure_owner(std::sync::Arc::downgrade(&self.inner));
        true
    }

    /// Single owner of the "this external link needs opening" transition —
    /// the `addAction(pca, CA_CONNECT)` C reaches through `dbCaAddLink`
    /// (`dbCa.c:397-401`), which is `dbCa.c:393` inside
    /// `dbCaAddLinkCallback` (`:373-395`).
    ///
    /// Every caller routes through here so the open runs on the link work
    /// owner and nowhere else; no path may call
    /// [`link_set::LinkSet::connect_link`] directly from a record-processing
    /// thread. Cheap and idempotent: the queue drops a repeat stage for a
    /// link it has already opened.
    ///
    /// Private to the `database` module, and reached only through
    /// [`Self::stage_external_link_open_by_name`], so no caller can skip the
    /// scheme split or the lset gate and mint a `LinkKey` of its own shape.
    fn stage_external_link_open(&self, target: link_put_queue::LinkTarget, name: &str) -> bool {
        // Nothing is staged that no owner can perform. A database built with
        // no tokio runtime captured no reactor, and `connect_link` has nowhere
        // to run (see `LinkPutQueue::network`) — so refuse here rather than
        // enqueue an open the owner would have to drop. The link stays
        // unopened, its reads keep returning `None`, and the reading record
        // takes LINK/INVALID every cycle, which is C's `!pca->isConnected`
        // arm (`db/dbCa.c:430-435` (`dbCaGetLink`); epics-base R7.0.10).
        if self.inner.link_puts.network().is_none() {
            return false;
        }
        self.inner
            .link_puts
            .ensure_owner(std::sync::Arc::downgrade(&self.inner));
        self.inner.link_puts.stage_open(link_put_queue::LinkKey {
            target,
            name: name.to_string(),
        })
    }

    /// Number of external-link opens the work owner has completed —
    /// diagnostic twin of [`Self::external_link_puts_completed`].
    pub fn external_link_opens_completed(&self) -> u64 {
        self.inner.link_puts.opened_count()
    }

    /// Add a simple PV with an initial value.
    ///
    /// Returns `Err` when `name` is already registered as a simple PV,
    /// a record, or an alias — mirroring epics-base C IOC which treats
    /// duplicate `dbLoadRecords` names as a fatal error. Callers that
    /// want replace-on-overwrite semantics must first call
    /// `remove_simple_pv` / `remove_record` / `remove_alias`.
    ///
    /// Serialized through `registration_mutex` so the
    /// cross-namespace check is atomic with the insert and the lock
    /// order across all add_*/remove_* methods is identical (no
    /// cross-namespace deadlock).
    pub async fn add_pv(&self, name: &str, initial: EpicsValue) -> CaResult<()> {
        let _gate = self.lock_registration("add_pv");
        self.check_name_free(name)?;
        let pv = Arc::new(ProcessVariable::new(name.to_string(), initial));
        self.inner.simple_pvs.lock().insert(name.to_string(), pv);
        Ok(())
    }

    /// Add a simple PV that already has a [`crate::server::pv::WriteHook`] installed.
    ///
    /// Equivalent to `add_pv` followed by `find_pv` + `set_write_hook`,
    /// but the PV is constructed with the hook in place so it is
    /// inserted into the `simple_pvs` map ATOMICALLY with the hook
    /// already attached. Closes a small race in proxy/gateway code
    /// where a downstream client could (in principle) `CREATE_CHAN` +
    /// `WRITE_NOTIFY` between the two awaits and hit the local
    /// `pv.set()` fallback path before the hook landed.
    ///
    /// Returns `Err` on duplicate name (see [`Self::add_pv`]).
    pub async fn add_pv_with_hook(
        &self,
        name: &str,
        initial: EpicsValue,
        hook: crate::server::pv::WriteHook,
    ) -> CaResult<()> {
        self.add_pv_with_hooks(name, initial, hook, None).await
    }

    /// like [`Self::add_pv_with_hook`] but also installs an
    /// optional [`AccessHook`](crate::server::pv::AccessHook) so the CA
    /// gateway can route this shadow PV's read/write access-rights
    /// decision through its own ACF. Both hooks are attached before the
    /// PV is inserted into `simple_pvs`, so a downstream `CREATE_CHAN`
    /// cannot observe the PV without its access hook bound.
    pub async fn add_pv_with_hooks(
        &self,
        name: &str,
        initial: EpicsValue,
        write_hook: crate::server::pv::WriteHook,
        access_hook: Option<crate::server::pv::AccessHook>,
    ) -> CaResult<()> {
        self.add_pv_with_hooks_full(name, initial, write_hook, access_hook, None)
            .await
    }

    /// like [`Self::add_pv_with_hooks`] but also installs an optional
    /// [`ReadHook`](crate::server::pv::ReadHook) so a proxy (the CA
    /// gateway in no-cache mode) can serve each downstream GET from a
    /// fresh upstream fetch instead of the stored value. All three hooks
    /// are attached before the PV is inserted into `simple_pvs`, so a
    /// downstream `CREATE_CHAN` cannot observe the PV without its hooks
    /// bound — the read hook lands atomically with registration, closing
    /// the same race the write/access hooks already close. `read_hook:
    /// None` is identical to [`Self::add_pv_with_hooks`].
    pub async fn add_pv_with_hooks_full(
        &self,
        name: &str,
        initial: EpicsValue,
        write_hook: crate::server::pv::WriteHook,
        access_hook: Option<crate::server::pv::AccessHook>,
        read_hook: Option<crate::server::pv::ReadHook>,
    ) -> CaResult<()> {
        let _gate = self.lock_registration("add_pv_with_hooks_full");
        self.check_name_free(name)?;
        let pv = Arc::new(ProcessVariable::new(name.to_string(), initial));
        pv.set_write_hook(write_hook);
        if let Some(access) = access_hook {
            pv.set_access_hook(access);
        }
        if let Some(read) = read_hook {
            pv.set_read_hook(read);
        }
        self.inner.simple_pvs.lock().insert(name.to_string(), pv);
        Ok(())
    }

    /// Remove a simple PV by name. Returns `Some(pv)` if a PV was
    /// removed. Used by the gateway sweep so an evicted upstream
    /// subscription doesn't leave a stale shadow PV (with a now-dead
    /// `WriteHook` capturing an aborted upstream channel).
    ///
    /// Also purges any aliases that pointed AT this name
    /// (otherwise a re-add of the same alias name would fail with
    /// "already registered as an alias" even though its target is
    /// gone).
    pub async fn remove_simple_pv(&self, name: &str) -> Option<Arc<ProcessVariable>> {
        let _gate = self.lock_registration("remove_simple_pv");
        // Simple PVs cannot be alias targets (aliases point at
        // records), but a stale alias whose name MATCHES this PV
        // would have been rejected at add_alias time. No alias
        // cleanup needed for simple-PV removal.
        let removed = self.inner.simple_pvs.lock().remove(name);
        if let Some(pv) = &removed {
            // Removal IS destruction: this funnel is `destroy`'s only
            // caller, so a PV cannot leave the directory while a server
            // still serves it. C ca-gateway does the same on upstream
            // death — `delete vc` (gatePv.cc:601), the downstream monitors
            // stop rather than take one more frame.
            pv.destroy();
            self.signal_destroyed();
        }
        removed
    }

    /// Register an already-built [`ProcessVariable`] under its own name.
    ///
    /// The `add_pv*` family builds the PV from parts; this one takes a PV
    /// the caller already holds, which is what a proxy re-installing a
    /// [`ProcessVariable::respawn`]ed replacement needs — the hooks and
    /// shadow metadata travel on the object instead of being re-derived at
    /// every call site.
    pub async fn add_simple_pv(&self, pv: ProcessVariable) -> CaResult<()> {
        let _gate = self.lock_registration("add_simple_pv");
        self.check_name_free(&pv.name)?;
        let name = pv.name.clone();
        self.inner.simple_pvs.lock().insert(name, Arc::new(pv));
        Ok(())
    }

    /// Subscribe to the "something was removed" wake-up.
    ///
    /// A server that holds channels on this database races this against its
    /// socket read and sweeps out every channel whose target now answers
    /// `is_destroyed()`. Lagging is harmless: one missed wake still means
    /// "re-check", which is what the sweep does.
    pub fn pv_destroyed_events(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.inner.pv_destroyed_tx.subscribe()
    }

    /// Fire the wake-up. A send error means no server is attached, which is
    /// the normal state for a bare in-process database.
    fn signal_destroyed(&self) {
        let _ = self.inner.pv_destroyed_tx.send(());
    }

    /// Enter the LOAD phase: records are being created and the database is not
    /// yet the one C would classify links against. Called by every path that
    /// begins creating records for an IOC — an `IocBuilder` build, an iocsh
    /// `dbLoadRecords` / `dbCreateRecord`, `IocApp::run` — and idempotent within
    /// the phase, because an `st.cmd` issues several loads and they are all one
    /// `iocInit` (R18-92).
    ///
    /// # Refused once the IOC is running (R19-63)
    ///
    /// C admits no record creation after `iocInit`: `dbReadCOM`
    /// (`dbLexRoutines.c:236`) fails every `.db`/`.dbd` read with `-2` once
    /// `getIocState() != iocVoid`, and `dbCreateRecordCallFunc`
    /// (`dbStaticIocRegister.c:288-291` at `f4ccf7bc8`, a command no release
    /// tag carries) fails with `S_dbLib_postInitRecRegister`.
    /// Asking to create records IS asking to enter the load phase, so the answer
    /// lives here and is a `Result` the caller cannot ignore — a creator that
    /// never asked cannot be written by accident, and one that asked cannot
    /// proceed on a refusal.
    ///
    /// The phase is left ONLY by [`Self::ioc_init`], and once left it is
    /// TERMINAL (R19-62): the queue is drained by exactly one `ioc_init`, so
    /// nothing can be pushed into it afterwards and stranded. A load that fails
    /// halfway leaves the phase open, which strands nothing: a queued
    /// classification blocks no caller, and it is dropped with the database.
    #[must_use = "C refuses a load after iocInit (dbReadCOM, dbLexRoutines.c:236); \
                  the refusal must be reported and no record created"]
    pub fn begin_load(&self) -> Result<(), IocAlreadyInitialized> {
        let mut phase = self.inner.init_phase.lock().unwrap();
        match *phase {
            // The only producer of `Loading`.
            DbInitPhase::Unloaded => {
                *phase = DbInitPhase::Loading(Vec::new());
                Ok(())
            }
            // An `st.cmd` issues several loads; they are all one `iocInit`.
            DbInitPhase::Loading(_) => Ok(()),
            // The barrier has begun, or has finished. Refused, as C refuses
            // a load once `iocInit` is under way.
            DbInitPhase::Initialising(_) | DbInitPhase::Running => Err(IocAlreadyInitialized),
        }
    }

    /// Has [`Self::ioc_init`] run?
    ///
    /// C's `dbtr` / `dbtgf` / `dbtpf` refuse to touch a record whose
    /// `lset` is still NULL — the field `iocInit` fills — each printing
    /// `<its own name> only works after iocInit` and returning −1
    /// (`dbTest.c:476-478`, `:520-522`, `:621-623`). This is the port's
    /// twin of that test: the phase is the one owner of "has iocInit
    /// run", so the shell asks it rather than probing a record for an
    /// initialised-only side effect.
    pub fn ioc_is_running(&self) -> bool {
        matches!(*self.inner.init_phase.lock().unwrap(), DbInitPhase::Running)
    }

    /// Schedule a record's link-status classification — the port's
    /// `init_record` tail (C `checkLinks`).
    ///
    /// During the LOAD phase the future is QUEUED for [`Self::ioc_init`]; a
    /// half-built database cannot be classified against because the code that
    /// would do it has not been polled. Before any load, and once `iocInit` has
    /// run, it is spawned at once — which is what a runtime `special()` link
    /// re-point needs.
    ///
    /// # `record` is what the init classifies, and it must exist first
    ///
    /// A record's first classification is issued from `set_async_context`,
    /// which [`Self::add_loaded_record`] calls *before* it inserts the record
    /// into `records` — the handle has to exist for `run_init_passes` to use
    /// it. So on the spawn-at-once paths the init would race its own record's
    /// registration and could post fields for a record the database does not
    /// have yet.
    ///
    /// This used to be papered over by starting each such future with a
    /// `crate::runtime::task::yield_now()`, on the assumption that yielding
    /// hands the thread back to the in-progress `add_record`. That assumption
    /// holds only on a current-thread runtime: on a multi-thread one the yield
    /// can return before the insert lands, and under `exec_backend` the init
    /// runs on the background executor's own thread, where a yield is not a
    /// synchronisation with `add_record` at all — it is nothing. The tests
    /// that read a link-status field right after `add_record` therefore failed
    /// by timing, with a different test failing per run.
    ///
    /// Now the ordering is a property of the data: an init naming a record that
    /// is not registered is *parked* under that name, and the only thing that
    /// can release it is the insert of that name. No yield, no window, and the
    /// same code path on every backend.
    pub(crate) fn schedule_record_init(
        &self,
        record: &str,
        init: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        if !self.inner.records.read().contains_key(record) {
            self.inner
                .record_init_waiting
                .lock()
                .unwrap()
                .entry(record.to_string())
                .or_default()
                .push(Box::pin(init));
            return;
        }
        self.dispatch_record_init(Box::pin(init));
    }

    /// Queue or spawn an init whose record is known to be registered.
    fn dispatch_record_init(&self, init: RecordInit) {
        let mut phase = self.inner.init_phase.lock().unwrap();
        match &mut *phase {
            DbInitPhase::Loading(queued) | DbInitPhase::Initialising(queued) => queued.push(init),
            DbInitPhase::Unloaded | DbInitPhase::Running => {
                drop(phase);
                // Middle band, not the record's PRIO: C runs `init_record`
                // inline on the `iocInit` thread (`dbInitRecord`,
                // dbAccess.c), never on a callback queue, so there is no
                // per-record band for record init to inherit.
                crate::runtime::task::spawn_background(
                    crate::runtime::task::CallbackPriority::Medium,
                    init,
                );
            }
        }
    }

    /// Release every init parked for `record` — called by the one site that
    /// registers the name, immediately after the insert that makes it real.
    fn release_record_inits(&self, record: &str) {
        let parked = self
            .inner
            .record_init_waiting
            .lock()
            .unwrap()
            .remove(record);
        for init in parked.into_iter().flatten() {
            self.dispatch_record_init(init);
        }
    }

    /// The `iocInit` barrier: end the LOAD phase and run every classification
    /// owed, to completion.
    ///
    /// After this returns the database is complete and every link status is
    /// FINAL — C's guarantee, where `init_record` runs inside `iocInit` and a
    /// `dbgf REC.INAV` right after it reads the classified value (before it, C
    /// refuses `dbgf` outright). Idempotent: an `st.cmd` that spells `iocInit`
    /// out and the `IocApp` that runs one anyway are the same single boundary.
    pub async fn ioc_init(&self) {
        let mut owed = {
            let mut phase = self.inner.init_phase.lock().unwrap();
            match std::mem::replace(&mut *phase, DbInitPhase::Initialising(Vec::new())) {
                DbInitPhase::Loading(queued) => queued,
                // An IOC that loaded nothing (programmatic / unit-test
                // database) still crosses the barrier: the phase becomes
                // terminal. It owes no per-record init pass.
                DbInitPhase::Unloaded => Vec::new(),
                // Already claimed or already finished — put back what was
                // taken and leave.
                already @ (DbInitPhase::Initialising(_) | DbInitPhase::Running) => {
                    *phase = already;
                    return;
                }
            }
        };
        // C `iocBuild` calls `dbCaLinkInit()` between its two halves
        // (`iocInit.c:216`), so the `dbCaLink` worker is up before the record
        // init passes of `iocBuild_2` — and before anything they wire can
        // stage a link. Here rather than in `ioc_app`'s lifecycle because this
        // barrier is the one point every bring-up path crosses: the
        // `IocApplication` walk and `CaServer::run` both reach it, and only
        // one of them runs that lifecycle.
        self.ca_link_init();
        // C's `dbInitRecordLinks`: every link is typed, checked and opened at
        // `iocInit`, not at `dbLoadRecords`. That is where a `{state:"NAME"}`
        // link's `dbStateCreate` happens — so its state exists from `iocInit`
        // onward whether or not anything ever reads it — and it is also where
        // a link the record's device support cannot take is refused, with the
        // record kept. Measured on softIoc R7.0.10: `dbStateShowAll 1` prints
        // nothing after `dbLoadRecords` alone and lists every name any link
        // mentions after `iocInit`, all FALSE, with nothing processed.
        //
        // Creating a state on first use alone — which the link read/write
        // owner still does, because a `dbLoadRecords` AFTER `iocInit` opens
        // its links then too — agrees on every value but left the registry
        // empty until a link was touched.
        self.db_init_record_links().await;
        // The pass is done and every link's text is final, so the phase becomes
        // terminal here — and what the pass queued joins the load's own
        // backlog, behind it, so a record classified twice publishes the later
        // verdict.
        {
            let mut phase = self.inner.init_phase.lock().unwrap();
            if let DbInitPhase::Initialising(queued) =
                std::mem::replace(&mut *phase, DbInitPhase::Running)
            {
                owed.extend(queued);
            }
        }
        // Sequential, in issue order: each classification is a short read of a
        // now-immutable record set, and C's `init_record` pass is a loop too.
        for init in owed {
            init.await;
        }
        // C `dbLockInitRecords` plus the merges `initDatabase` drives as it
        // opens each DB link (`iocInit.c:178-179`). It runs after the per-record
        // init pass for the same reason C runs it after `prepareLinks`: the link
        // fields must have their final text before the graph is read.
        self.build_lock_sets();
    }

    /// C `dbInitRecordLinks` (`dbStaticLib.c:2171-2233`), which C runs once per
    /// record at `iocInit` and which this port runs once over the whole
    /// database at the same barrier.
    ///
    /// Three things happen to every link field, in C's order. Its TYPE comes
    /// from the device support the record's `DTYP` binds — `CONSTANT` for every
    /// field that is not the device link, since only `INP`/`OUT` have a
    /// `devSup` — and its payload starts empty (`:2185-2212`). A field the load
    /// gave no text to is then left exactly there (`:2213`), which is why
    /// `record(ai,"X"){field(DTYP,"Soft Timestamp")}` shows `INP : INST_IO @`
    /// and not `CONSTANT`. Text that does not fit the declared type is refused
    /// with a printed line and dropped — `dbInitRecordLinks` returns 0
    /// unconditionally, so the record stays, the load does not fail, and the
    /// IOC runs. Measured on softIoc R7.0.10 over a ten-record file with eight
    /// bad links: eight `ERROR:` lines at `iocInit`, ten names in `dbl` before
    /// and after, and `iocRun: All initialization complete`.
    ///
    /// The jlink `open` half rides here too, because in C it is the same pass:
    /// `dbSetLink` (`:2225`) is what calls a jlink's `open`, and it is reached
    /// only by a link that passed the check. So a `{state:…}` link on a field
    /// whose device support refuses `JSON_LINK` never creates its `dbState` —
    /// `lnkState_open` (`lnkState.c:110-116`) is `dbStateCreate`, find-or-create
    /// (`dbState.c:50-66`), and it is not called at all.
    ///
    /// C runs this pass BEFORE `init_record`; this port still runs the init
    /// passes in [`Self::add_loaded_record`], and that ordering IS observable.
    /// Measured against softIoc R7.0.10 on
    /// `record(calcout,"X"){field(INPA,"@instio p") field(OUT,"@instio q")}`:
    /// C reads `INAV`/`OUTV` as `Constant`, this port read `Ext PV NC`, because
    /// calcout had classified both at load from text this pass then replaced.
    /// A record's cached link status is therefore re-derived where the text
    /// changes (see [`Self::set_link_text`]) and once per record at the end of
    /// the loop, which is the same seam a runtime re-point uses. What is NOT
    /// closed is `init_record` itself: device support still reads the refused
    /// text where C's reads the emptied link. That needs the init passes moved
    /// to this barrier, which the two load callers that run the pass tail
    /// (`iocsh::commands`' `dbLoadRecords` and `IocBuilder`) would have to
    /// follow.
    async fn db_init_record_links(&self) {
        use crate::runtime::log::ERL_ERROR;
        use crate::server::record::{
            DbLinkType, ParsedLink, declared_link_type, link_type_refusal,
        };
        let empty_assigned =
            std::mem::take(&mut *self.inner.empty_link_assignments.lock().unwrap());
        let states = crate::server::database::filters::sync::db_state_registry();
        for name in self.all_record_names().await {
            let Some(rec) = self.get_record(&name) else {
                continue;
            };
            let (record_type, dtyp, fields) = {
                let inst = rec.read();
                (
                    inst.record.record_type().to_string(),
                    inst.common.dtyp.as_str().to_string(),
                    Self::link_field_texts(&inst),
                )
            };
            let assigned_empty = empty_assigned.get(&name);
            for (field, text, ftype) in fields {
                // C `if (!plink->text) continue;` (`:2213`) — see
                // `PvDatabaseInner::empty_link_assignments` for why the empty
                // assignment needs its own record.
                let assigned = !text.is_empty()
                    || assigned_empty.is_some_and(|fields| fields.iter().any(|f| f == &field));
                let mut refused = false;
                if assigned {
                    if let Some(line) =
                        link_type_refusal(&name, &record_type, Some(&dtyp), &field, &text)
                    {
                        eprintln!("{ERL_ERROR}: {line}");
                        refused = true;
                    }
                }
                if !assigned || refused {
                    let empty = declared_link_type(&record_type, Some(&dtyp), &field)
                        .map_or("", DbLinkType::empty_link_text);
                    if empty != text {
                        Self::set_link_text(&mut rec.write(), &field, empty);
                    }
                    continue;
                }
                if let ParsedLink::State(state) =
                    crate::server::record::parse_link_field(&text, ftype)
                {
                    states.get_or_create(&state.name);
                }
            }
            // The record's links are final from here on. `init_links` is the
            // hook that says so — calcout and swait capture the common `OUT`
            // through it, because their `special()` deliberately does not
            // re-classify that one field — and it is idempotent, so the load
            // callers' earlier call is simply superseded by this one.
            let mut inst = rec.write();
            let inst = &mut *inst;
            inst.record.init_links(&inst.common);
        }
    }

    /// Write a link field's text back through whichever storage owns it — the
    /// same split [`Self::link_field_texts`] reads it from. Only ever used to
    /// install the empty link of a declared type, so a rejected put would mean
    /// the two owners disagree about which fields are links.
    fn set_link_text(inst: &mut RecordInstance, field: &str, text: &str) {
        let value = EpicsValue::String(text.into());
        let put = if inst.common_link_text(field).is_some() {
            inst.put_common_field_db_load(field, value).map(|_| ())
        } else {
            inst.record.put_field(field, value)
        };
        debug_assert!(put.is_ok(), "{field} is a link field with no writer");
        // C brackets every link write with the special pair — `dbPutSpecial`
        // pass 0 then pass 1 around `dbSetLink` (`dbAccess.c:1174,1178`) — and
        // pass 1 is where a record re-derives what it caches from a link's
        // text: calcout `INAV..INUV`, transform's and (a)scalcout's link
        // tables. Neither writer above runs it, and this pass is the LAST
        // writer of every link field's text, so without it the classification
        // the record made at load stands against text that no longer exists.
        let _ = inst.record.special(field, true);
    }

    /// **The only entry point that can serve a link-backed field's metadata.**
    ///
    /// C reads that metadata live: `get_units` and its three siblings call
    /// `dbGetUnits`/`dbGetPrecision`/`dbGetGraphicLimits`/`dbGetAlarmLimits`
    /// inline, and `dbDbLink.c:240-261` takes the TARGET's lock for the
    /// duration — legal there because `dbLock.c:725-760` merges every
    /// DB_LINK-connected record into one lock set behind one recursive mutex,
    /// so the source's lock and the target's lock are the same mutex. This
    /// port has lock sets too now (`record_lock`), but the lock C is taking
    /// there is the one guarding the record's DATA, and that one is still a
    /// `parking_lot::RwLock` per record — non-recursive and not merged — so
    /// reaching for the target's from under the source's still inverts the
    /// order record processing takes and two mutually linked records are still
    /// enough to deadlock.
    ///
    /// So the invariant is **no record lock is held while a link is
    /// resolved**, and this function is its owner: it asks under a short read
    /// lock which link (if any) backs `field`, drops the lock, resolves, and
    /// only then re-locks to build. The resolved value is handed to the
    /// builder as a borrowed [`LinkBacking`] and never stored, which is what
    /// makes *"a served snapshot's link-backed metadata was resolved during
    /// THIS build"* true by construction rather than by a freshness check.
    ///
    /// A field no link backs never leaves the first lock.
    ///
    /// `string_view` is the channel's `$` modifier, and it is not optional:
    /// there is no door here that takes a field name without one. C decides
    /// the view once, in `dbChannelCreate` (`dbChannel.c:486-505`), and the
    /// `dbChannel` carries it for the channel's whole life, so a delivery
    /// path that knows the field but not the view is a path that has lost
    /// half of what it was asked to serve. An ineligible `$` answers `None`
    /// — the same shape [`RecordInstance::snapshot_for_field`] uses to make
    /// a caller at the wrong door serve nothing rather than something wrong.
    pub fn channel_snapshot_for_field(
        &self,
        record: &Arc<parking_lot::RwLock<RecordInstance>>,
        field: &str,
        string_view: bool,
    ) -> Option<crate::server::snapshot::Snapshot> {
        self.channel_snapshot_for_field_guarded(
            record,
            field,
            string_view,
            &mut std::collections::HashSet::new(),
        )
    }

    /// [`Self::channel_snapshot_for_field`] carrying C's `DBLINK_FLAG_VISITED` set
    /// (`dbDbLink.c:253-257`).
    ///
    /// The resolve is recursive: a `calc` whose `INPA` points at another
    /// `calc` answers its `A` metadata from THAT record's rset, which routes
    /// through ITS link. C guards the recursion with one flag per link,
    /// cleared as the inner fetch returns, so a diamond still reports on both
    /// arms while a cycle stops. `db_target_metadata` passes its caller's set
    /// through to here, so one guard spans the whole resolve exactly as C's
    /// does.
    pub(crate) fn channel_snapshot_for_field_guarded(
        &self,
        record: &Arc<parking_lot::RwLock<RecordInstance>>,
        field: &str,
        string_view: bool,
        visited: &mut std::collections::HashSet<String>,
    ) -> Option<crate::server::snapshot::Snapshot> {
        // 1. Under a short read lock: is this field link-backed at all, and if
        //    so what does its link field currently say. Nothing is resolved
        //    here — resolving needs the target's lock.
        let link = {
            let inst = record.read();
            match inst.link_backed_metadata_field_of(field) {
                None => {
                    return inst.channel_snapshot_for_field(
                        field,
                        string_view,
                        LinkBacking::none(),
                    );
                }
                Some(link_field) => match inst.record.get_field(&link_field) {
                    Some(EpicsValue::String(text)) => {
                        let text = text.as_str_lossy().into_owned();
                        (!text.is_empty()).then_some((link_field, text))
                    }
                    _ => None,
                },
            }
        };

        // 2. No record lock held. A link that resolves to nothing — CONSTANT,
        //    an unresolvable target, or one the visited guard refused — leaves
        //    the map empty, which serves each slot's C seed.
        let mut resolved = HashMap::new();
        if let Some((link_field, text)) = link {
            let parsed = crate::server::record::parse_link_v2(&text);
            if let Some(meta) = self.link_metadata(&parsed, visited) {
                resolved.insert(link_field, meta);
            }
        }

        // 3. Re-lock and build with what was just resolved. The borrow ends
        //    with this call, so there is nowhere to keep it.
        record.read().channel_snapshot_for_field(
            field,
            string_view,
            LinkBacking::resolved(&resolved),
        )
    }

    /// Every link-backed field's metadata for one record, resolved once with
    /// no record lock held — what a process cycle or a put hands to the
    /// monitor posters, which run with the record's own lock held and so
    /// cannot resolve anything themselves.
    ///
    /// Empty for the record types that back no metadata with a link, which is
    /// all but `calc`, `calcout`, `sub` and `aSub`.
    pub fn resolve_link_backed_metadata(
        &self,
        record: &Arc<parking_lot::RwLock<RecordInstance>>,
    ) -> HashMap<String, LinkMetadata> {
        let links: Vec<(String, String)> = {
            let inst = record.read();
            if inst.link_backed_metadata_links().is_empty() {
                return HashMap::new();
            }
            inst.link_backed_metadata_links()
                .iter()
                .filter_map(|lf| match inst.record.get_field(lf) {
                    Some(EpicsValue::String(text)) => {
                        let text = text.as_str_lossy().into_owned();
                        (!text.is_empty()).then_some((lf.clone(), text))
                    }
                    _ => None,
                })
                .collect()
        };
        let mut resolved = HashMap::new();
        for (link_field, text) in links {
            let parsed = crate::server::record::parse_link_v2(&text);
            let mut visited = std::collections::HashSet::new();
            if let Some(meta) = self.link_metadata(&parsed, &mut visited) {
                resolved.insert(link_field, meta);
            }
        }
        resolved
    }

    /// Add a record (accepts a boxed Record to avoid double-boxing).
    ///
    /// Returns `Err` when `name` collides with an existing record,
    /// simple PV, or alias. The C IOC's `dbLoadRecords` treats this as
    /// fatal; do not silently replace.
    ///
    /// The records-map insert AND scan-index insert run
    /// under the same `registration_mutex` hold, eliminating the
    /// TOCTOU window where `remove_record` could land between them
    /// and leave a phantom scan entry.
    pub async fn add_record(&self, name: &str, record: Box<dyn Record>) -> CaResult<()> {
        self.add_loaded_record(name, record, RecordLoad::default())
            .await
    }

    /// Add a record together with the field set its `.db` definition loaded
    /// into it — the creation sink for every `dbLoadRecords` path.
    ///
    /// C's `dbLoadRecords` writes a record's ENTIRE field set through
    /// `dbStaticLib` (including the `UDF = 0` that `dbPutString`
    /// (`dbStaticLib.c:2653-2661`) implies for any put to a field named
    /// `VAL`), and only afterwards does `iocInit::doInitRecord0`
    /// (`iocInit.c:508-536`) evaluate `if (udf && stat == UDF_ALARM) sevr =
    /// udfs`. The port used to add the record first and apply its loaded common
    /// fields afterwards, so the init passes ran against a PRE-LOAD field set:
    /// every record with a `field(VAL,…)` latched `SEVR = INVALID` at creation
    /// and the `UDF = 0` that arrived a moment later could not lower it again.
    /// A whole `.db` of setpoint defaults and sim constants came up red.
    ///
    /// Taking the loaded fields here is what makes C's ordering hold by
    /// construction: there is no window in which the init passes can observe a
    /// record whose `.db` fields have not landed, because the record is not
    /// reachable until they have. `RecordInstance::run_init_passes` is
    /// crate-private for the same reason — the sink is the only caller.
    pub async fn add_loaded_record(
        &self,
        name: &str,
        record: Box<dyn Record>,
        load: RecordLoad,
    ) -> CaResult<()> {
        let gate = self.lock_registration("add_loaded_record");
        // A record created after `iocInit` needs a lock set, and its links —
        // in both directions — may merge it into existing ones. `None` while
        // the database is still loading, which is every ordinary
        // `dbLoadRecords`: `build_lock_sets` builds the whole graph at
        // `iocInit` instead.
        let _relink = self.lock_set_membership_change(name);
        self.check_name_free(name)?;
        let mut instance = RecordInstance::new_boxed(name.to_string(), record);
        // Hand the record a cycle-free handle to its own database so it can
        // post out-of-band field updates / wire completion-driven re-entry
        // (asyn TRACE callback, sseq WAITn) without owning the database.
        // C records reach `dbCommon::pdba`/the IOC the same way at
        // `dbDefineRecord` init; the framework supplies the back-reference,
        // the record never constructs it. Defaulted no-op for records that
        // do not need it.
        instance
            .record
            .set_async_context(name.to_string(), self.async_handle());

        // Hand the record the current breakpoint-table registry snapshot so a
        // LINR>=3 ai/ao record can resolve its table lazily at convert time.
        // add_record is the single creation sink (IocBuilder, dbLoadRecords,
        // dbCreateRecord, inline records all funnel through here), so this one
        // install covers every creation path uniformly. The trait default is a
        // no-op for records that don't use it; skipped when no tables are
        // loaded so the common case pays no Arc clone. A record created before
        // its table is loaded is re-installed by `add_breaktables`.
        {
            let snapshot = self.inner.breaktable_registry.load_full();
            if !snapshot.is_empty() {
                instance.record.install_breaktable_registry(snapshot);
            }
        }

        // The `.db` load, applied to the instance BEFORE the init passes below
        // — C's `dbLoadRecords` → `iocInit` ordering. The `.db` value coercion
        // (`put_common_field_db_load`) differs from a runtime `dbPut`'s: C's
        // loader converter has a wider menu bound (`dbStaticRun.c`).
        //
        // The scan-index entry is built from `instance.common.scan` further
        // down, i.e. from the POST-load field set, so a `field(SCAN,…)` needs
        // no index fix-up here — the record has not been published yet.
        //
        // C stores each of them with `dbPutString` and, when that fails, prints
        // the refusal and calls `yyerror(NULL)` (`dbLexRoutines.c:1406-1416`):
        // the field keeps its default, the record's other fields still load,
        // and the load's status goes non-zero. The port used to print a warning
        // of its own and carry on, so `field(SCAN,"Passiv")` loaded a Passive
        // record where C refuses the database outright — and `field(DTYP,...)`
        // was not checked at all, because the DTYP arm stores whatever string
        // it is handed.
        //
        // This is where the menu arm of that put can be decided: a `DBF_DEVICE`
        // field's choices are the record type's registered device support, and
        // that registry is complete only once the builder has run every
        // registration — after the `.db` text was parsed. C has no such
        // ordering, since every `.dbd` is loaded before any `.db`.
        let mut refused: Option<String> = None;
        // C's `plink->text` for the empty assignment — see
        // `PvDatabaseInner::empty_link_assignments`. Taken from the load's own
        // field list because that is the only place the distinction between
        // `field(INP,"")` and no `INP` line survives.
        let mut empty_links: Vec<String> = Vec::new();
        for (field, value) in load.common_fields {
            if let crate::types::EpicsValue::String(text) = &value {
                if text.as_str_lossy().is_empty()
                    && instance.common_link_text(&field.to_uppercase()).is_some()
                {
                    empty_links.push(field.to_uppercase());
                }
            }
            if let crate::types::EpicsValue::String(text) = &value {
                if let Some(refusal) = crate::server::db_loader::menu_value_refusal(
                    instance.record.record_type(),
                    name,
                    &field,
                    &text.as_str_lossy(),
                ) {
                    if let Some(notice) = refusal.notice {
                        eprintln!("{notice}");
                    }
                    eprintln!("{}", refusal.line);
                    // C follows the refusal with `dbPutStringSuggest`, which
                    // proposes the closest choice or prints nothing
                    // (`dbLexRoutines.c:1414`). It reaches the operator through
                    // errlog rather than stderr, so C's own output shows the
                    // proposals batched at the end; one stream puts each under
                    // the line it explains.
                    if let Some(suggestion) = refusal.suggestion {
                        eprintln!("{suggestion}");
                    }
                    refused.get_or_insert(format!("{name}.{field}"));
                    continue;
                }
            }
            if let Err(e) = instance.put_common_field_db_load(&field, value) {
                eprintln!("put_common_field({field}) failed for {name}: {e}");
            }
        }
        if let Some(what) = refused {
            return Err(CaError::BadChoice(what));
        }
        // `info(...)` tags land before `init_record`, so device support that
        // reads them at init sees the values.
        for (key, value) in &load.info_tags {
            instance.set_info(key, value);
        }

        // C's `iocInit` init passes, through their owner (the `doInitRecord0`
        // prologue — `pact = FALSE` plus the initial UDF severity — then
        // `init_record(0)`, `init_record(1)`, and the UDF tail). This sink is
        // the single site that runs them: a record built programmatically, by
        // iocsh `dbCreateRecord`, or from a `.db` is initialised the same way,
        // and — since the load above has already landed — always against its
        // FINAL field set.
        if !empty_links.is_empty() {
            self.inner
                .empty_link_assignments
                .lock()
                .unwrap()
                .insert(name.to_string(), empty_links);
        }

        // C `doInitRecord0` (`iocInit.c:530-536`) binds the dset and only then
        // calls `init_record(pass 0)`. Both lines are here, in that order,
        // because every `<rec>Record.c init_record` opens by testing the dset
        // — `ao`'s `prec->init = TRUE`, `sub`'s MLST/ALST/LALM seed and `ai`'s
        // are all BELOW that test, and running the passes first is what made
        // them reachable for a record C refuses.
        crate::server::ioc_app::attach_device_support(
            &mut instance,
            name,
            self.inner.device_support_resolver.load().as_deref(),
        );
        // C `registryFunctionFind` reads a process-global registry from inside
        // `init_record` pass 1, so the record is handed the registry rather
        // than resolved against it from out here: the lookup's failure is an
        // early return that the init tail must not run past, and only the init
        // owner can honour that.
        instance.arm_init_subroutines(self.inner.subroutine_registry.load_full());
        instance.run_init_passes(name);

        // The init-seed owner: every CONSTANT link the record declares
        // (`Record::constant_init_links`) is loaded into its value field ONCE,
        // here — a constant delivers NOTHING at process time
        // (`dbConstLink.c:219-225`). `add_record` is the creation sink every
        // path funnels through, so this covers a record built programmatically
        // as well as one loaded from a .db; `IocBuilder`/`dbLoadRecords` call
        // the owner again after `init_record(1)`, once the record's final
        // NELM/FTVL buffer exists for an array constant to land in. Seeding
        // twice is a no-op — both run before any client put.
        super::database::processing::seed_constant_links(&mut instance);

        let scan = instance.common.scan;
        let phas = instance.common.phas;
        let record_type = instance.record.record_type();
        let rec_arc = Arc::new(parking_lot::RwLock::new(instance));
        self.inner
            .records
            .write()
            .insert(name.to_string(), rec_arc.clone());
        // The record is reachable from this line on, so anything its
        // `set_async_context` parked above may now run. This is the only
        // release site because this is the only site that registers the name.
        self.release_record_inits(name);

        // Assign a monotonic load-order sequence — the scan-index
        // secondary sort key, so same-PHAS records keep load order.
        let seq = self
            .inner
            .load_order_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.load_order.update(|m| {
            m.insert(name.to_string(), seq);
        });

        self.add_to_scan_list(scan, phas, record_type, seq, name);

        // Registration is complete and the name is published, so the gate has
        // nothing left to serialize. It is released HERE, before the tail
        // below, and that is load-bearing rather than tidy:
        // `recGblInitSimm` can swap SCAN, and a scan change is applied by
        // `update_scan_index`, which takes this same `registration_mutex`
        // itself as the single owner of a scan-index transition. Holding it
        // across the call is a self-deadlock on a non-reentrant mutex — the
        // record only has to carry a constant SIML and a periodic SCAN to
        // reach it.
        drop(gate);

        // The rest of C's `init_record` pass 1, which needs the record
        // REGISTERED and so cannot run with `run_init_passes` above:
        // `recGblInitSimm` plus its `recGblInitConstantLink(&siol, …, &sval)`
        // (recGbl.c:439-446, from e.g. aiRecord.c:101), then `wdogInit`
        // (histogramRecord.c:168). C reaches both through `iterateRecords`
        // (`iocInit.c:562-586`), which visits every record in the database
        // whatever created it; here they sat on the loader callers instead, so
        // an inline or `dbCreateRecord` record got neither. Both are no-ops for
        // a record type that declares no SIMM / no SDEL. Running them outside
        // the gate is also what C does: `iterateRecords` is a separate pass
        // over an already-built database, holding no registration lock.
        self.rec_gbl_init_simm(&rec_arc);
        self.arm_watchdog(name);
        Ok(())
    }

    /// Verify that `name` is not currently registered in any of the
    /// three namespaces. Caller MUST hold `registration_mutex` so the
    /// peek-then-insert sequence is atomic — without that, two tasks
    /// can both see the name as free and race the insert.
    ///
    /// Synchronous: all three namespaces are blocking locks now, so this peek
    /// makes no suspension point inside the `registration_mutex` hold — which
    /// is what lets that gate become a `PriorityInheritanceMutex` whose `!Send`
    /// guard may not cross an `.await`.
    fn check_name_free(&self, name: &str) -> CaResult<()> {
        let kind = if self.inner.simple_pvs.lock().contains_key(name) {
            Some("simple PV")
        } else if self.inner.records.read().contains_key(name) {
            Some("record")
        } else if self.inner.aliases.read().contains_key(name) {
            Some("alias")
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(CaError::DbParseError {
                line: 0,
                token: String::new(),
                message: format!("name '{name}' is already registered as a {kind}"),
            });
        }
        Ok(())
    }

    /// Remove a record by name. Returns `true` if a record was removed,
    /// `false` if no such name was registered. Mirrors epics-base PR
    /// #505 — deletion at database creation, exposed here as a public
    /// API so iocsh `dbDeleteRecord` and tests can drive it.
    ///
    /// The cleanup covers the three indices that `add_record` populates:
    /// the records map, the scan index, and CP-link source/target lists.
    /// Live subscribers on the removed record drop their `Sender` clone
    /// when the `RecordInstance` is dropped — they observe `Closed` on
    /// next recv, matching the existing dbEvent cancel flow.
    pub async fn remove_record(&self, name: &str) -> bool {
        let _gate = self.lock_registration("remove_record");
        // C `dbDeleteRecord` frees the record's `lockRecord`, so the set it
        // was in loses a member and may fall apart into several. Declared
        // here so the relink runs once the record is out of the map.
        let _relink = self.lock_set_membership_change(name);
        // 1) Remove from main map; keep scan + phas for scan-index cleanup.
        let removed = self.inner.records.write().remove(name);
        let Some(rec_arc) = removed else {
            return false;
        };
        let scan = {
            let inst = rec_arc.read();
            inst.common.scan
        };

        // 2) Drop from scan index if it was scheduled.
        self.delete_from_scan_list(scan, name);

        // 2b) Drop the load-order entry.
        self.inner.load_order.update(|m| {
            m.remove(name);
        });

        // 3) Drop from CP-link tables. Removed both as source (channel
        // change → trigger targets) and as target (other channels'
        // CP lists may still reference this name).
        self.inner.cp_links.update(|cp| {
            cp.remove(name);
            for targets in cp.values_mut() {
                targets.retain(|t| t.record != name);
            }
        });

        // 4) Purge aliases that pointed AT the
        // removed record. Otherwise `find_pv("ALT")` returns None
        // (target gone) but `add_pv("ALT", ...)` still fails with
        // "already registered as an alias" — orphan blocks reuse.
        let mut aliases = self.inner.aliases.write();
        let orphaned: Vec<String> = aliases
            .iter()
            .filter(|(_, target)| *target == name)
            .map(|(alias, _)| alias.clone())
            .collect();
        aliases.retain(|_alias, target| target != name);
        drop(aliases);
        // The alias nodes go with the record, and so do their load-order
        // sequences: a name with a sequence and no node would keep a
        // node-list walk sorting against a node that no longer exists.
        if !orphaned.is_empty() {
            self.inner.load_order.update(|m| {
                for alias in &orphaned {
                    m.remove(alias);
                }
            });
        }

        // Same rule as `remove_simple_pv`: removal IS destruction. The
        // record's own `Arc` outlives the map entry for as long as a CA
        // channel holds it, so without the mark a downstream monitor would
        // keep serving a record the database no longer has.
        rec_arc.write().destroy();
        self.signal_destroyed();

        true
    }

    /// Internal: synchronous lookup without invoking the search resolver.
    async fn find_entry_no_resolve(&self, name: &str) -> Option<PvEntry> {
        // a channel name may carry a `.{"arr":...}` filter
        // suffix. Strip it before lookup — the suffix is a per-channel
        // filter spec, not part of the PV identity. `split_channel_name`
        // is the single owner of "channel name → record_path" and is
        // idempotent on an already-stripped name. Without this a
        // filtered SimplePv (`SP.{"arr":...}`) never matches
        // `simple_pvs` (keyed by the bare PV name) and even a filtered
        // record fails when the JSON contains a `.` (e.g.
        // `{"dbnd":{"d":0.5}}`), because the bare `parse_pv_name` last-dot
        // split would tear the suffix apart instead of removing it.
        let record_path = filters::split_channel_name(name).record_path;
        let (base, _field) = parse_pv_name(&record_path);

        let simple = self
            .inner
            .simple_pvs
            .lock()
            .get(record_path.as_str())
            .cloned();
        if let Some(pv) = simple {
            return Some(PvEntry::Simple(pv));
        }
        if let Some(rec) = self.inner.records.read().get(base) {
            return Some(PvEntry::Record(rec.clone()));
        }
        // Alias resolve (epics-base PR #336): the alternate name maps
        // to a canonical record name. Look up the real record after
        // translating the base.
        if let Some(target) = self.inner.aliases.read().get(base).cloned() {
            if let Some(rec) = self.inner.records.read().get(&target) {
                return Some(PvEntry::Record(rec.clone()));
            }
        }
        None
    }

    /// Register an alias `alias` for an existing record `target`.
    /// Mirrors epics-base PR #336. Returns `Err(...)` when the target
    /// does not exist or the alias name is already in use anywhere
    /// in the database (records, simple PVs, or other aliases).
    ///
    /// Pre-fix the alias path checked only
    /// `records` and `aliases` — a simple-PV with the same name as
    /// the proposed alias was missed, leaving the database in a
    /// state where `find_pv(alias)` could resolve to either the
    /// simple PV or the alias-mapped record depending on lookup
    /// order. Now we run the same cross-namespace `check_name_free`
    /// guard the other add_* paths use.
    pub async fn add_alias(&self, alias: &str, target: &str) -> CaResult<()> {
        let _gate = self.lock_registration("add_alias");
        // A link naming `alias` resolved to nothing until now, so the alias
        // can turn a dangling link into a real edge and merge two sets.
        let _relink = self.lock_set_membership_change(target);
        if !self.inner.records.read().contains_key(target) {
            return Err(CaError::ChannelNotFound(format!(
                "alias target '{target}' is not a registered record"
            )));
        }
        self.check_name_free(alias)?;
        self.inner
            .aliases
            .write()
            .insert(alias.to_string(), target.to_string());
        // An alias is a node of the database, so it takes a sequence from the
        // same counter the records draw from — C numbers it identically,
        // `pnewnode->order = pdbentry->pdbbase->no_records++`
        // (`dbStaticLib.c:1704`), which is what puts an alias at its own load
        // position in the list `dbl` and `dbglob` walk.
        let seq = self
            .inner
            .load_order_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.load_order.update(|m| {
            m.insert(alias.to_string(), seq);
        });
        Ok(())
    }

    /// The current breakpoint-table registry snapshot. C reaches the same
    /// list as `pdbbase->bptList`, which `dbDumpBreaktable`
    /// (`dbStaticLib.c:3533-3555`) enumerates; the port keeps it here.
    ///
    /// Distinct from `add_breaktables(vec![])`, which reads the same cell but
    /// takes the registration gate to do it — a reader has nothing to
    /// serialise against, since the cell is replaced wholesale.
    pub fn breaktable_registry(&self) -> Arc<crate::server::cvt_bpt::BreakTableRegistry> {
        self.inner.breaktable_registry.load_full()
    }

    /// Resolve an alias to its target record name, or `None` when the
    /// name is not an alias.
    pub fn resolve_alias(&self, name: &str) -> Option<String> {
        self.inner.aliases.read().get(name).cloned()
    }

    /// Queue an iocsh command line for post-PINI execution.
    /// Mirrors epics-base PR #558 — `afterIocRunning <command>` lets
    /// the startup script schedule actions that run after iocInit
    /// completes (when the record set is fully wired up).
    pub fn queue_after_ioc_running(&self, line: impl Into<String>) {
        self.inner
            .after_ioc_running
            .lock()
            .unwrap()
            .push(line.into());
    }

    /// Drain the post-PINI iocsh command queue. Called by
    /// `IocApplication::run` after PINI processing.
    pub fn take_after_ioc_running(&self) -> Vec<String> {
        std::mem::take(&mut *self.inner.after_ioc_running.lock().unwrap())
    }

    /// Internal: synchronous existence check without resolver.
    fn has_name_no_resolve(&self, name: &str) -> bool {
        // strip the channel-filter suffix before lookup so a
        // filtered channel (`SP.{"arr":...}` / `REC.{"dbnd":{"d":0.5}}`)
        // resolves to its underlying PV at UDP-search time. This is the
        // search-side twin of `find_entry_no_resolve`; without it a
        // filtered SimplePv never answers a SEARCH and the client never
        // reaches CREATE_CHAN. See that function for the full rationale.
        let record_path = filters::split_channel_name(name).record_path;
        if self
            .inner
            .simple_pvs
            .lock()
            .contains_key(record_path.as_str())
        {
            return true;
        }
        // C's search-side test is `dbChannelTest` (`dbChannel.c:441-464`),
        // which resolves the FIELD too — `REC.NOSUCH` answers "does not
        // exist" rather than drawing the client into a CREATE_CHAN it must
        // then refuse (pvxs#193). Validate an explicit suffix; a bare name
        // binds `VAL`, which every record type declares, so the record's
        // existence alone answers it — that also keeps this function
        // lock-free per record for the DB-link locality callers, which
        // pass suffix-less record names.
        let (base, explicit_field) = match record_path.rsplit_once('.') {
            Some((base, field)) => (base, Some(field)),
            None => (record_path.as_str(), None),
        };
        let rec = self.inner.records.read().get(base).cloned().or_else(|| {
            // Alias entry exists and points to a live record
            // (epics-base PR #336).
            let target = self.inner.aliases.read().get(base).cloned();
            target.and_then(|t| self.inner.records.read().get(&t).cloned())
        });
        let Some(rec) = rec else {
            return false;
        };
        let Some(field) = explicit_field else {
            return true;
        };
        let instance = rec.read();
        // Trailing `$` is the long-string modifier, part of the channel
        // syntax (`dbChannel.c:486-505`): eligible only on a `DBF_STRING`
        // or link field, and `dbChannelTest` refuses it anywhere else.
        match field.strip_suffix('$') {
            Some(core) => instance
                .resolve_string_view_field(&core.to_ascii_uppercase())
                .is_some(),
            None => {
                // Existence is the DECLARED-name question, not the
                // has-a-value question: `dbNameToAddr` resolves any field
                // the `.dbd` declares — including `DBF_NOACCESS` ones like
                // `MLOK` — so pvxs answers the SEARCH for them and refuses
                // at CREATE instead (measured against `softIocPVX`:
                // `pvxget ORACLE:AI.MLOK` → `Refused to create Channel`).
                // Three name sources, matching the port's field model:
                // `resolve_field` (valued fields, incl. common/virtual ones
                // like `RTYP`/`TIME` that C answers from dbStaticLib),
                // `field_desc` (declared-but-valueless record fields), and
                // the `DBF_NOACCESS` internals the generated tables drop —
                // record-own (`BPTR`, from `record_noaccess_fields`) and
                // `dbCommon` (`MLOK`) alike.
                let upper = field.to_ascii_uppercase();
                instance.resolve_field(&upper).is_some()
                    || instance.field_desc(&upper).is_some()
                    || instance.resolves_noaccess_name(&upper)
                    // ... and the record type's attributes, which
                    // `pvNameLookup` reaches through `dbGetAttributePart`
                    // once the declared list has missed
                    // (`dbChannel.c:326-327`). Shadowing needs no test
                    // here: a declared name has already answered above.
                    || self
                        .record_type_attribute(instance.record.record_type(), &upper)
                        .is_some()
            }
        }
    }

    /// Look up an entry by name. Supports "record.FIELD" syntax.
    ///
    /// If the name is not found and a search resolver is installed,
    /// the resolver is invoked once. If the resolver returns true, the
    /// database is re-checked.
    pub async fn find_entry(&self, name: &str) -> Option<PvEntry> {
        self.find_entry_from(name, None).await
    }

    /// Like [`Self::find_entry`], but threads the downstream client's
    /// socket address into the search resolver. The CA TCP CREATE_CHANNEL
    /// handler passes the connection peer so the gateway can apply
    /// host-scoped `.pvlist` admission.
    pub async fn find_entry_from(
        &self,
        name: &str,
        peer: Option<std::net::SocketAddr>,
    ) -> Option<PvEntry> {
        if let Some(entry) = self.find_entry_no_resolve(name).await {
            // A cached simple PV must still pass the per-request
            // existence gate (CA gateway host/state admission). When the
            // gate denies it, answer does-not-exist for this requester
            // instead of returning the stale shadow entry — C ca-gateway
            // re-runs `gateAs::findEntry`/cache-state on every
            // `pvExistTest` (gateServer.cc:1516-1637). Records/aliases
            // bypass the gate (see `simple_pv_gate_denies`).
            if matches!(entry, PvEntry::Simple(_)) && self.simple_pv_gate_denies(name, peer).await {
                return None;
            }
            return Some(entry);
        }
        // Try the search resolver
        let resolver = self.inner.search_resolver.load_full().map(|r| (*r).clone());
        if let Some(r) = resolver {
            if r(name.to_string(), peer).await {
                return self.find_entry_no_resolve(name).await;
            }
        }
        None
    }

    /// Check if a base name exists (for UDP search).
    ///
    /// If the name is not in the database and a search resolver is installed,
    /// the resolver is invoked. The resolver may populate the database
    /// (e.g., subscribe to an upstream IOC and add a placeholder PV) and
    /// return true; this method then re-checks.
    pub async fn has_name(&self, name: &str) -> bool {
        self.has_name_from(name, None).await
    }

    /// Like [`Self::has_name`], but threads the downstream client's
    /// socket address into the search resolver. The CA UDP search
    /// responder passes the datagram source address so the gateway can
    /// apply host-scoped `.pvlist` admission.
    pub async fn has_name_from(&self, name: &str, peer: Option<std::net::SocketAddr>) -> bool {
        if self.has_name_no_resolve(name) {
            // Same per-request gate as `find_entry_from`: a cached simple
            // PV the gateway's host/state admission denies must answer
            // does-not-exist at search time. Records/aliases bypass.
            if self.simple_pv_gate_denies(name, peer).await {
                return false;
            }
            return true;
        }
        let resolver = self.inner.search_resolver.load_full().map(|r| (*r).clone());
        if let Some(r) = resolver {
            if r(name.to_string(), peer).await {
                return self.has_name_no_resolve(name);
            }
        }
        false
    }

    /// Look up a simple PV by name (backward-compatible).
    pub async fn find_pv(&self, name: &str) -> Option<Arc<ProcessVariable>> {
        self.inner.simple_pvs.lock().get(name).cloned()
    }

    /// Get a record Arc by name. Alias-aware (epics-base PR #336):
    /// when `name` is not a canonical record but matches a registered
    /// alias, the alias' target record is returned. Mirrors base
    /// `dbNameToAddr` behaviour, so dbpf/dbpr/dbgf, CA channel lookup,
    /// and DB-link target resolution all work transparently for
    /// aliases.
    ///
    /// Use [`Self::get_record_no_resolve`] when the caller already
    /// holds a canonical name and wants to suppress the alias path
    /// (e.g. to detect alias collisions during builder wiring).
    pub fn get_record(&self, name: &str) -> Option<Arc<parking_lot::RwLock<RecordInstance>>> {
        if let Some(rec) = self.inner.records.read().get(name).cloned() {
            return Some(rec);
        }
        let target = self.inner.aliases.read().get(name).cloned()?;
        self.inner.records.read().get(&target).cloned()
    }

    /// Strict variant of [`Self::get_record`] — does NOT consult the
    /// alias table. Returns `Some` only when a canonical record with
    /// that exact name exists.
    pub fn get_record_no_resolve(
        &self,
        name: &str,
    ) -> Option<Arc<parking_lot::RwLock<RecordInstance>>> {
        self.inner.records.read().get(name).cloned()
    }

    /// Every record name, in **database load order** — the single owner of
    /// whole-database iteration order.
    ///
    /// C parity: `dbFirstRecord`/`dbNextRecord` walk the record list of each
    /// record type in the order `dbReadDatabase` appended them, so every
    /// whole-database pass a C IOC makes — `initDevSup`, `initDatabase`,
    /// `initialProcess` (PINI), `dbl`/`dbgrep` dumps — visits records in load
    /// order. Device support is written against that contract: a dynamic device
    /// support whose record references another record (epics-modules/opcua's
    /// element records require their `opcuaItem` record to have bound first,
    /// linkParser.cpp:226-234) only boots if the referenced record was wired
    /// first, which the `.db` guarantees by declaring it first.
    ///
    /// The names live in a `HashMap`, so returning `keys()` made that order the
    /// hash order: neither load order nor even stable across runs of the same
    /// binary (`RandomState` reseeds per process). Booting the same database
    /// twice could wire records in two different orders — one boot succeeding
    /// and the next failing. Ordering here, at the one accessor every
    /// whole-database walk already goes through, makes every such pass
    /// deterministic and load-ordered at once.
    ///
    /// The order key is the existing per-record `load_order` sequence (the
    /// scan-index's secondary sort key), so this ordering and the scan lists'
    /// ordering are the same fact, not two. A record with no sequence — none
    /// exists; `add_record` is the only insertion path — would sort last by
    /// name rather than nondeterministically.
    pub async fn all_record_names(&self) -> Vec<String> {
        // Lock order records → load_order (matching `add_record`/`remove_record`):
        // the `records` map is a sync `parking_lot::RwLock` now, so its guard is
        // `!Send` and MUST NOT be held across the async `load_order` read. Snapshot
        // the keys under the records guard, release it (block close), then await
        // load_order — neither lock is ever held while waiting on the other, so the
        // records→load_order order is honoured without an AB-BA against add_record.
        // The two reads are no longer one atomic snapshot: a record inserted between
        // them is absent from `load_order` and sorts last by name via the
        // `unwrap_or(u64::MAX)` fallback — the same degradation already defined for a
        // sequence-less record, and every whole-database walk is racy against a
        // concurrent add/remove regardless.
        let mut names: Vec<String> = {
            let records = self.inner.records.read();
            records.keys().cloned().collect()
        };
        let load_order = self.inner.load_order.load();
        names.sort_by(|a, b| {
            let seq = |n: &String| load_order.get(n).copied().unwrap_or(u64::MAX);
            seq(a).cmp(&seq(b)).then_with(|| a.cmp(b))
        });
        names
    }

    /// Every node C's `dbFirstRecord` / `dbNextRecord` walk visits — the
    /// records AND the alias nodes — in database load order.
    ///
    /// See [`DbNode`] for why the alias nodes belong in this list. The order
    /// is C's for the same reason the record order is: both kinds of node draw
    /// their sequence from one counter (`pdbbase->no_records++`,
    /// `dbStaticLib.c:1704`), so an alias sits where it was declared rather
    /// than after every record.
    ///
    /// The caller groups by record type; an alias groups with its target,
    /// because C's alias node lives in the target's own type list.
    pub async fn all_db_nodes(&self) -> Vec<DbNode> {
        // Same lock discipline as `all_record_names`: snapshot each map under
        // its own guard and let the guard die with the statement, so no two
        // are ever held at once and none is live across an await.
        let mut nodes: Vec<DbNode> = {
            let records = self.inner.records.read();
            records
                .keys()
                .map(|name| DbNode {
                    name: name.clone(),
                    alias_of: None,
                })
                .collect()
        };
        nodes.extend({
            let aliases = self.inner.aliases.read();
            aliases
                .iter()
                .map(|(alias, target)| DbNode {
                    name: alias.clone(),
                    alias_of: Some(target.clone()),
                })
                .collect::<Vec<_>>()
        });
        let load_order = self.inner.load_order.load();
        nodes.sort_by(|a, b| {
            let seq = |n: &DbNode| load_order.get(&n.name).copied().unwrap_or(u64::MAX);
            seq(a).cmp(&seq(b)).then_with(|| a.name.cmp(&b.name))
        });
        nodes
    }

    /// Get all alias names registered against existing records.
    /// Mirrors the alias-half of base's `dbFirstRecord` iteration —
    /// `dbgrep` / `dbglob` / `dbsr` walk both record names and
    /// aliases when matching a glob.
    pub fn all_alias_names(&self) -> Vec<String> {
        self.inner.aliases.read().keys().cloned().collect()
    }

    /// Return every alias that points at `canonical`. Sorted for
    /// stable output; empty when the record has no aliases. Used by
    /// `dbpr` to surface alias-form names so admins can see how
    /// clients may reach the record.
    pub fn aliases_for_record(&self, canonical: &str) -> Vec<String> {
        let aliases = self.inner.aliases.read();
        let mut hits: Vec<String> = aliases
            .iter()
            .filter_map(|(alias, target)| {
                if target == canonical {
                    Some(alias.clone())
                } else {
                    None
                }
            })
            .collect();
        hits.sort();
        hits
    }

    /// Get all simple PV names.
    pub async fn all_simple_pv_names(&self) -> Vec<String> {
        self.inner.simple_pvs.lock().keys().cloned().collect()
    }
}

// RTEMS-EXEC-MODEL-ALLOW(1):
// `the_snapshot_door_refuses_an_ineligible_dollar_view` is a `#[tokio::test]`,
// so the attribute builds the current-thread runtime it needs rather than
// borrowing an ambient one, and its body only takes database locks — it never
// reaches the `runtime::task` seam the exec backend replaces. Run under
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`: it passes, so gating it out would drop
// live coverage of the `$`-view door.
#[cfg(test)]
mod channel_view_door_tests {
    use super::PvDatabase;
    use crate::server::records::ai::AiRecord;

    /// The database's snapshot door takes the channel's `$` view, and an
    /// ineligible one answers `None` rather than the unviewed snapshot.
    ///
    /// C decides this in `dbChannelCreate`: `$` re-views a `DBF_STRING`
    /// (`dbChannel.c:488-493`) or a `DBF_INLINK..DBF_FWDLINK` link
    /// (`:494-498`), and anything else is `S_dbLib_fieldNotFound`
    /// (`:499-501`). There is deliberately no door here that takes a field
    /// name alone: `REC.VAL` resolves for any type, so a caller holding the
    /// field but not the view cannot tell the two cases apart and would
    /// serve `VAL$` on a `DBF_DOUBLE` as a double.
    #[tokio::test]
    async fn the_snapshot_door_refuses_an_ineligible_dollar_view() {
        let db = PvDatabase::new();
        db.add_record("VD:ai", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        let rec = db.get_record("VD:ai").expect("record");

        assert!(
            db.channel_snapshot_for_field(&rec, "VAL", false).is_some(),
            "the unviewed VAL is an ordinary double snapshot"
        );
        assert!(
            db.channel_snapshot_for_field(&rec, "VAL", true).is_none(),
            "`VAL$` on a DBF_DOUBLE is S_dbLib_fieldNotFound, not a double"
        );

        // Both eligible branches still answer, and answer the string the
        // view collapses to (pvxs `iocsource.cpp:133-136`).
        for eligible in ["DESC", "NAME", "EGU", "FLNK"] {
            let snap = db
                .channel_snapshot_for_field(&rec, eligible, true)
                .unwrap_or_else(|| panic!("`{eligible}$` must be eligible"));
            assert!(
                matches!(snap.value, crate::types::EpicsValue::String(_)),
                "`{eligible}$` serves the string, got {:?}",
                snap.value
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C `recGblGetTimeStampSimm` (recGbl.c:310-343) maps TSE values
    /// to epicsTime sources via the constants in `epicsTime.h:102-104`.
    /// The Rust port previously misread TSE=-1 as "device-provided
    /// with BestTime fallback" and gated the BestTime call on a
    /// UNIX_EPOCH check. C calls `epicsTimeGetEvent(-1)`
    /// unconditionally; only TSE=-2 (epicsTimeEventDeviceTime) leaves
    /// `precord->time` untouched.
    ///
    /// Regression: a stale device write (any non-epoch SystemTime)
    /// suppressed every BestTime refresh thereafter.
    #[test]
    fn apply_timestamp_tse_minus_one_always_overwrites_with_best_time() {
        use crate::server::record::CommonFields;
        use std::time::{Duration, SystemTime};

        // Pre-populate `time` with a stale but non-epoch sentinel.
        let stale = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut common = CommonFields::default();
        common.tse = -1;
        common.time = stale;

        TselStamp::None.stamp("REC", &mut common, false);

        // BestTime must have run unconditionally — `common.time` is
        // no longer the stale sentinel.
        assert_ne!(
            common.time, stale,
            "TSE=-1 must always overwrite via generalTime BestTime, \
             matching C epicsTimeGetEvent(-1) called unconditionally"
        );
    }

    /// C `epicsTimeEventDeviceTime = -2` (epicsTime.h:104). The C
    /// path does NOT call `epicsTimeGetEvent` for this TSE value;
    /// device support has already set `precord->time` before the
    /// recGbl call. The Rust port must leave `common.time` untouched.
    #[test]
    fn apply_timestamp_tse_minus_two_preserves_device_provided_time() {
        use crate::server::record::CommonFields;
        use std::time::{Duration, SystemTime};

        let device_time = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let mut common = CommonFields::default();
        common.tse = -2;
        common.time = device_time;

        TselStamp::None.stamp("REC", &mut common, false);

        assert_eq!(
            common.time, device_time,
            "TSE=-2 (epicsTimeEventDeviceTime) must preserve device-provided time"
        );
    }

    /// C `generalTimeGetEventPriority` rejects every event below
    /// `epicsTimeEventBestTime` with `S_time_badEvent`
    /// (`epicsGeneralTime.c:254-255`), and `recGblGetTimeStampSimm`
    /// (`recGbl.c:325-327`) writes nothing into `prec->time` on that status —
    /// it errlogs and the record keeps the stamp it had. `TSE` is
    /// `epicsInt16`, so `caput X.TSE -3` reaches this path.
    #[test]
    fn apply_timestamp_below_best_time_keeps_the_stale_stamp_and_errlogs() {
        use crate::server::record::CommonFields;
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, SystemTime};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct CaptureBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for CaptureBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for CaptureBuf {
            type Writer = CaptureBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = CaptureBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::INFO)
            .with_target(false)
            .without_time()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let stale = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut common = CommonFields::default();
        common.tse = -3;
        common.time = stale;

        TselStamp::None.stamp("X", &mut common, false);

        assert_eq!(
            common.time, stale,
            "TSE below epicsTimeEventBestTime must leave TIME alone, not stamp now"
        );
        let logged = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        assert!(
            logged.contains("recGblGetTimeStampSimm: epicsTimeGetEvent failed, X.TSE = -3"),
            "C errlogs the failed event lookup; captured: {logged:?}"
        );
    }

    #[test]
    fn select_link_indices_fanout_all_specified_mask() {
        use crate::server::record::AlarmSeverity;
        // All — every slot.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 0, 0, 0, 0, 16);
        assert_eq!(r.indices, (0..16).collect::<Vec<_>>());
        assert!(r.alarm.is_none());

        // Specified, 0-based: SELN=0 selects LNK0 (C parity, fanout).
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 0, 0, 0, 16);
        assert_eq!(r.indices, vec![0]);
        // Specified with OFFS bias: SELN=2 + OFFS=3 → index 5.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 2, 3, 0, 16);
        assert_eq!(r.indices, vec![5]);
        // Out-of-range Specified → INVALID alarm, no links.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 20, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // Negative resolved index (SELN + negative OFFS) → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 0, -1, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));

        // Mask: SELN=0b101 → bits 0 and 2.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 0, 16);
        assert_eq!(r.indices, vec![0, 2]);
        // Mask with SHFT: SELN=0b101 >> 1 = 0b10 → bit 1.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 1, 16);
        assert_eq!(r.indices, vec![1]);
        // Mask with negative SHFT: SELN=0b101 << 1 = 0b1010 → bits 1,3.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, -1, 16);
        assert_eq!(r.indices, vec![1, 3]);
        // SHFT out of [-15,15] → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 16, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));

        // Unknown SELM → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 9, 0, 0, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
    }

    #[test]
    fn select_link_indices_dfanout_specified_is_one_based() {
        use crate::server::record::AlarmSeverity;
        // dfanout Specified is 1-based: SELN=1 → OUTA (index 0).
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 1, 0, 0, 16);
        assert_eq!(r.indices, vec![0]);
        // SELN=2 → OUTB (index 1).
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 2, 0, 0, 16);
        assert_eq!(r.indices, vec![1]);
        // SELN=0 → drive nothing, NO alarm.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 0, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert!(r.alarm.is_none());
        // SELN > 16 → INVALID.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 17, 0, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // dfanout Mask has no SHFT — SHFT arg ignored.
        let r = select_link_indices_ex(SelmKind::Dfanout, 2, 5, 0, 7, 16);
        assert_eq!(r.indices, vec![0, 2]);
    }

    /// `SELN` is unsigned, and the rule that makes it unsigned depends on the
    /// SOURCE type — because in C the source type picks the conversion routine.
    #[test]
    fn seln_cast_follows_the_source_type() {
        // Integer source -> `getLongUshort`, `(epicsUInt16)(epicsInt32)v`.
        // C DEFINES this (C17 6.3.1.3p2, modulo 2^16), so we reproduce it.
        assert_eq!(dbr_ushort_cast(&EpicsValue::Long(-1)), 65535);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Long(65536)), 0);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Short(-1)), 65535);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Int64(-1)), 65535);
        assert_eq!(dbr_ushort_cast(&EpicsValue::Long(3)), 3);

        // Float source -> `getDoubleUshort`, `(epicsUInt16)d`. C leaves this
        // UNDEFINED (C17 6.3.1.4p1), and whatever `types::c_cast` decides to do
        // about that is a SEPARATE question from this one — the point here is
        // only that the float source takes the float rule and the integer
        // source does not.
        assert_eq!(
            dbr_ushort_cast(&EpicsValue::Double(-1.0)),
            crate::types::c_cast::f64_to_u16(-1.0)
        );
        assert_eq!(
            dbr_ushort_cast(&EpicsValue::Double(65536.0)),
            crate::types::c_cast::f64_to_u16(65536.0)
        );
        // In range: no policy in play, both rules truncate toward zero.
        assert_eq!(dbr_ushort_cast(&EpicsValue::Double(3.7)), 3);
    }

    /// Whatever produced it, a `SELN` of 65535 selects nothing under Specified
    /// (out of range -> INVALID) and everything under Mask.
    #[test]
    fn seln_at_the_unsigned_maximum_selects_by_selm() {
        use crate::server::record::AlarmSeverity;
        let seln_max = 65535u16;
        // fanout/seq Specified: C `i = (epicsUInt16)seln + offs` = 65535 →
        // out of range → INVALID. A signed read would clamp to 0 and wrongly
        // drive link 0.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, seln_max, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // fanout/seq Mask: 65535 → all 16 low bits set → every link. A signed
        // read would produce an empty mask.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, seln_max, 0, 0, 16);
        assert_eq!(r.indices, (0..16).collect::<Vec<_>>());
        // dfanout Specified: 65535 > count → INVALID. A signed read would see
        // -1 ≤ 0 → drive nothing, with no alarm.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, seln_max, 0, 0, 16);
        assert!(r.indices.is_empty());
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
    }

    /// Lset that flips to "connected" after a configurable delay.
    /// Drives the wait_for_external_links time-budget tests below.
    struct DelayedConnectLset {
        names: Vec<String>,
        connect_at: crate::runtime::task::Instant,
    }

    #[async_trait::async_trait]
    impl link_set::LinkSet for DelayedConnectLset {
        fn is_connected(&self, _: &str) -> bool {
            crate::runtime::task::Instant::now() >= self.connect_at
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn link_names(&self) -> Vec<String> {
            self.names.clone()
        }
    }

    #[epics_macros_rs::epics_test]
    async fn wait_for_external_links_returns_zero_zero_when_no_lsets() {
        let db = PvDatabase::new();
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_millis(50))
            .await;
        assert_eq!((c, t), (0, 0));
    }

    #[epics_macros_rs::epics_test]
    async fn wait_for_external_links_connected_quickly() {
        let db = PvDatabase::new();
        // Local-target forced-CA links (dbChannelTest==0 → isLocal): these
        // get DBCA_CALLBACK_INIT_START, so iocInit waits for them.
        db.add_pv("pv:A", EpicsValue::Long(0)).await.unwrap();
        db.add_pv("pv:B", EpicsValue::Long(0)).await.unwrap();
        let lset = Arc::new(DelayedConnectLset {
            names: vec!["pv:A".to_string(), "pv:B".to_string()],
            connect_at: crate::runtime::task::Instant::now(),
        });
        // Registered under "ca": the iocInit wait is CA-facility only, so
        // the working set comes from the "ca" link set (these forced-CA
        // local-target links), never from a "pva" set.
        db.register_link_set("ca", lset).await;
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_secs(1))
            .await;
        assert_eq!((c, t), (2, 2));
    }

    #[epics_macros_rs::epics_test]
    async fn wait_for_external_links_returns_partial_on_timeout() {
        let db = PvDatabase::new();
        // Local target so the link is in the init-wait set (dbLink.c:130);
        // connect-time well past the budget below, so the wait must return
        // (0, 1) instead of blocking.
        db.add_pv("slow:pv", EpicsValue::Long(0)).await.unwrap();
        let lset = Arc::new(DelayedConnectLset {
            names: vec!["slow:pv".to_string()],
            connect_at: crate::runtime::task::Instant::now() + std::time::Duration::from_secs(60),
        });
        db.register_link_set("ca", lset).await;
        let started = crate::runtime::task::Instant::now();
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_millis(250))
            .await;
        let elapsed = started.elapsed();
        assert_eq!((c, t), (0, 1));
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "wait must consume at least the configured budget, got {:?}",
            elapsed
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "wait must not exceed the budget by much, got {:?}",
            elapsed
        );
    }

    /// C parity (dbLink.c:130): a link whose target is NOT a local record
    /// (`dbChannelTest != 0`) gets no DBCA_CALLBACK_INIT_START, so iocInit
    /// must not block on it. An areaDetector `test CP MS` placeholder — a CP
    /// link to a PV that exists nowhere — must drop straight through, leaving
    /// the link to connect (or dangle) asynchronously and silently, like C.
    #[epics_macros_rs::epics_test]
    async fn wait_for_external_links_skips_nonlocal_targets() {
        let db = PvDatabase::new();
        // "test" has no local record and would never connect.
        let lset = Arc::new(DelayedConnectLset {
            names: vec!["test".to_string()],
            connect_at: crate::runtime::task::Instant::now() + std::time::Duration::from_secs(60),
        });
        db.register_link_set("ca", lset).await;
        let started = crate::runtime::task::Instant::now();
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_secs(10))
            .await;
        // Non-local target is excluded from the wait set entirely, so the
        // call returns (0, 0) immediately rather than blocking the budget.
        assert_eq!((c, t), (0, 0));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "non-local link must not be waited on, got {:?}",
            started.elapsed()
        );
        // And it is reported as unconnected by neither path (silent, like C).
        assert!(db.unconnected_external_links().await.is_empty());
    }

    /// Lset that is connected but whose post-connect init actions (the
    /// metadata fetch) never complete: `init_ready` stays false.
    struct ConnectedMetaPendingLset {
        names: Vec<String>,
    }

    #[async_trait::async_trait]
    impl link_set::LinkSet for ConnectedMetaPendingLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn init_ready(&self, _: &str) -> bool {
            false
        }
        fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn link_names(&self) -> Vec<String> {
            self.names.clone()
        }
    }

    /// epics-base #856 (ef4829829, "dbCa: iocInit wait for all
    /// conditions"): a connected link whose attribute fetch has not
    /// completed still holds iocInit — the wait polls `init_ready`
    /// (C `testInitReady`'s three-bit gate), not `is_connected` alone,
    /// and the timeout diagnostic names the link it proceeded without.
    #[epics_macros_rs::epics_test]
    async fn wait_for_external_links_holds_until_init_ready() {
        let db = PvDatabase::new();
        db.add_pv("meta:pending", EpicsValue::Long(0))
            .await
            .unwrap();
        let lset = Arc::new(ConnectedMetaPendingLset {
            names: vec!["meta:pending".to_string()],
        });
        db.register_link_set("ca", lset).await;
        let (c, t) = db
            .wait_for_external_links(std::time::Duration::from_millis(250))
            .await;
        assert_eq!((c, t), (0, 1));
        assert_eq!(
            db.unconnected_external_links().await,
            vec!["meta:pending".to_string()]
        );
    }

    // epics-base PR #336 — alias parsing + lookup integration tests.

    #[epics_macros_rs::epics_test]
    async fn alias_resolves_through_find_entry() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(42.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS_NAME", "TARGET").await.unwrap();

        // find_entry on the alias must return the same record as
        // find_entry on the target.
        let via_alias = db.find_entry("ALIAS_NAME").await;
        let via_target = db.find_entry("TARGET").await;
        assert!(via_alias.is_some());
        assert!(via_target.is_some());
        // has_name flips true for the alias too.
        assert!(db.has_name("ALIAS_NAME").await);
        assert!(db.has_name("TARGET").await);
        assert!(!db.has_name("NOT:THERE").await);
    }

    /// C answers a SEARCH through `dbChannelTest` (`dbChannel.c:441-464`),
    /// which validates the field: `REC.NOSUCH` is "does not exist", not an
    /// invitation to a CREATE_CHAN the server must then refuse (pvxs#193).
    /// One case per boundary: bare name, real field, missing field, the
    /// alias twin of each, and the `$` modifier's eligibility split.
    #[epics_macros_rs::epics_test]
    async fn search_gate_refuses_a_field_the_record_does_not_have() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(42.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS_NAME", "TARGET").await.unwrap();

        assert!(db.has_name("TARGET.VAL").await);
        assert!(db.has_name("TARGET.SEVR").await);
        assert!(!db.has_name("TARGET.NOSUCH").await);
        // Declared but valueless (`MLOK` is `DBF_NOACCESS`): `dbNameToAddr`
        // resolves it, so the search answers and CREATE is where the
        // refusal lands — measured `pvxget ORACLE:AI.MLOK` → `Refused to
        // create Channel` (see `search_claims_every_dbd_name_but_create_
        // gates_on_a_servable_field` in `epics-pva-rs`).
        assert!(db.has_name("TARGET.MLOK").await);
        // Record-own `DBF_NOACCESS` twin (`waveform.BPTR`): C's resolver
        // does not distinguish common from record-own internals
        // (`dbFindField` walks the type's full `dbFldDes` set), so the
        // same answer-then-refuse applies. The generated tables drop the
        // descs but keep the names (`record_noaccess_fields`).
        db.add_record(
            "WF",
            Box::new(crate::server::records::waveform::WaveformRecord::new(
                8,
                crate::types::DbFieldType::Double,
            )),
        )
        .await
        .unwrap();
        assert!(db.has_name("WF.BPTR").await);
        assert!(!db.has_name("WF.NOSUCH").await);
        assert!(db.has_name("ALIAS_NAME.EGU").await);
        assert!(!db.has_name("ALIAS_NAME.NOSUCH").await);
        // `$` re-views a DBF_STRING/link field as a char array; anything
        // else is `S_dbLib_fieldNotFound` (`dbChannel.c:486-505`).
        assert!(db.has_name("TARGET.EGU$").await);
        assert!(!db.has_name("TARGET.VAL$").await);
    }

    #[epics_macros_rs::epics_test]
    async fn alias_target_must_exist() {
        let db = PvDatabase::new();
        let err = db.add_alias("DANGLING", "MISSING_TARGET").await;
        assert!(err.is_err(), "alias to missing target must be rejected");
    }

    #[epics_macros_rs::epics_test]
    async fn alias_collision_with_existing_record_rejected() {
        let db = PvDatabase::new();
        db.add_record(
            "EXISTING",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_record(
            "OTHER",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        let err = db.add_alias("EXISTING", "OTHER").await;
        assert!(
            err.is_err(),
            "alias name colliding with record must be rejected"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn get_record_resolves_alias() {
        // Regression: get_record must transparently resolve
        // aliases so dbpf / dbgf / dbpr / CA put paths see the same
        // record whether the caller uses the canonical name or the
        // alias.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        let via_canonical = db.get_record("TARGET");
        let via_alias = db.get_record("ALIAS");
        assert!(via_canonical.is_some());
        assert!(via_alias.is_some(), "get_record must resolve alias");
        // Both calls return the same Arc (pointer equality).
        assert!(Arc::ptr_eq(&via_canonical.unwrap(), &via_alias.unwrap()));
    }

    /// `add_record` is the single creation sink: a record added AFTER its
    /// breakpoint table is loaded must receive the registry snapshot so a
    /// `LINR >= 3` conversion resolves — without any explicit per-call-site
    /// `install_breaktable_registry`. This covers the dbCreateRecord and
    /// inline-record creation paths that previously skipped the install.
    #[epics_macros_rs::epics_test]
    async fn add_record_installs_breaktable_registry_from_snapshot() {
        let db = PvDatabase::new();
        let ramp = crate::server::cvt_bpt::BrkTable::build(
            "ramp",
            &[(0.0, 0.0), (100.0, 10.0), (300.0, 30.0)],
        )
        .unwrap();
        db.add_breaktables(vec![ramp]).await;

        let mut rec = crate::server::records::ai::AiRecord::new(0.0);
        rec.put_field("LINR", EpicsValue::Short(15)).unwrap(); // ramp = first user-table index
        db.add_record("AI:BPT", Box::new(rec)).await.unwrap();

        let arc = db.get_record("AI:BPT").unwrap();
        let mut inst = arc.write();
        inst.record.put_field("RVAL", EpicsValue::Long(50)).unwrap();
        inst.record.process().unwrap();
        // raw 50 in [0,100] -> eng 5.0, proving the registry was installed by
        // add_record alone.
        assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(5.0)));
    }

    /// `add_breaktables` re-installs the new snapshot into records that
    /// already exist, so a record created BEFORE its table was loaded (inline
    /// records added before dbLoadRecords; merge-reloads repointing LINR) can
    /// still resolve `LINR >= 3`. Without the re-install the record keeps an
    /// empty registry and never linearises.
    #[epics_macros_rs::epics_test]
    async fn add_breaktables_reinstalls_registry_into_existing_records() {
        let db = PvDatabase::new();
        // Record added while the registry is still empty: add_record installs
        // nothing (the inline-record / pre-load ordering case).
        let mut rec = crate::server::records::ai::AiRecord::new(0.0);
        rec.put_field("LINR", EpicsValue::Short(15)).unwrap(); // ramp = first user-table index
        db.add_record("AI:BPT", Box::new(rec)).await.unwrap();

        // Load the table afterwards — re-install must reach the existing record.
        let ramp = crate::server::cvt_bpt::BrkTable::build(
            "ramp",
            &[(0.0, 0.0), (100.0, 10.0), (300.0, 30.0)],
        )
        .unwrap();
        db.add_breaktables(vec![ramp]).await;

        let arc = db.get_record("AI:BPT").unwrap();
        let mut inst = arc.write();
        inst.record.put_field("RVAL", EpicsValue::Long(50)).unwrap();
        inst.record.process().unwrap();
        assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(5.0)));
    }

    #[epics_macros_rs::epics_test]
    async fn get_record_no_resolve_skips_alias_table() {
        // Strict variant must NOT see aliases — keeps the canonical
        // distinction available for builder code paths.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        assert!(db.get_record_no_resolve("TARGET").is_some());
        assert!(
            db.get_record_no_resolve("ALIAS").is_none(),
            "get_record_no_resolve must not follow alias table"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn register_cp_link_normalises_alias_to_canonical() {
        // Regression: CP link registration must store the
        // canonical record names. dispatch_cp_targets looks up by
        // canonical, so an alias-keyed entry is functionally dead.
        let db = PvDatabase::new();
        db.add_record(
            "SRC_REAL",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_record(
            "DST_REAL",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("SRC_ALIAS", "SRC_REAL").await.unwrap();
        db.add_alias("DST_ALIAS", "DST_REAL").await.unwrap();

        // Register using the alias forms (CP edge: passive_only = false).
        db.register_cp_link("SRC_ALIAS", "DST_ALIAS", false).await;

        // Lookup must succeed via the canonical source name.
        let targets = db.get_cp_targets("SRC_REAL");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].record, "DST_REAL");
        assert!(!targets[0].passive_only);
        // Alias-keyed lookup must NOT have been registered.
        let alias_lookup = db.get_cp_targets("SRC_ALIAS");
        assert!(alias_lookup.is_empty());
    }

    #[epics_macros_rs::epics_test]
    async fn aliases_for_record_returns_sorted_targets_only() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_record(
            "OTHER",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ZZ", "TARGET").await.unwrap();
        db.add_alias("AA", "TARGET").await.unwrap();
        db.add_alias("MM", "OTHER").await.unwrap();

        // Sorted, only TARGET's aliases.
        assert_eq!(
            db.aliases_for_record("TARGET"),
            vec!["AA".to_string(), "ZZ".to_string()]
        );
        // OTHER's alone.
        assert_eq!(db.aliases_for_record("OTHER"), vec!["MM".to_string()]);
        // Unknown record → empty, not None.
        assert!(db.aliases_for_record("MISSING").is_empty());
    }

    #[epics_macros_rs::epics_test]
    async fn all_alias_names_returns_registered_aliases() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS_A", "TARGET").await.unwrap();
        db.add_alias("ALIAS_B", "TARGET").await.unwrap();

        let mut aliases = db.all_alias_names();
        aliases.sort();
        assert_eq!(aliases, vec!["ALIAS_A".to_string(), "ALIAS_B".to_string()]);
        // Canonical names are NOT returned here.
        assert!(!aliases.contains(&"TARGET".to_string()));
    }

    #[epics_macros_rs::epics_test]
    async fn complete_async_record_accepts_alias() {
        // Invariant audit: complete_async_record (the
        // entry point used by async device-support callbacks to
        // finish processing) must accept an alias name. Pre-fix it
        // walked `inner.records` directly and would
        // `ChannelNotFound` if the original name was an alias.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        // Use complete_async_record by alias — must not error.
        db.complete_async_record("ALIAS").await.unwrap();
        // And by canonical too — keeps existing behaviour.
        db.complete_async_record("TARGET").await.unwrap();
    }

    #[epics_macros_rs::epics_test]
    async fn process_record_accepts_alias() {
        // Regression: process_record must accept an alias
        // name. Pre-fix it walked `inner.records` directly.
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        // Both should succeed and reach the same record.
        db.process_record("TARGET").await.unwrap();
        db.process_record("ALIAS").await.unwrap();

        // A bogus name still errors.
        assert!(db.process_record("MISSING").await.is_err());
    }

    #[epics_macros_rs::epics_test]
    async fn process_record_with_links_accepts_alias_and_avoids_cycle() {
        // Regression: process_record_with_links normalises
        // the alias so that (a) the records-map lookup hits and
        // (b) the cycle-detection set doesn't treat alias and
        // canonical as two distinct entries (which would let a
        // self-loop slip past the visited check).
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();

        // The marker's lifetime is the frame, so by the time the call
        // returns the stack is empty and so is the set — see the invariant on
        // `run_process_frame`. What this pins is that the alias resolved to
        // the canonical name on the way IN: seed the set with "TARGET" and the
        // entry must find itself already on the stack and decline.
        let mut visited = std::collections::HashSet::new();
        db.process_record_with_links("ALIAS", &mut visited, 0)
            .await
            .unwrap();
        assert!(
            visited.is_empty(),
            "a finished frame leaves no marker behind: {visited:?}",
        );

        let mut seeded = std::collections::HashSet::new();
        seeded.insert("TARGET".to_string());
        db.process_record_with_links("ALIAS", &mut seeded, 0)
            .await
            .unwrap();
        assert!(
            !seeded.contains("ALIAS"),
            "the alias form must never enter the set: {seeded:?}",
        );
        assert_eq!(
            seeded.len(),
            1,
            "the alias resolved to TARGET and was declined, adding nothing: {seeded:?}",
        );
    }

    #[epics_macros_rs::epics_test]
    async fn alias_duplicate_rejected() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        db.add_alias("ALIAS", "TARGET").await.unwrap();
        // Re-registering the same alias name (even to the same target)
        // must fail — base behaviour: aliases are inserted once.
        let err = db.add_alias("ALIAS", "TARGET").await;
        assert!(err.is_err(), "duplicate alias name must be rejected");
    }

    /// `add_pv`, `add_pv_with_hook`, and `add_record` must
    /// refuse to silently replace an existing registration. Mirrors
    /// epics-base C IOC which treats a duplicate `dbLoadRecords` name
    /// as a fatal load error.
    #[epics_macros_rs::epics_test]
    async fn add_pv_and_add_record_reject_duplicates_across_namespaces() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_pv("A", EpicsValue::Double(1.0)).await.unwrap();
        // Same name as simple_pv — every namespace must see it.
        assert!(db.add_pv("A", EpicsValue::Double(2.0)).await.is_err());
        let noop_hook: crate::server::pv::WriteHook =
            std::sync::Arc::new(|_v, _ctx| Box::pin(async { Ok(()) }));
        assert!(
            db.add_pv_with_hook("A", EpicsValue::Double(2.0), noop_hook)
                .await
                .is_err()
        );
        assert!(
            db.add_record("A", Box::new(AiRecord::new(0.0)))
                .await
                .is_err()
        );
        assert!(db.add_alias("A", "A").await.is_err());

        db.add_record("R", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        assert!(
            db.add_record("R", Box::new(AiRecord::new(1.0)))
                .await
                .is_err()
        );
        assert!(db.add_pv("R", EpicsValue::Double(0.0)).await.is_err());
        assert!(db.add_alias("R", "R").await.is_err());

        db.add_alias("AL", "R").await.unwrap();
        assert!(db.add_pv("AL", EpicsValue::Double(0.0)).await.is_err());
        assert!(
            db.add_record("AL", Box::new(AiRecord::new(0.0)))
                .await
                .is_err()
        );
    }

    /// Removing a record must purge aliases
    /// that pointed AT it. Otherwise the alias name stays
    /// "registered" forever and `add_pv` / `add_record` rejecting
    /// reuse causes a permanent name leak.
    #[epics_macros_rs::epics_test]
    async fn remove_record_purges_dangling_aliases() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("R", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("ALT1", "R").await.unwrap();
        db.add_alias("ALT2", "R").await.unwrap();
        // An alias that points elsewhere must NOT be touched.
        db.add_record("OTHER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("KEEPER", "OTHER").await.unwrap();

        assert!(db.remove_record("R").await);

        // Both aliases pointing at R should be gone — `add_pv` of
        // those names succeeds again.
        db.add_pv("ALT1", EpicsValue::Double(0.0)).await.unwrap();
        db.add_pv("ALT2", EpicsValue::Double(0.0)).await.unwrap();
        // The unrelated alias must survive.
        assert_eq!(db.resolve_alias("KEEPER"), Some("OTHER".to_string()));
    }

    /// The node list is C's `recList`: records and aliases in ONE sequence,
    /// each at the position it was declared at — C draws both from
    /// `pdbbase->no_records++` (`dbStaticLib.c:1704`), which is why an alias
    /// of the first record precedes the second record rather than trailing
    /// every record.
    #[epics_macros_rs::epics_test]
    async fn all_db_nodes_interleaves_aliases_at_their_load_position() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("FIRST", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("FIRST:ALT", "FIRST").await.unwrap();
        db.add_record("SECOND", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        assert_eq!(
            db.all_db_nodes().await,
            vec![
                DbNode {
                    name: "FIRST".into(),
                    alias_of: None
                },
                DbNode {
                    name: "FIRST:ALT".into(),
                    alias_of: Some("FIRST".into())
                },
                DbNode {
                    name: "SECOND".into(),
                    alias_of: None
                },
            ]
        );
    }

    /// The other end of the same sequence: `remove_record` is the only path
    /// that drops an alias, and it must drop the alias's place in the list
    /// with it. A sequence left behind would order the walk against a node
    /// that no longer exists, and a later alias of the same name would then
    /// sort at the dead position instead of its own.
    #[epics_macros_rs::epics_test]
    async fn removing_a_record_drops_its_alias_nodes_from_the_list() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("GONE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("GONE:ALT", "GONE").await.unwrap();
        db.add_record("STAYS", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        assert!(db.remove_record("GONE").await);
        assert_eq!(
            db.all_db_nodes().await,
            vec![DbNode {
                name: "STAYS".into(),
                alias_of: None
            }]
        );

        // Re-registering the freed name puts it at the END of the list, the
        // position its NEW sequence names — not the hole the old one left.
        db.add_record("GONE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("GONE:ALT", "GONE").await.unwrap();
        assert_eq!(
            db.all_db_nodes()
                .await
                .into_iter()
                .map(|node| node.name)
                .collect::<Vec<_>>(),
            vec!["STAYS", "GONE", "GONE:ALT"]
        );
    }

    /// `add_alias` must reject collisions with
    /// every namespace, including simple PVs (which the pre-fix
    /// code missed).
    #[epics_macros_rs::epics_test]
    async fn add_alias_rejects_simple_pv_collision() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_pv("PVX", EpicsValue::Double(0.0)).await.unwrap();
        db.add_record("TARGET", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // alias name "PVX" collides with the simple PV — must fail.
        assert!(db.add_alias("PVX", "TARGET").await.is_err());
    }

    /// Concurrent `add_pv` and `add_record` with
    /// the same name must not deadlock and must serialize so that
    /// exactly one succeeds. Pre-fix the two methods grabbed
    /// different write locks first, opening a cross-lock-order
    /// deadlock window.
    #[epics_macros_rs::epics_test]
    async fn concurrent_add_pv_and_add_record_do_not_deadlock() {
        use crate::server::records::ai::AiRecord;

        let db = std::sync::Arc::new(PvDatabase::new());
        let db1 = db.clone();
        let db2 = db.clone();
        let reactor =
            crate::runtime::task::Reactor::current().expect("the test driver enters an executor");
        let h1 = reactor.spawn(async move { db1.add_pv("RACE", EpicsValue::Double(1.0)).await });
        let h2 = reactor
            .spawn(async move { db2.add_record("RACE", Box::new(AiRecord::new(0.0))).await });
        // Both complete within a reasonable bound — pre-fix this
        // could hang because T1 holds simple_pvs.write and waits
        // for records.read while T2 holds records.write and waits
        // for simple_pvs.read.
        let r1 = crate::runtime::task::timeout(std::time::Duration::from_secs(2), h1)
            .await
            .expect("add_pv must not block on add_record");
        let r2 = crate::runtime::task::timeout(std::time::Duration::from_secs(2), h2)
            .await
            .expect("add_record must not block on add_pv");
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();
        // Exactly one of the two wins; the other reports
        // "already registered".
        assert!(
            (r1.is_ok() && r2.is_err()) || (r1.is_err() && r2.is_ok()),
            "exactly one of the racing inserts must succeed: r1={r1:?} r2={r2:?}",
        );
    }

    #[epics_macros_rs::epics_test]
    async fn existence_gate_blocks_cached_simple_pv_per_request() {
        // A cached simple PV must re-pass the installed existence gate on
        // both the search (`has_name_from`) and create (`find_entry_from`)
        // paths. Records bypass the gate. With no gate the short-circuit
        // is unchanged (plain-IOC behaviour).
        use std::net::SocketAddr;

        let db = PvDatabase::new();
        db.add_pv("SHADOW:x", EpicsValue::Double(1.0))
            .await
            .unwrap();
        db.add_record(
            "REC",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();

        let denied: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let allowed: SocketAddr = "192.0.2.5:5064".parse().unwrap();

        // No gate installed: the cached simple PV resolves unconditionally.
        assert!(db.has_name_from("SHADOW:x", Some(denied)).await);
        assert!(db.find_entry_from("SHADOW:x", Some(denied)).await.is_some());

        // Gate denies the simple PV only for `denied` (the gateway's
        // host-scoped `.pvlist` admission has exactly this shape).
        let gate: ExistenceGate = Arc::new(move |name, peer| {
            Box::pin(async move { !(name == "SHADOW:x" && peer == Some(denied)) })
        });
        db.set_existence_gate(gate).await;

        // Denied peer: does-not-exist on both paths despite the PV being
        // cached in `simple_pvs`.
        assert!(!db.has_name_from("SHADOW:x", Some(denied)).await);
        assert!(db.find_entry_from("SHADOW:x", Some(denied)).await.is_none());

        // Allowed peer: still resolves.
        assert!(db.has_name_from("SHADOW:x", Some(allowed)).await);
        assert!(
            db.find_entry_from("SHADOW:x", Some(allowed))
                .await
                .is_some()
        );

        // Records are never gateway-managed — the gate must not gate them
        // even for the denied peer.
        assert!(db.has_name_from("REC", Some(denied)).await);
        assert!(db.find_entry_from("REC", Some(denied)).await.is_some());
    }

    /// The declaration-keyed sweep sees every name the three hand lists it
    /// replaced spelled, and the names they did NOT spell.
    ///
    /// The old lists were `COMMON_LINK_FIELDS`, `Record::multi_input_links`
    /// and `links::CP_INPUT_LINK_FIELDS`; the third is deleted, so its content
    /// is repeated here as the oracle rather than read from it. It carried one
    /// hard-won fact worth keeping visible — `SVL` and not `SGNL`, because
    /// `histogramRecord.dbd.pod:212` declares `field(SVL,DBF_INLINK)` while
    /// `SGNL` (:202) is the `DBF_DOUBLE` the link reads INTO. The declaration
    /// now carries that by itself, which is the point: this asserts it does.
    #[test]
    fn the_declared_class_sweep_covers_every_name_the_hand_lists_spelled() {
        use crate::types::{DbfLinkClass, dbf_link_class};

        // (name, the type that declares it, the class it must resolve to).
        let cp_inputs: &[(&str, &str)] = &[
            ("DOL", "ao"),
            ("DOL0", "seq"),
            ("DOLF", "seq"),
            ("DOL1", "sseq"),
            ("DOLA", "sseq"),
            ("NVL", "sel"),
            ("SELL", "sseq"),
            ("SVL", "histogram"),
        ];
        for (field, record_type) in cp_inputs {
            assert_eq!(
                dbf_link_class(record_type, field),
                Some(DbfLinkClass::InLink),
                "{record_type}.{field} was a CP_INPUT_LINK_FIELDS name and must \
                 still resolve as an input link"
            );
        }
        // SGNL is the value, not the link — the bug the old list carried.
        assert_eq!(dbf_link_class("histogram", "SGNL"), None);

        for (field, _) in crate::server::record::record_instance::COMMON_LINK_FIELDS {
            assert!(
                dbf_link_class("ai", field).is_some() || dbf_link_class("ao", field).is_some(),
                "{field} was a COMMON_LINK_FIELDS name and must still resolve"
            );
        }

        // The names no hand list spelled, which is why five of seven probed
        // record shapes lost lock-set members: `dblsr` on softIoc R7.0.10 put
        // a `fanout` and its `LNK1` target in one set, the port did not.
        let unspelled: &[(&str, &str, DbfLinkClass)] = &[
            ("fanout", "LNK1", DbfLinkClass::FwdLink),
            ("dfanout", "OUTA", DbfLinkClass::OutLink),
            ("ai", "SIML", DbfLinkClass::InLink),
            ("ai", "SIOL", DbfLinkClass::InLink),
            ("aSub", "SUBL", DbfLinkClass::InLink),
            ("aSub", "OUTA", DbfLinkClass::OutLink),
            ("seq", "LNK1", DbfLinkClass::OutLink),
        ];
        for (record_type, field, class) in unspelled {
            assert_eq!(
                dbf_link_class(record_type, field),
                Some(*class),
                "{record_type}.{field}"
            );
        }
    }

    /// Every link field a record declares reaches `link_field_texts`, and
    /// nothing else does.
    ///
    /// The boundary the name lists could not hold: a `fanout`'s `LNK1` is
    /// `DBF_FWDLINK` while an `sseq`'s `LNK1` is `DBF_OUTLINK`, so the same
    /// spelling is two classes and only the declaration can tell them apart.
    #[epics_macros_rs::epics_test]
    async fn link_field_texts_is_the_declared_link_set() {
        use crate::server::record::LinkFieldType;
        use crate::server::records::fanout::FanoutRecord;

        let db = PvDatabase::new();
        db.add_record("FAN", Box::new(FanoutRecord::default()))
            .await
            .unwrap();
        {
            let rec = db.get_record("FAN").unwrap();
            let mut inst = rec.write();
            inst.record
                .put_field("LNK1", EpicsValue::String("TARGET".into()))
                .unwrap();
        }
        let inst = db.get_record("FAN").unwrap();
        let inst = inst.read();
        let fields = PvDatabase::link_field_texts(&inst);
        let lnk1 = fields
            .iter()
            .find(|(f, _, _)| f == "LNK1")
            .unwrap_or_else(|| panic!("fanout.LNK1 must be enumerated, got {fields:?}"));
        assert_eq!(lnk1.1, "TARGET");
        assert!(
            matches!(lnk1.2, LinkFieldType::Fwd),
            "fanout.LNK1 is DBF_FWDLINK, got {:?}",
            lnk1.2
        );
        assert!(
            !fields.iter().any(|(f, _, _)| f == "VAL"),
            "a non-link field must not be enumerated, got {fields:?}"
        );
    }

    /// `record_link_fields` must surface a record's device-support `INP`
    /// link. An `ai`'s `INP` is a `DBF_INLINK` field stored in
    /// `common.inp` — it is not a `DbFieldType::String` entry in
    /// `field_list()` — so the earlier `field_list()` scan filtered by
    /// `String` silently dropped it. The pvalink install scan walks this
    /// method, so a Passive `ai` carrying a CP/CPP pvalink `INP` never had
    /// its monitor opened at iocInit. Enumerating the canonical
    /// `common.inp` storage fixes it; C `dbpvar`/`dbcar` likewise dump
    /// every link field including device-support INP/OUT.
    #[epics_macros_rs::epics_test]
    async fn record_link_fields_surfaces_device_support_inp() {
        use crate::server::record::ParsedLink;
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        // A `pva` link set has to be installed for a `pva://` link to survive
        // `db_init_link_locality`, which refuses a link whose scheme nothing
        // can service. This test is about the ENUMERATION reaching
        // `common.inp`, so it installs the lset the link names rather than
        // asserting the refusal.
        db.register_link_set(
            "pva",
            std::sync::Arc::new(DelayedConnectLset {
                names: Vec::new(),
                connect_at: crate::runtime::task::Instant::now(),
            }),
        )
        .await;
        db.add_record("AI", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // Device-support INP lives in `common.inp` (DBF_INLINK), the
        // exact storage a `field_list()` String scan cannot reach.
        {
            let rec = db.get_record("AI").unwrap();
            rec.write().common.inp = "pva://mini:current?proc=CP".to_string();
        }

        let links = db.record_link_fields("AI");
        let inp = links
            .iter()
            .find(|(f, _, _)| f == "INP")
            .unwrap_or_else(|| panic!("INP link must be surfaced, got {links:?}"));
        assert_eq!(inp.1, "pva://mini:current?proc=CP");
        assert!(
            matches!(inp.2, ParsedLink::Pva(_)),
            "a pva:// INP must parse to ParsedLink::Pva, got {:?}",
            inp.2
        );
    }

    /// The watchdog table an operator compares against C's: C `iocBuild`
    /// calls `dbCaLinkInit` (`iocInit.c:216`), so `dbCaLink` is one of the
    /// threads a C IOC lists whether or not any link is external. The port
    /// started the owner from the first staged external link, so an IOC whose
    /// output links are all local listed no `dbCaLink` at all.
    ///
    /// Asserted with no link ever staged — a test that staged one first would
    /// pass on the lazy path too.
    // RTEMS-EXEC-MODEL-ALLOW(1): needs the ambient reactor because that is
    // exactly what the test is about — `ca_link_init` starts nothing without
    // one. Green on the exec backend.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ioc_init_starts_the_ca_link_owner() {
        fn table() -> String {
            let out = std::cell::RefCell::new(String::new());
            crate::runtime::taskwd::taskwd_show(1, &|line| {
                out.borrow_mut().push_str(line);
                out.borrow_mut().push('\n');
            });
            out.into_inner()
        }

        assert!(
            !table().contains("dbCaLink"),
            "the link owner was registered before any IOC init"
        );

        let db = PvDatabase::new();
        db.ioc_init().await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !table().contains("dbCaLink") {
            assert!(
                std::time::Instant::now() < deadline,
                "`dbCaLink` never reached the watchdog table:\n{}",
                table()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// The other boundary: a database that captured no reactor has nowhere to
    /// run the owner's network work, which is the same condition
    /// `stage_external_link_open` already refuses on. Starting a watchdog-
    /// registered task that can never do its job would be worse than the
    /// missing row — the table would claim a working link owner.
    // The `exec_backend` executor is not the ambient tokio reactor, and
    // `BlockingBridge::try_capture` finds it with no runtime entered — so
    // there is no reactor-less database to test in that configuration.
    #[cfg(tokio_backend)]
    #[test]
    fn ca_link_init_starts_nothing_without_a_reactor() {
        let db = PvDatabase::new();
        assert!(
            !db.ca_link_init(),
            "a database with no captured reactor must refuse to start the owner"
        );

        let out = std::cell::RefCell::new(String::new());
        crate::runtime::taskwd::taskwd_show(1, &|line| {
            out.borrow_mut().push_str(line);
            out.borrow_mut().push('\n');
        });
        assert!(
            !out.into_inner().contains("dbCaLink"),
            "the refused owner still reached the watchdog table"
        );
    }
}
