pub(crate) mod breakpoint;
mod ca_client;
mod ca_link;
pub mod channel_io;
pub mod char_view;
#[cfg(test)]
mod cycle_plan_tests;
pub mod db_access;
pub mod dbr_delivery;
mod field_io;
pub mod filters;
mod io_intr_turn;
mod link_check;
mod link_put_queue;
mod link_set;
mod links;
mod processing;
#[cfg(test)]
mod reads_leak_tests;
pub(crate) use processing::{AlarmPosts, alarm_field_posts};
mod record_lock;
mod record_table;
pub(crate) mod scan_index;
mod snapshot;
mod subroutine_context;

pub use crate::server::record::OwnRecord;
pub use ca_client::{
    CaClientContext, CaClientService, ClientChannel, ClientFuture, ClientMonitor, Connection,
    InProcessChannel, InProcessEvents, InProcessSubscription, PutCompletion,
};
pub use ca_link::CA_LINK_EVENT_MASK;
pub use field_io::{
    HeldPuts, HeldRecord, LockedRecord, ProcessMode, PutSplit, RestoreTarget, UnheldLinkPut,
};
pub(crate) use field_io::{
    PutSrc, db_access_debug_putf, render_for_string_target, set_db_access_debug_putf,
};
pub use link_put_queue::CpWorker;
pub(crate) use link_put_queue::{LinkId, LinkInstall};
pub use link_set::{
    DynLinkSet, EnumString, LinkBacking, LinkDbfType, LinkDiagnostics, LinkForward,
    LinkForwardRefused, LinkHolder, LinkMetadata, LinkOpen, LinkPutDone, LinkPutOp, LinkRef,
    LinkSerial, LinkSet, LinkSetRegistry, LinkShape, PutAdmission, RemoteAlarm, link_put_convert,
    report_put_not_issued,
};
#[cfg(test)]
pub(crate) use links::ExternalDone;
pub(crate) use links::{
    LinkAlarm, ProcessTargetGate, inherit_sevr_msg, multi_out_rows, multi_output_dispatch_owned,
    posts_software_event, reads_sell, resolves_subroutine_from_link,
};
pub use processing::{
    AsyncDbHandle, AsyncToken, CaLinkInit, DbAddr, DeviceDone, DeviceDoneSource, ExternalCpProcess,
    FieldPut, HeldInstance, InitHold, InputLinkTexts, PostItem, PostRule, ProcStack,
    ProcessCallback, TokenWatch, hop_floor,
};
pub(crate) use processing::{
    DelayedCallbackTimers, NotifiedRead, ReadPolicy, seed_record_constants,
};
#[cfg(feature = "test-hooks")]
pub use record_lock::fresh_hold_hook;
pub(crate) use record_lock::{
    CycleHints, FrameGate, InFrame, InitPass, LockRecord, OwedRead, ReadWait, SetGuard,
    owe_callback, owe_cvt, owe_read,
};
pub use record_lock::{
    LockSetInfo, LockSetReport, ManyRecordWriteGuard, RecordWriteGuard, in_sync_driver_wait,
    scan_lock_held, sync_driver_wait,
};
pub(crate) use record_table::RecordIdx;
pub(crate) use subroutine_context::RecordDb;
pub use subroutine_context::SubroutineContext;

use crate::error::{CaError, CaResult};
use arc_swap::{ArcSwap, ArcSwapOption};
use snapshot::SnapshotCell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::server::pv::ProcessVariable;
use crate::server::record::{Record, RecordCell, RecordInstance};
use crate::types::{EpicsValue, PvString, RecordName};

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
/// The record half is [`DbNameParts`]' record part, to the first `.` as C
/// `dbFindRecordPart` (`dbChannel.c:180-200`) has it; the field half is the
/// rest unread, for the caller's own field lookup. A last-`.` split tore a
/// filter such as `{"d":1.5}` apart, so a name like `REC.VAL{"d":1.5}`
/// found no record.
///
/// The record half is bytes, as `dbNameToAddr` hashes it. Every field name is
/// an ASCII identifier, so a field part that is not UTF-8 names none and is
/// given back as U+FFFD, which no field lookup matches.
pub fn parse_pv_name<N: AsRef<[u8]> + ?Sized>(name: &N) -> (&[u8], &str) {
    let name = name.as_ref();
    let record = DbNameParts::of(name).record;
    match name.get(record.len() + 1..) {
        Some(rest) => (record, std::str::from_utf8(rest).unwrap_or("\u{fffd}")),
        None => (name, "VAL"),
    }
}

/// C `dbNameToAddr`'s reading of a name (`dbAccess.c:666-683`): the record
/// part runs to the first `.` (`dbFindRecordPart`), the field part is the C
/// identifier after it, empty meaning `VAL` (`dbFindFieldPart`), and the one
/// modifier read is the single character after that, so `LSI.VAL$x` and
/// `LSI.$` are both `VAL$` and an undotted `LSI$` is a record name.
pub struct DbNameParts<'a> {
    pub record: &'a [u8],
    pub field: &'a str,
    pub tail: &'a [u8],
}

impl<'a> DbNameParts<'a> {
    pub fn of(pname: &'a [u8]) -> Self {
        let Some(dot) = pname.iter().position(|&c| c == b'.') else {
            return Self {
                record: pname,
                field: "",
                tail: b"",
            };
        };
        let (record, rest) = (&pname[..dot], &pname[dot + 1..]);
        let len = match rest.first() {
            Some(&c) if c == b'_' || c.is_ascii_alphabetic() => rest
                .iter()
                .position(|&c| c != b'_' && !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len()),
            _ => 0,
        };
        Self {
            record,
            // An identifier: ASCII by the scan above.
            field: std::str::from_utf8(&rest[..len]).unwrap_or_default(),
            tail: &rest[len..],
        }
    }

    /// The field `dbFindFieldPart` looks up, `VAL` when none is given.
    pub fn field_or_val(&self) -> &'a str {
        if self.field.is_empty() {
            "VAL"
        } else {
            self.field
        }
    }

    /// The one modifier read is `$` (`dbAccess.c:682`).
    pub fn char_view(&self) -> bool {
        self.tail.first() == Some(&b'$')
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
        Time(crate::types::WallTime, u64),
        /// Every other `TSEL` (`recGbl.c:322`): `dbGetLink(&prec->tsel,
        /// DBR_SHORT, &prec->tse, 0, 0)`.
        Tse(i16),
        /// The `dbGetLink` of a non-`.TIME` TSEL failed: `TSE` keeps its own
        /// value and the stamp raises `setLinkAlarm`, at the stamp point
        /// because that is where C's read is.
        Failed,
        /// This pass calls no `recGblGetTimeStamp`: a
        /// [`CycleHooks::withholds_cycle_stamp`](crate::server::record::CycleHooks::withholds_cycle_stamp)
        /// type whose pass did not ask. `TSEL` was not read and nothing is
        /// stored.
        Withheld,
    }

    impl TselStamp {
        /// C `recGblGetTimeStampSimm`'s body: store what `TSEL` yielded, then
        /// resolve `TSE`→`TIME`. `simm_set` answers C's `simm != menuSimmNO`
        /// and is called only on the `TSE == -2` arm, so a record that never
        /// uses device time pays nothing for it: with no SIOL to take a stamp
        /// from (`siol == NULL`, every output record) a simulating `SIMM`
        /// stamps the current time (`recGbl.c:331-342`). `false` is
        /// [`Self::Withheld`], which stores nothing.
        #[inline(always)]
        pub(crate) fn stamp(
            self,
            name: &[u8],
            common: &mut CommonFields,
            simm_set: impl FnOnce() -> bool,
        ) -> bool {
            let begun = common.begin_stamp();
            self.stamp_begun(begun, name, common, simm_set)
        }

        /// [`Self::stamp`] for a cycle that opened its stamp earlier, so a
        /// device or INP-source time could land in between as C's `readValue`
        /// does after `process()` took `timeLast`.
        #[inline(always)]
        pub(crate) fn stamp_begun(
            self,
            _begun: crate::server::record::StampBegun,
            name: &[u8],
            common: &mut CommonFields,
            simm_set: impl FnOnce() -> bool,
        ) -> bool {
            match self {
                TselStamp::Time(time, utag) => {
                    common.time = time;
                    common.utag = utag;
                    // C's `return` (`recGbl.c:321`), which is the whole reason
                    // the event lookup does not run. Writing `TSE = -2` to get
                    // the same effect made the record report a value its `.db`
                    // never declared, and overwrote one that did.
                    return true;
                }
                TselStamp::Withheld => return false,
                TselStamp::None => {}
                TselStamp::Tse(tse) => common.set_tse(tse),
                TselStamp::Failed => {
                    crate::server::recgbl::rec_gbl_set_link_alarm(common, "TSEL");
                }
            }
            match common.stamp_mode() {
                crate::server::record::StampMode::Plain => {
                    common.time = crate::runtime::general_time::get_current();
                }
                crate::server::record::StampMode::DeviceTime => device_time(common, simm_set),
                // A TSEL link may just have loaded any TSE, -2 included.
                crate::server::record::StampMode::Event
                | crate::server::record::StampMode::FromTsel => {
                    if common.tse() == crate::server::record::EPICS_TIME_EVENT_DEVICE_TIME {
                        device_time(common, simm_set);
                    } else {
                        apply_timestamp(name, common);
                    }
                }
            }
            true
        }
    }

    /// Apply timestamp to a record based on its TSE field, `TSE != -2`.
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
    #[inline]
    fn apply_timestamp(name: &[u8], common: &mut CommonFields) {
        // `stamp` has taken `TSE == -2` already, so this is `get_time_stamp`'s
        // other arm; the two share `recgbl::event_time` and cannot drift.
        match crate::server::recgbl::event_time(common.tse()) {
            Some(t) => common.time = t,
            None => tse_failed(name, common.tse()),
        }
    }

    /// `TSE == -2`: device support already wrote `time`; only a simulating
    /// `SIMM` replaces it.
    #[cold]
    #[inline(never)]
    fn device_time(common: &mut CommonFields, simm_set: impl FnOnce() -> bool) {
        if simm_set() {
            common.time = crate::types::WallTime::now();
        }
    }

    #[cold]
    fn tse_failed(name: &[u8], tse: i16) {
        crate::runtime::log::errlog_vprintf(format_args!(
            "recGblGetTimeStampSimm: epicsTimeGetEvent failed, {}.TSE = {tse}\n",
            String::from_utf8_lossy(name)
        ));
    }
}

/// Unified entry in the PV database.
pub enum PvEntry {
    Simple(Arc<ProcessVariable>),
    Record(Arc<RecordCell>),
}

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
            RecordName,
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
            RecordName,
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
/// One CP/CPP edge in the `PvDatabaseInner::external_cp_links` index: the
/// record to (re)process when the link's channel posts.
///
/// `passive_only` distinguishes CPP from CP. C adds the `CA_DBPROCESS`
/// action for a CP link unconditionally, but for a CPP link only when the
/// link-holding record's `SCAN` is Passive (`dbCa.c:825`, `:959`, `:1034`). CP
/// edges clear the flag; CPP edges set it, and `process_one_cp_target`
/// honours it.
#[derive(Clone, Debug, PartialEq)]
pub struct CpTarget {
    pub record: RecordName,
    pub passive_only: bool,
}

/// What one CP/CPP link watches: the channel it names, CPP's gate, and
/// the link, whose own channel's events process its holder.
#[derive(Clone, Debug)]
pub(crate) struct CpSource {
    pub(crate) record: RecordName,
    pub(crate) passive_only: bool,
    pub(crate) link: LinkSerial,
}

/// The CP/CPP edges, each owned by the link that makes it: C gives every
/// link its own `caLink`, so an edge is keyed by its holder and link field
/// and [`Self::set_link`] replaces it — a repeated registration or a relink
/// moves the edge, never adds a second.
#[derive(Clone, Debug, Default)]
pub(crate) struct CpRegistry {
    /// Edges fired when the source posts.
    by_source: HashMap<RecordName, Vec<CpTarget>>,
    /// (holder, link field) → that link's edge.
    by_link: HashMap<(RecordName, String), CpSource>,
    /// The link's serial → its holder: what the link's own channel processes
    /// (`dbCa.c:958-962`).
    by_serial: HashMap<LinkSerial, CpTarget>,
    /// iocInit's link pass has run: from here a relink adds its own link,
    /// as C `dbAddLink` does once `initDatabase` has run.
    links_initialized: bool,
}

impl CpRegistry {
    pub(crate) fn targets(&self, source: &[u8]) -> Option<&Vec<CpTarget>> {
        self.by_source.get(source)
    }

    /// The holder `link`'s events process, while `link` is a CP/CPP link.
    pub(crate) fn link_target(&self, link: LinkSerial) -> Option<&CpTarget> {
        self.by_serial.get(&link)
    }

    pub(crate) fn sources(&self) -> impl Iterator<Item = &RecordName> {
        self.by_source.keys()
    }

    pub(crate) fn links_initialized(&self) -> bool {
        self.links_initialized
    }

    pub(crate) fn mark_links_initialized(&mut self) {
        self.links_initialized = true;
    }

    /// The one writer: the link `field` of `holder` now carries `edge`, or
    /// none.
    pub(crate) fn set_link(&mut self, holder: &[u8], field: &str, edge: Option<CpSource>) {
        let key = (RecordName::from(holder), field.to_string());
        if let Some(old) = self.by_link.remove(&key) {
            self.by_serial.remove(&old.link);
            if let Some(edges) = self.by_source.get_mut(&old.record) {
                let is_old =
                    |t: &CpTarget| *t.record == *holder && t.passive_only == old.passive_only;
                if let Some(i) = edges.iter().position(is_old) {
                    edges.remove(i);
                }
                if edges.is_empty() {
                    self.by_source.remove(&old.record);
                }
            }
        }
        if let Some(edge) = edge {
            let target = CpTarget {
                record: key.0.clone(),
                passive_only: edge.passive_only,
            };
            self.by_source
                .entry(edge.record.clone())
                .or_default()
                .push(target.clone());
            self.by_serial.insert(edge.link, target);
            self.by_link.insert(key, edge);
        }
    }

    /// Every edge `name` holds or sources.
    pub(crate) fn remove_record(&mut self, name: &[u8]) {
        self.by_source.remove(name);
        for edges in self.by_source.values_mut() {
            edges.retain(|e| *e.record != *name);
        }
        self.by_source.retain(|_, edges| !edges.is_empty());
        self.by_link
            .retain(|(holder, _), edge| **holder != *name && *edge.record != *name);
        let by_link = &self.by_link;
        self.by_serial
            .retain(|serial, _| by_link.values().any(|edge| edge.link == *serial));
    }
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
///
/// The key also carries the instance it names. A sweep otherwise hands the
/// frame a `&str` and the frame hashes it back into the records map once per
/// record per cycle, re-deriving what the list already knew. It is held
/// STRONGLY: a key leaving the bucket and its record being destroyed are one
/// operation (`remove_record_entry` takes the key out as it destroys the
/// instance), and a handle that outlives its key is caught by `is_destroyed`
/// under the data lock the frame takes anyway. A `Weak` here was a second,
/// weaker encoding of the same fact, and it cost a `compare_exchange` loop per
/// record per scan cycle to decode — 6% of the server's cycles.
///
/// `Ord`/`Eq` are the four sort fields alone — the handle is what the key
/// names, not part of which key it is — so the ordering contract above is
/// unchanged and a key built for a lookup need not carry one.
#[derive(Clone)]
struct ScanKey {
    phas: i16,
    /// The type's name: C's `recordTypeList` is name-sorted (`dbdExpand.pl`
    /// writes the expanded `.dbd` sorted, `DBD/Output.pm:61`), so a type
    /// registered at runtime sorts among the declared ones as there.
    record_type: &'static str,
    load_order: u64,
    /// An `Arc` and not a `String` because the cursor hands this name out once
    /// per record per scan cycle, and a `String` there made every sweep
    /// allocate — twice, since the cursor step clones the key as well.
    /// `Ord` on the bytes is `str`'s on a UTF-8 name, so the sort rule above
    /// is unchanged.
    name: RecordName,
    /// The instance `name` resolves to — see the type doc. `None` only for a
    /// key built before its record was registered, or for a lookup key.
    handle: Option<Arc<RecordCell>>,
}

impl ScanKey {
    fn sort_fields(&self) -> (i16, &'static str, u64, &[u8]) {
        (self.phas, self.record_type, self.load_order, &self.name)
    }
}

// Hand-written because the handle is an `Arc<RecordCell>`, and printing a
// record cell would take its lock from inside a formatter.
impl std::fmt::Debug for ScanKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanKey")
            .field("phas", &self.phas)
            .field("record_type", &self.record_type)
            .field("load_order", &self.load_order)
            .field("name", &self.name)
            .field("handle", &self.handle.is_some())
            .finish()
    }
}

impl PartialEq for ScanKey {
    fn eq(&self, other: &Self) -> bool {
        self.sort_fields() == other.sort_fields()
    }
}

impl Eq for ScanKey {}

impl PartialOrd for ScanKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScanKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_fields().cmp(&other.sort_fields())
    }
}

impl ScanKey {
    fn new(
        phas: i16,
        record_type: &'static str,
        load_order: u64,
        name: impl Into<RecordName>,
        handle: Option<Arc<RecordCell>>,
    ) -> Self {
        Self {
            phas,
            record_type,
            load_order,
            name: name.into(),
            handle,
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
/// The database's alias table entry — C's alias `dbRecordNode`.
///
/// C allocates an alias node into the target's own record list and numbers it
/// from the counter a record draws from (`pnewnode->order =
/// pdbentry->pdbbase->no_records++`, `dbStaticLib.c:1703-1704`), which is why
/// an alias appears at its declaration point in a `dbl` walk and not after
/// every record. This port keeps aliases in a map of their own, so the node's
/// two facts — what it points at, where it was declared — are this struct.
struct AliasNode {
    /// The canonical record name the alias resolves to.
    target: RecordName,
    /// Load sequence, from the same counter as [`RecordCell::load_order`].
    order: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbNode {
    /// The name this node is reached by — an alias node carries the ALIAS
    /// name, which is what C prints from `dbGetRecordName`.
    pub name: RecordName,
    /// The record an alias node stands for, or `None` for a record node.
    pub alias_of: Option<RecordName>,
}

/// C `afterIocRunning.c`'s `cmdList` and `initEndFlag` (`:36-37`) as one
/// value: lines queue until the first `initHookAfterIocRunning` has run them,
/// and never after.
enum AfterIocRunning {
    Queuing(std::collections::VecDeque<Vec<u8>>),
    Ran,
}

/// Why [`PvDatabase::queue_after_ioc_running`] did not queue a line, in C
/// `afterIocRunningFunc`'s order of asking (`afterIocRunning.c:75-86`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterIocRunningRefusal {
    /// The queue has already run (`initEndFlag`).
    AfterIocInit,
    /// No command, or an empty one.
    Empty,
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
        crate::runtime::sync::PriorityInheritanceMutex<HashMap<RecordName, Arc<ProcessVariable>>>,
    /// Moved by every `simple_pvs` insert and remove, after it, through
    /// [`PvDatabase::insert_simple_pv`] / [`PvDatabase::take_simple_pv`]: a
    /// simple PV spelled `REC.FLD` shadows that field, so a cached link
    /// address must not outlive one appearing.
    simple_pvs_revision: std::sync::atomic::AtomicU64,
    /// Wake-up for servers holding channels on this database: something was
    /// removed, re-check what you serve. Carries no name — the receiver
    /// tests the target it already holds
    /// ([`ProcessVariable::is_destroyed`] /
    /// [`RecordInstance::is_destroyed`]), so an alias, a `.FIELD` suffix or
    /// a `.{filter}` suffix on the client's channel name cannot make the
    /// match miss the way a name comparison would.
    pv_destroyed_tx: tokio::sync::broadcast::Sender<()>,
    /// Every registered record, keyed by its canonical name.
    ///
    /// The key is an `Arc<str>` because it is the ONE allocation of that name:
    /// `PvDatabase::lookup_record` hands it out and every hop of a process
    /// chain — the cycle guard, the scan key, the lock-set lookup — carries a
    /// share of it rather than a copy. A name-keyed map that owned `String`s
    /// made a scan step allocate the name of every record it touched.
    records: RecursiveReadLock<HashMap<RecordName, Arc<RecordCell>>>,
    /// Every record registered, by [`RecordIdx`]: where a resolved forward
    /// link finds its target without the name map or a count.
    record_table: record_table::RecordTable,
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
    /// C `pdbbase->no_records` (`dbStaticLib.c:1704`) — the one counter every
    /// node of the database draws its load sequence from as it is created:
    /// records ([`RecordCell::load_order`]) and alias nodes
    /// ([`AliasNode::order`]) alike, which is what interleaves the two in the
    /// list `dbl` walks.
    ///
    /// The sequences live ON the nodes. They used to live in a copy-on-write
    /// `SnapshotCell<HashMap<String, u64>>` beside them, which cloned the
    /// whole map once per record added — `dbLoadRecords` was quadratic in
    /// record count — and let a sequence outlive its node or go missing from
    /// it, which every reader answered for with an `unwrap_or` fallback.
    /// A sequence is now a field of the node it belongs to, so neither is
    /// representable.
    load_order_counter: std::sync::atomic::AtomicU64,
    /// CP/CPP link index: maps the PV a link set serves (`OTHER:PV` from
    /// `INP="OTHER:PV CP"`, a local record's name included, since C makes
    /// every CP/CPP link a CA link) → holder edges to process when that
    /// channel posts. The only trigger is the calink/pvalink monitor
    /// callback, which requests it through an [`ExternalCpProcess`] — C
    /// `dbCa.c:958-962` `eventCallback` adding `CA_DBPROCESS`.
    ///
    /// Read-modify-write cell with **two writers that share no other gate**:
    /// `CpRegistry::set_link` takes no [`Self::registration_mutex`],
    /// `remove_record` does, so the [`SnapshotCell`] writer gate is required.
    external_cp_links: SnapshotCell<CpRegistry>,
    /// C `dbCaRun`'s gate over the CP/CPP workers.
    ca_run: std::sync::Arc<processing::CaRunGate>,
    /// The I/O Intr pumps' turns a local CA link's first event waits for.
    io_intr_turns: io_intr_turn::IoIntrTurns,
    /// Alias map: alternate-name → the [`AliasNode`] it names. Mirrors
    /// epics-base PR #336 (alias name validation + parsing). `find_entry` and
    /// related lookups consult this map after the canonical record
    /// table so an alias resolves transparently to its target.
    aliases: RecursiveReadLock<HashMap<RecordName, AliasNode>>,
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
    ///
    /// Unlisted: C has no such gate (its record load is single-threaded), so
    /// `epicsMutexShowAll` has no entry for it.
    registration_mutex: crate::runtime::sync::UnlistedPriorityInheritanceMutex<()>,
    /// The IOC lifecycle phase — the port's `iocInit` boundary. See
    /// [`DbInitPhase`], [`PvDatabase::begin_load`],
    /// [`PvDatabase::schedule_record_init`] and [`PvDatabase::ioc_init`].
    init_phase: std::sync::Mutex<DbInitPhase>,
    /// Record inits parked because the record they classify is not registered
    /// yet — see [`PvDatabase::schedule_record_init`]. Keyed by record name and
    /// released by [`PvDatabase::add_loaded_record`] the moment that name lands
    /// in `records`, which is what makes "the init observes a registered
    /// record" hold by construction rather than by scheduler timing.
    record_init_waiting: std::sync::Mutex<HashMap<RecordName, Vec<RecordInit>>>,
    /// Records whose device-support binding and `init_record` passes are OWED
    /// to [`PvDatabase::ioc_init`] because they were created during the LOAD
    /// phase — C's `dbLoadRecords` links a record into `pdbbase` at once but
    /// runs `init_record` only at `iocInit`. [`PvDatabase::add_loaded_record`]
    /// used to bind the dset and run the passes eagerly at load time, so a
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
    empty_link_assignments: std::sync::Mutex<HashMap<RecordName, Vec<String>>>,
    /// Claimed by the one [`PvDatabase::db_init_record_links`] pass.
    links_prepared: std::sync::atomic::AtomicBool,
    /// The iocsh `afterIocRunning` queue; see [`AfterIocRunning`].
    after_ioc_running: std::sync::Mutex<AfterIocRunning>,
    /// Optional async resolver invoked on `has_name` misses (e.g. CA gateway).
    ///
    /// Whole-value replace: the only writer stores a complete new value, so
    /// an [`ArcSwapOption`] store IS the mutation and no writer gate is
    /// needed. Readers take the `Arc` with no lock at all (§3 row L8g).
    search_resolver: ArcSwapOption<SearchResolver>,
    /// Optional per-request gate consulted before a *cached* simple PV is
    /// advertised as existing (e.g. CA gateway host/state admission). See
    /// [`ExistenceGate`]. `None` for a plain IOC (short-circuit unchanged).
    ///
    /// Whole-value replace, as [`Self::search_resolver`] (§3 row L8h).
    existence_gate: ArcSwapOption<ExistenceGate>,
    /// The database debugger, installed by the first `dbb` and removed when
    /// its last breakpoint goes — C's `lset_stack_count`, which `dbProcess`
    /// tests before it calls either breakpoint hook (`dbAccess.c:504`,
    /// `:614`).
    ///
    /// Whole-value replace, as [`Self::search_resolver`]: a database nobody
    /// is debugging pays one relaxed atomic load per processed record where C
    /// pays one comparison, and the mechanism stays testable and removable
    /// instead of welded into `process_record_with_links_body` as a pair of
    /// `if` branches.
    breakpoints: ArcSwapOption<breakpoint::BreakpointTable>,
    /// Whether anything is being debugged — C's `lset_stack_count != 0`
    /// (`dbAccess.c:504`), the test both breakpoint hooks make on every
    /// processed record.
    ///
    /// Derived from [`Self::breakpoints`], and written only by the two
    /// functions that write that slot, so the pair cannot disagree. It exists
    /// because the hooks ask a yes/no question and `ArcSwapOption::load_full`
    /// answers it by cloning an `Arc` behind a hazard pointer — twice per
    /// record per scan cycle for a database nobody is debugging, which is the
    /// one comparison C pays turned into two.
    ///
    /// Ordered so a stale read is always the harmless one: the flag is raised
    /// BEFORE the table is installed and lowered AFTER it is dropped, so a
    /// reader can see `true` with no table (and find nothing to do) but never
    /// `false` with a live table.
    debugging: std::sync::atomic::AtomicBool,
    /// Per-scheme link sets — pluggable backends for `pva` / `ca` link
    /// resolution, the only route `resolve_external_pv` takes.
    /// Mirrors the C-EPICS lset abstraction.
    ///
    /// Read-modify-write cell (`register_link_set` inserts one scheme into
    /// the existing registry), so it takes the [`SnapshotCell`] writer gate.
    /// Every reader either resolves one scheme inside a single expression or
    /// collects the lsets and drops the registry **before** awaiting — the
    /// deliberate discipline documented at `links.rs:951-952` — so a coherent
    /// snapshot is what the read paths already assumed.
    link_sets: SnapshotCell<link_set::LinkSetRegistry>,
    /// The `ca` scheme's entry in `link_sets`, which the network lset
    /// registered under `ca` sits behind.
    ca_links: Arc<ca_link::CaLinks>,
    /// Pending external OUT-link writes — the `dbCa` `workList` analogue.
    /// Record processing stages a write here and returns; the queue's single
    /// owner task performs the `ca`/`pva` network write off the
    /// record's advisory write gate, exactly as `dbCaTask` does
    /// (`dbCa.c:1093-1260`). See [`link_put_queue`].
    link_puts: Arc<link_put_queue::LinkPutQueue>,
    /// True once the ScanScheduler has been started for this DB.
    /// Prevents duplicate scan tasks when multiple protocol servers (CA + PVA)
    /// both try to start scanning on the same DB.
    /// This database's `scanCtl` and the scan-start claim; see
    /// [`ScanControl`](crate::server::scan::ScanControl).
    scan_ctl: Arc<crate::server::scan::ScanControl>,
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
    /// (`dbStaticLib.c:1232-1277`) adds or overwrites from there. A type
    /// holds an entry here once `dbPutAttribute` first touched it, starting
    /// from that seed; a type without one answers [`seeded_record_attribute`].
    /// Both maps
    /// are `BTreeMap`s because C keeps the inner list in `strcmp` order and
    /// walks it in that order in `dbGetAttributePart` and
    /// `dbDumpRecordType`; sorting is the container's job here rather than an
    /// insertion dance.
    ///
    /// A published snapshot: a name lookup reads it under a record's hold and
    /// on the search side, so a reader takes no lock. Writers serialise on
    /// [`Self::record_attributes_write`].
    record_attributes: ArcSwap<RecordAttributes>,
    /// Unlisted: C's `attributeList` is written without a lock.
    record_attributes_write: crate::runtime::sync::UnlistedPriorityInheritanceMutex<()>,
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
    subroutine_registry: ArcSwap<HashMap<String, Arc<crate::server::record::Subroutine>>>,
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
    /// `pdbbase->linkList`. Read with no lock because a link put asks it with
    /// the record held; written only by a `.dbd` load or a binary's
    /// declaration.
    link_list: SnapshotCell<crate::server::record::LinkList>,
}

thread_local! {
    /// Set for exactly as long as this thread holds L46. Read only by
    /// [`PvDatabase::lock_registration`].
    static REGISTRATION_GATE_HELD: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Whether this thread is inside a [`RegistrationGate`] window — the L46
/// half of the check a fresh lock-set acquisition makes in debug builds
/// (`record_lock.rs`, `LockSet::acquire`).
pub(super) fn registration_gate_held() -> bool {
    REGISTRATION_GATE_HELD.with(|h| h.get().is_some())
}

thread_local! {
    /// How many [`RecursiveReadLock`] read guards this thread holds. Read by
    /// `LockSet::lock_fresh` (`record_lock.rs`) in debug builds.
    static MAP_READ_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether this thread holds a read guard on the records map or the alias
/// table — the other half of the check a fresh lock-set acquisition makes
/// in debug builds (`record_lock.rs`, `LockSet::lock_fresh`), beside
/// [`registration_gate_held`].
pub(super) fn map_read_held() -> bool {
    MAP_READ_DEPTH.with(|d| d.get() > 0)
}

/// A reader-writer lock whose readers never queue behind a waiting writer.
///
/// The records map and the alias table are read from inside a record's lock
/// set — every `get_record` on the process path — and written only by the
/// registration entry points. With a fair `RwLock`, a reader arriving while
/// a writer waits is parked, and that parks a thread holding a lock set: if
/// the reader the writer is waiting on then wants that same set (a
/// registration-path walk that reads records), the three are wedged. With
/// `read_recursive` a reader is parked only by an ACTIVE writer, so a
/// reader that already holds a set is never wedged by a writer.
///
/// The writer side needs the converse rule. `remove_record_entry` holds the
/// removed record's lock set while it takes the write lock — its `destroy`
/// must be exclusive against a process cycle — so a reader that takes a
/// FRESH lock set under its read guard can be the reader that writer waits
/// on, each holding what the other wants. **A read guard MUST NOT be held
/// across a lock-set acquisition**: clone the `Arc<RecordCell>` out under
/// the guard, drop the guard, then lock the record. This is the one place
/// readers of these two maps are handed out, and every guard it hands out
/// counts itself on the thread, so `LockSet::lock_fresh` can fail on the
/// thread that broke the rule instead of wedging two threads later.
pub(crate) struct RecursiveReadLock<T> {
    lock: parking_lot::RwLock<T>,
    /// Bumped under every write lock, so a reader can tell whether the map
    /// may have changed since it last looked — what a cached link target
    /// ([`crate::server::record::record_instance::LinkTargetResolver`])
    /// checks instead of re-resolving a name every cycle.
    revision: std::sync::atomic::AtomicU64,
}

impl<T> RecursiveReadLock<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            lock: parking_lot::RwLock::new(value),
            revision: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn read(&self) -> MapReadGuard<'_, T> {
        let guard = self.lock.read_recursive();
        MAP_READ_DEPTH.with(|d| d.set(d.get() + 1));
        MapReadGuard { guard }
    }

    /// Bumps the revision once the write lock is held, so a reader that
    /// loaded the revision before reading the map either saw the old number
    /// or waited for this write to finish.
    pub(crate) fn write(&self) -> parking_lot::RwLockWriteGuard<'_, T> {
        let guard = self.lock.write();
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        guard
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// A [`RecursiveReadLock`] read guard, counted on the thread that holds it
/// for as long as it lives — see [`map_read_held`].
pub(crate) struct MapReadGuard<'a, T> {
    guard: parking_lot::RwLockReadGuard<'a, T>,
}

impl<T> std::ops::Deref for MapReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> Drop for MapReadGuard<'_, T> {
    fn drop(&mut self) {
        MAP_READ_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// The database's end is every record's removal — `remove_record`'s rule,
/// removal IS destruction, applied to the records the map still holds. What
/// lets the cells drop with the map: a link target handle holds its target's
/// `Arc`, so two records linked to each other hold each other until
/// [`RecordInstance::destroy`] lets go.
impl Drop for PvDatabaseInner {
    fn drop(&mut self) {
        for rec in self.records.lock.get_mut().values() {
            rec.write().destroy();
        }
    }
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

/// The link slots a fanout, dfanout or seq has: `LNK0..LNKF`, `OUTA..OUTP`.
pub(crate) const SELM_SLOTS: usize = 16;

/// Result of resolving a SELM/SELN selection.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SelmResult {
    /// Bit `i` drives link slot `i`: a selection is resolved every cycle, so
    /// it is held in place rather than in a list.
    pub selected: u16,
    /// `Some` when C would raise an alarm for an out-of-range
    /// `SELN`/`OFFS`/`SHFT`. C uses `recGblSetSevr(prec, SOFT_ALARM,
    /// INVALID_ALARM)` in every such path.
    pub alarm: Option<(u16, crate::server::record::AlarmSeverity)>,
}

impl SelmResult {
    /// The selected slots, ascending, as C's loops drive them.
    pub(crate) fn indices(self) -> SelectedSlots {
        SelectedSlots(self.selected)
    }
}

/// The set bits of a selection, lowest first. Exact-size, so seq's group
/// list is sized once rather than grown slot by slot.
pub(crate) struct SelectedSlots(u16);

impl Iterator for SelectedSlots {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.0 == 0 {
            return None;
        }
        let i = self.0.trailing_zeros() as usize;
        self.0 &= self.0 - 1;
        Some(i)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.0.count_ones() as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for SelectedSlots {}

/// Select which link indices are active based on SELM/SELN, applying
/// the record-type-specific `OFFS`/`SHFT` bias.
///
/// SELM: 0 = All, 1 = Specified, 2 = Mask, over [`SELM_SLOTS`] link slots.
///
/// `seln` is the native `DBF_USHORT` value: C declares `SELN` as
/// `epicsUInt16`, so every comparison below is unsigned, matching C's
/// selection arithmetic — never `-1`. What an out-of-range `SELL` converts
/// *to* is the `DBR_USHORT` link read's decision, not this function's.
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
) -> SelmResult {
    use crate::server::recgbl::alarm_status::SOFT_ALARM;
    use crate::server::record::AlarmSeverity;

    let count = SELM_SLOTS;
    let invalid = || SelmResult {
        selected: 0,
        alarm: Some((SOFT_ALARM, AlarmSeverity::Invalid)),
    };
    let ok = |selected: u16| SelmResult {
        selected,
        alarm: None,
    };

    match selm {
        // All — every slot.
        0 => ok(u16::MAX),
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
                    ok(1 << i)
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
                    ok(0)
                } else {
                    ok(1 << (seln_i - 1))
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
            // Bits shifted past the last slot drive nothing.
            ok(mask as u16)
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

type RecordAttributes =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

/// C `dbReadCOM`'s tail (`dbLexRoutines.c:311-331`): every record type a
/// `.dbd` declared carries `RTYP` = its own name and `VERS` =
/// `"none specified"`. The port's types come from the generated table and
/// from other crates' registrations, so the seed is answered for a type with
/// no table entry rather than written for a list of types up front.
fn seeded_record_attribute<'a>(record_type: &'a str, name: &str) -> Option<&'a str> {
    match name {
        "RTYP" => Some(record_type),
        "VERS" => Some("none specified"),
        _ => None,
    }
}

/// [`seeded_record_attribute`]'s whole list, for the entry a first
/// `dbPutAttribute` on `record_type` creates.
fn seeded_record_attributes(record_type: &str) -> std::collections::BTreeMap<String, String> {
    ["RTYP", "VERS"]
        .into_iter()
        .filter_map(|name| {
            let value = seeded_record_attribute(record_type, name)?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// Raised before the first breakpoint table or TPRO lands in any database,
/// never lowered: the process frame's one test for both debug hooks, so a
/// process nobody debugs pays a single relaxed load per record.
static DEBUG_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`DEBUG_ARMED`]'s one writer, called before a breakpoint table or a TPRO
/// is written.
pub(crate) fn arm_debugging() {
    DEBUG_ARMED.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[inline]
pub(crate) fn debug_armed() -> bool {
    DEBUG_ARMED.load(std::sync::atomic::Ordering::Relaxed)
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
            inner: Arc::new_cyclic(|weak| {
                let ca_run: Arc<processing::CaRunGate> = Default::default();
                let ca_links = Arc::new(ca_link::CaLinks::new(weak.clone(), ca_run.clone()));
                let mut link_sets = link_set::LinkSetRegistry::new();
                link_sets.register("ca", ca_links.clone());
                PvDatabaseInner {
                    simple_pvs: crate::runtime::sync::PriorityInheritanceMutex::new(
                        HashMap::new(),
                        crate::runtime::sync::CMutex::PvdBucket,
                    ),
                    simple_pvs_revision: std::sync::atomic::AtomicU64::new(0),
                    pv_destroyed_tx: tokio::sync::broadcast::channel(16).0,
                    breakpoints: ArcSwapOption::empty(),
                    debugging: std::sync::atomic::AtomicBool::new(false),
                    search_resolver: ArcSwapOption::empty(),
                    existence_gate: ArcSwapOption::empty(),
                    link_sets: SnapshotCell::new(link_sets),
                    ca_links,
                    link_puts: Arc::new(link_put_queue::LinkPutQueue::default()),
                    records: RecursiveReadLock::new(HashMap::new()),
                    record_table: record_table::RecordTable::default(),
                    scan_index: scan_index::ScanIndex::new(),
                    load_order_counter: std::sync::atomic::AtomicU64::new(0),
                    external_cp_links: SnapshotCell::new(CpRegistry::default()),
                    ca_run,
                    io_intr_turns: Default::default(),
                    aliases: RecursiveReadLock::new(HashMap::new()),
                    registration_mutex: crate::runtime::sync::UnlistedPriorityInheritanceMutex::new(
                        (),
                    ),
                    init_phase: std::sync::Mutex::new(DbInitPhase::Unloaded),
                    record_init_waiting: std::sync::Mutex::new(HashMap::new()),
                    empty_link_assignments: std::sync::Mutex::new(HashMap::new()),
                    links_prepared: std::sync::atomic::AtomicBool::new(false),
                    after_ioc_running: std::sync::Mutex::new(AfterIocRunning::Queuing(
                        std::collections::VecDeque::new(),
                    )),
                    scan_ctl: Arc::new(crate::server::scan::ScanControl::new()),
                    pini_done: std::sync::atomic::AtomicBool::new(false),
                    pini_notify: tokio::sync::Notify::new(),
                    record_locks: record_lock::RecordLockRegistry::default(),
                    record_attributes: ArcSwap::from_pointee(RecordAttributes::new()),
                    record_attributes_write:
                        crate::runtime::sync::UnlistedPriorityInheritanceMutex::new(()),
                    subroutine_registry: ArcSwap::from_pointee(HashMap::new()),
                    device_support_resolver: ArcSwapOption::empty(),
                    breaktable_registry: SnapshotCell::new(
                        crate::server::cvt_bpt::BreakTableRegistry::new(),
                    ),
                    link_list: SnapshotCell::new(crate::server::record::LinkList::base()),
                }
            }),
        }
    }

    /// C `dbPutAttribute` (`dbAccess.c:436-460`) minus its shell wrapper:
    /// set or create one attribute of one record type.
    ///
    /// C's `!pdbbase` arm is unreachable here — the record-type set is the
    /// generated table plus every registered type, which exist before any
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
        if !crate::server::db_loader::record_type_visible(record_type) {
            return Err(RecordAttributeError::RecordTypeNotFound);
        }
        // C truncates by byte (`strncpy` then `[MAX_STRING_SIZE-1] = 0`),
        // which can leave a partial UTF-8 sequence; truncating by character
        // keeps the same 39 for every name iocsh can pass and never produces
        // a value that is not a string.
        let truncated: String = value.chars().take(MAX_ATTRIBUTE_LEN).collect();
        let _write = self.inner.record_attributes_write.lock();
        let mut next = RecordAttributes::clone(&self.inner.record_attributes.load());
        next.entry(record_type.to_string())
            .or_insert_with(|| seeded_record_attributes(record_type))
            .insert(name.to_string(), truncated);
        self.inner.record_attributes.store(Arc::new(next));
        Ok(())
    }

    /// C `dbGetAttributePart` (`dbStaticLib.c:1279-1315`) for a whole name —
    /// the fallback `pvNameLookup` takes when `dbFindFieldPart` answers
    /// `S_dbLib_fieldNotFound` (`dbChannel.c:326-327`).
    ///
    /// The caller owes the shadowing test: C reaches this only after the
    /// record type's declared field list has missed, so a type that declares
    /// a field of the same name (`motor.VERS`) hides the attribute.
    ///
    /// `record_type` is a record's own type, so it is one the IOC knows.
    pub fn record_type_attribute(&self, record_type: &str, name: &str) -> Option<String> {
        match self.inner.record_attributes.load().get(record_type) {
            Some(attrs) => attrs.get(name).cloned(),
            None => seeded_record_attribute(record_type, name).map(str::to_string),
        }
    }

    /// [`Self::record_type_attribute`]'s existence half, copying nothing.
    pub(crate) fn has_record_type_attribute(&self, record_type: &str, name: &str) -> bool {
        match self.inner.record_attributes.load().get(record_type) {
            Some(attrs) => attrs.contains_key(name),
            None => seeded_record_attribute(record_type, name).is_some(),
        }
    }

    /// One record type's whole attribute list in C's `strcmp` order — the
    /// order `dbPutRecordAttribute` inserts in and `dbDumpRecordType` prints.
    pub fn record_type_attributes(&self, record_type: &str) -> Vec<(String, String)> {
        match self.inner.record_attributes.load().get(record_type) {
            Some(attrs) => attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            None if crate::server::db_loader::record_type_visible(record_type) => {
                seeded_record_attributes(record_type).into_iter().collect()
            }
            None => Vec::new(),
        }
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
        // Record data is behind the record's lock set, which sits ABOVE L46
        // in the acquisition order; the gate has done its job (registry
        // update and map snapshot are one transaction) and is released
        // before any record is opened. A record registered from here on
        // installs the snapshot itself in `add_loaded_record`.
        drop(_gate);
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
        registry: HashMap<String, Arc<crate::server::record::Subroutine>>,
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

    /// C `dbDTYPtoDevSup` for a device-link put: a fresh device object for
    /// `ctx`, built from the table [`Self::install_device_support_resolver`]
    /// installed, or `None` when none serves it.
    pub(crate) fn resolve_device_support(
        &self,
        ctx: &crate::server::ioc_app::DeviceSupportContext,
    ) -> Option<Box<dyn crate::server::device_support::DeviceSupport>> {
        self.inner
            .device_support_resolver
            .load()
            .as_deref()
            .and_then(|resolve| resolve(ctx))
    }

    /// How many records hold device support. Derived from the records
    /// themselves rather than tallied by a wiring pass, because there is no
    /// longer a wiring pass: the bind happens at each record's creation.
    #[cfg(test)]
    pub(crate) async fn records_with_device_support(&self) -> usize {
        let names = self.all_record_names();
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
    ) -> Option<Arc<crate::server::record::Subroutine>> {
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

    /// This database's scan control cell: C's one `scanCtl`, scoped to the
    /// database so one process can run several without any of them
    /// reaching another's facility.
    pub fn scan_control(&self) -> &Arc<crate::server::scan::ScanControl> {
        &self.inner.scan_ctl
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
    async fn simple_pv_gate_denies(&self, name: &[u8], peer: Option<std::net::SocketAddr>) -> bool {
        let Some(gate) = self.inner.existence_gate.load_full() else {
            return false;
        };
        let gate = (*gate).clone();
        // Strip the channel-filter suffix exactly as the lookups do
        // (CA-FR-8) so the gate sees the same record-path key the
        // simple-PV map and the gateway cache are keyed on.
        let record_path = RecordName::from(filters::split_channel_name(name).record_path);
        // Own statement: the `!Send` guard must be down before the gate's
        // `.await` below (see the `simple_pvs` field doc).
        let known = self.inner.simple_pvs.lock().contains_key(&record_path);
        if !known {
            return false;
        }
        !gate(record_path, peer).await
    }

    /// The database debugger, or `None` when nothing is being debugged.
    ///
    /// The hot-path read behind both breakpoint hooks: C's
    /// `if (lset_stack_count)` guard in `dbProcess`.
    pub(crate) fn breakpoints(&self) -> Option<Arc<breakpoint::BreakpointTable>> {
        self.inner.breakpoints.load_full()
    }

    /// [`Self::breakpoints`] for the two hooks in the process frame, behind
    /// [`debug_armed`]: C's
    /// `if (lset_stack_count)` first, and the table only if it answers yes.
    /// See [`PvDatabaseInner::debugging`].
    pub(crate) fn breakpoints_if_debugging(&self) -> Option<Arc<breakpoint::BreakpointTable>> {
        if !self
            .inner
            .debugging
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        self.breakpoints()
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
        // Raised before the table lands — see [`PvDatabaseInner::debugging`].
        arm_debugging();
        self.inner
            .debugging
            .store(true, std::sync::atomic::Ordering::Relaxed);
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
            // Lowered after the table is gone — see [`PvDatabaseInner::debugging`].
            self.inner
                .debugging
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Register a [`LinkSet`] under `scheme` (e.g. `"pva"` /
    /// `"ca"`). The lset is consulted for `ParsedLink::Pva` /
    /// `ParsedLink::Ca` link reads/writes. A later call for the same
    /// scheme replaces it for reads and writes, but a forward link already
    /// opened keeps the first lset's handle: its slot is set once.
    ///
    /// The `ca` scheme is this database's own link set, which serves every
    /// name the database does: an lset registered as `ca` becomes its
    /// network half, reached only for the names it does not.
    pub async fn register_link_set(&self, scheme: &str, lset: link_set::DynLinkSet) {
        if scheme == "ca" {
            self.inner.ca_links.set_network(lset);
        } else {
            self.inner.link_sets.update(|r| r.register(scheme, lset));
        }
        // A forward link loaded after `iocInit` with no lset in place is
        // opened now.
        self.open_unopened_forwards();
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
    ) -> Vec<(String, PvString, crate::server::record::LinkFieldType)> {
        use crate::server::record::LinkFieldType;
        use crate::types::DbfLinkClass;
        let record_type = inst.record.record_type();
        let mut out: Vec<(String, PvString, LinkFieldType)> = Vec::new();
        // `dbCommon` first, then the record's own, which is C's `papFldDes`
        // order; a record type that redeclares a `dbCommon` field keeps the
        // first entry, since that is the one `declared_field` resolves.
        for desc in crate::server::record::declared_fields(record_type) {
            let ftype = match crate::types::link_class_of(desc) {
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
            // A link the type leaves to the framework (mca's SIML/SIOL) keeps
            // its text on the instance, not the rset: `link_text` reads both.
            let raw = match inst.common_link_text(desc.name) {
                Some(raw) => raw.into(),
                None => match inst.link_text(desc.name) {
                    Some(text) => text,
                    None if inst.record.get_field(desc.name).is_some() => PvString::new(),
                    // Declared but held by no owner — a downstream type whose
                    // generated table outruns its `get_field`. C would have
                    // the `dbFldDes` and an empty link; the port has no text
                    // to offer, so the field is simply absent rather than
                    // reported as an empty link.
                    None => continue,
                },
            };
            out.push((desc.name.to_string(), raw, ftype));
        }
        out
    }

    /// The parse each link field of `inst` holds, for a field with text: the
    /// one an lset is addressed through ([`LinkRef`]).
    pub(crate) fn installed_links(
        inst: &RecordInstance,
    ) -> Vec<(String, Arc<crate::server::record::ParsedLink>)> {
        Self::link_field_texts(inst)
            .into_iter()
            .filter(|(_, raw, _)| !raw.is_empty())
            .filter_map(|(field, _, _)| {
                let link = inst.stored_link_shared(&field)?.clone();
                Some((field, link))
            })
            .collect()
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
    /// field text. An external link the record holds with no lset — removed,
    /// or left so by its `openLink` — is the [`ParsedLink::Removed`] the
    /// record holds, which no consumer opens, scans or counts.
    ///
    /// [`ParsedLink::Removed`]: crate::server::record::ParsedLink::Removed
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
        record_name: impl AsRef<[u8]>,
    ) -> Vec<(String, PvString, crate::server::record::ParsedLink)> {
        let record_name = record_name.as_ref();
        let rec = match self.get_record(record_name) {
            Some(r) => r,
            None => return Vec::new(),
        };
        // The texts leave the hold before any is parsed. An external link
        // the record holds is reported as held: the parse is the `caLink`,
        // and a copy of the text would be another link to its lset.
        let (texts, held) = {
            let inst = rec.read();
            let texts = Self::link_field_texts(&inst);
            let held: Vec<_> = texts
                .iter()
                .map(|(field, _, _)| {
                    inst.stored_link(field)
                        .filter(|l| {
                            l.external().is_some()
                                || matches!(l, crate::server::record::ParsedLink::Removed(_))
                        })
                        .cloned()
                })
                .collect();
            (texts, held)
        };
        let (mut out, ftypes): (
            Vec<(String, PvString, crate::server::record::ParsedLink)>,
            Vec<_>,
        ) = texts
            .into_iter()
            .zip(held)
            .filter(|((_, raw, _), _)| !raw.is_empty())
            .map(|((field, raw, ftype), held)| {
                let parsed = crate::server::record::parse_link_field(&raw, ftype);
                ((field, raw, parsed), (ftype, held))
            })
            .filter(|((_, _, parsed), _)| {
                !matches!(parsed, crate::server::record::ParsedLink::None)
            })
            .unzip();
        // Apply C `dbInitLink`'s locality fallthrough once, here, so no
        // consumer re-derives it. Done after dropping the record-instance
        // guard: the locality query reads the database's record map, and
        // this is the only place that would otherwise hold an instance lock
        // across it.
        for (entry, (ftype, held)) in out.iter_mut().zip(ftypes) {
            entry.2 = self.db_init_link_locality(
                std::mem::replace(&mut entry.2, crate::server::record::ParsedLink::None),
                ftype,
            );
            // A held external link stays external, as C decides locality
            // once (`dbLink.c:96-100`); a removed one replaces only an
            // external view, since a removed local link keeps its parse here
            // for the lock-set walk.
            if let Some(held) = held
                && (held.external().is_some() || entry.2.external().is_some())
            {
                entry.2 = held;
            }
        }
        out
    }

    /// Resolve an external PV name through the one lset the link is
    /// addressed to
    /// ([`ExternalRef::lset`](crate::server::record::ExternalRef::lset)). No
    /// other lset is asked — C picks one lset per link and never searches
    /// across them (`dbLink.c:107-130`).
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
    pub(crate) fn resolve_external_pv(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> Option<EpicsValue> {
        let (scheme, lref) = Self::external_link_ref(link);
        if let Some(v) =
            link_put_queue::find_lset(&self.inner, scheme, |lset| lset.get_cached_value(lref))
        {
            return Some(v);
        }
        self.stage_external_link_open(link);
        None
    }

    /// `link` as the one lset it is addressed to addresses it.
    pub(crate) fn external_link_ref(
        link: crate::server::record::ExternalRef<'_>,
    ) -> (&'static str, LinkRef<'_>) {
        (link.lset.scheme(), LinkRef::new(link.name, link.open))
    }

    /// Whether `scheme`'s lset can take `link`: the `ca` set only for a
    /// name it can connect ([`ca_link::CaLinks::addresses`]).
    pub(crate) fn link_addressable(&self, scheme: &str, link: LinkRef<'_>) -> bool {
        match scheme {
            "ca" => self.inner.ca_links.addresses(link),
            s => self.inner.link_sets.load().get_ref(s).is_some(),
        }
    }

    /// How many staged writes from `record`'s link `field` to the external
    /// link `link_name` a later write on the same link overwrote before it
    /// reached the wire — C's `pca->nNoWrite`, which `dbcar` prints beside
    /// the link (`dbCaTest.c:111-116`).
    ///
    /// `link_name` is a CA link's PV name as the record carries it: `dbcar`
    /// reports CA links alone. C keeps the count per `caLink`, i.e. per link
    /// field, and
    /// so does the queue: two records whose OUT links name one PV have two
    /// counters.
    pub fn external_link_puts_coalesced_for(
        &self,
        record: impl AsRef<[u8]>,
        field: &str,
        link_name: impl AsRef<[u8]>,
    ) -> u64 {
        let Some(rec) = self.get_record(record) else {
            return 0;
        };
        self.inner.link_puts.coalesced_for(
            rec.index(),
            field,
            crate::server::record::LinkSetTarget::Ca.scheme(),
            link_name.as_ref(),
        )
    }

    /// C `dbCaLinkInit` (`dbCa.c:322-346`), which `iocBuild` calls before it
    /// announces `initHookAfterCaLinkInit`: the `dbCaLink` worker exists from
    /// init, and only a build or this call starts it — never record
    /// processing, which would spawn it under the record's hold. A database
    /// no build has initialised calls it itself, as a C test calls
    /// `testIocInitOk`; until then its external opens and puts wait queued.
    ///
    /// Returns false, having started nothing, when this database captured no
    /// reactor: the owner's network work has nowhere to run, which is the same
    /// condition an external link open already refuses on.
    pub fn ca_link_init(&self) -> bool {
        if self.inner.link_puts.network().is_none() {
            return false;
        }
        if let Some(started) = self
            .inner
            .link_puts
            .start_owner(std::sync::Arc::downgrade(&self.inner))
        {
            started.wait();
        }
        true
    }

    /// Run a link's [`ExternalCpProcess::run_worker`] on the `dbCaLink`
    /// thread, where C `dbCaTask` runs `CA_DBPROCESS` (`dbCa.c:1314-1321`).
    pub fn spawn_cp_worker(
        &self,
        worker: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> CpWorker {
        self.inner.link_puts.spawn_cp_worker(worker)
    }

    /// C `dbCaLinkInitIsolated`'s half beyond [`Self::ca_link_init`]: a CA
    /// link reaches only the names this database serves.
    pub(crate) fn ca_link_isolate(&self) {
        self.inner.ca_links.isolate();
    }

    /// C `dbCaShutdown` (`dbCa.c:324-333`), from `iocShutdown` before
    /// `initHookAfterStopLinks`: the `dbCaLink` worker drains and is joined.
    pub(crate) fn ca_link_shutdown(&self) {
        self.inner.link_puts.shutdown();
    }

    /// Single owner of the "this external link needs opening" transition —
    /// the `addAction(pca, CA_CONNECT)` C reaches through `dbCaAddLink`
    /// (`dbCa.c:397-401`), which is `dbCa.c:393` inside
    /// `dbCaAddLinkCallback` (`:373-395`).
    ///
    /// Every caller routes through here so the open runs on the link work
    /// owner and nowhere else; no path may call
    /// [`link_set::LinkSet::connect_link`] directly from a record-processing
    /// thread. Once the queue holds the link's open, whoever staged it, the
    /// link's latch says so, and from then on a cycle that misses reads the
    /// latch and nothing else — C stages one `CA_CONNECT` per `caLink`.
    ///
    /// Gated on an lset existing for the link: the queue's open state is
    /// terminal, so an open staged while no lset is registered would be
    /// serviced against an empty lset list and marked `Done`, burning the
    /// link's one connect. Returns true when this call is the one that staged
    /// it (false when already staged, or when nothing could open it).
    pub(crate) fn stage_external_link_open(
        &self,
        link: crate::server::record::ExternalRef<'_>,
    ) -> bool {
        if link.open.is_set() || link.open.is_removed() {
            return false;
        }
        let (scheme, body) = (link.lset.scheme(), link.name);
        if !self.link_addressable(scheme, LinkRef::new(body, link.open)) {
            return false;
        }
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
        // C `dbInitLink` adds a local CA link with `DBCA_CALLBACK_INIT_START`
        // (`dbLink.c:127-129`), which `dbCaRun` waits for. Registered as the
        // open is queued, so nothing the open does can report it first.
        //
        // Deviation: C's `dbChannelTest` passes a local name whose filter
        // chain `dbChannelCreate` refuses, and `dbCaRun` then waits forever
        // for a channel that never connects (C bug: iocInit hangs). Such a
        // link is not waited for.
        let local_ca = scheme == "ca"
            && self.has_name_no_resolve(body)
            && filters::split_channel_name(body)
                .json_suffix
                .is_none_or(|json| filters::try_parse_filter_chain(&json).is_ok());
        let staged = self.inner.link_puts.stage_open(
            link_put_queue::Channel::new(scheme, body, link.open),
            || local_ca.then(|| self.inner.ca_run.register()).flatten(),
        );
        link.open.set();
        staged
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
    fn insert_simple_pv(&self, name: RecordName, pv: Arc<ProcessVariable>) {
        self.inner.simple_pvs.lock().insert(name, pv);
        self.inner
            .simple_pvs_revision
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    fn take_simple_pv(&self, name: &[u8]) -> Option<Arc<ProcessVariable>> {
        let removed = self.inner.simple_pvs.lock().remove(name);
        self.inner
            .simple_pvs_revision
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        removed
    }

    pub(crate) fn simple_pvs_revision(&self) -> u64 {
        self.inner
            .simple_pvs_revision
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub async fn add_pv(&self, name: impl AsRef<[u8]>, initial: EpicsValue) -> CaResult<()> {
        let _gate = self.lock_registration("add_pv");
        let name = RecordName::from(name.as_ref());
        self.check_name_free(&name)?;
        let pv = Arc::new(ProcessVariable::new(name.clone(), initial));
        self.insert_simple_pv(name, pv);
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
        name: impl AsRef<[u8]>,
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
        name: impl AsRef<[u8]>,
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
        name: impl AsRef<[u8]>,
        initial: EpicsValue,
        write_hook: crate::server::pv::WriteHook,
        access_hook: Option<crate::server::pv::AccessHook>,
        read_hook: Option<crate::server::pv::ReadHook>,
    ) -> CaResult<()> {
        let _gate = self.lock_registration("add_pv_with_hooks_full");
        let name = RecordName::from(name.as_ref());
        self.check_name_free(&name)?;
        let pv = Arc::new(ProcessVariable::new(name.clone(), initial));
        pv.set_write_hook(write_hook);
        if let Some(access) = access_hook {
            pv.set_access_hook(access);
        }
        if let Some(read) = read_hook {
            pv.set_read_hook(read);
        }
        self.insert_simple_pv(name, pv);
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
    pub async fn remove_simple_pv(&self, name: impl AsRef<[u8]>) -> Option<Arc<ProcessVariable>> {
        let name = name.as_ref();
        let _gate = self.lock_registration("remove_simple_pv");
        // Simple PVs cannot be alias targets (aliases point at
        // records), but a stale alias whose name MATCHES this PV
        // would have been rejected at add_alias time. No alias
        // cleanup needed for simple-PV removal.
        let removed = self.take_simple_pv(name);
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
        self.check_name_free(pv.name.as_bytes())?;
        let name = pv.name.clone();
        self.insert_simple_pv(name, Arc::new(pv));
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

    /// Is a load under way that `iocInit` has not yet finished? A CA client
    /// in the IOC cannot connect to the IOC's own records then, because the
    /// server is not up; a database built without a load never waits.
    pub fn ioc_init_pending(&self) -> bool {
        matches!(
            *self.inner.init_phase.lock().unwrap(),
            DbInitPhase::Loading(_) | DbInitPhase::Initialising(_)
        )
    }

    /// Schedule a record's link-status classification — the port's
    /// `init_record` tail (C `checkLinks`).
    ///
    /// During the LOAD phase the future is QUEUED for [`Self::ioc_init`]; a
    /// half-built database cannot be classified against because the code that
    /// would do it has not been polled. Before any load, and once `iocInit` has
    /// run, it is spawned at once — which is what a runtime `special()` link
    /// re-point needs — except a new record's first, which `add_record` runs
    /// before it returns, as `iocInit` does a loaded record's.
    ///
    /// # `record` is what the init classifies, and it must exist first
    ///
    /// A record's first classification is issued from `set_async_context`,
    /// which [`Self::add_loaded_record`] calls *before* it inserts the record
    /// into `records` — the handle has to exist for `run_init_pass0` to use
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
        record: &[u8],
        init: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        if !self.inner.records.read().contains_key(record) {
            self.inner
                .record_init_waiting
                .lock()
                .unwrap()
                .entry(RecordName::from(record))
                .or_default()
                .push(Box::pin(init));
            return;
        }
        self.dispatch_record_init(Box::pin(init));
    }

    /// Whether a classification made now reads the database
    /// [`Self::dispatch_record_init`] would spawn it against, rather than one
    /// still loading, whose classifications it queues for `iocInit`.
    pub(crate) fn classifies_links_at_once(&self) -> bool {
        matches!(
            *self.inner.init_phase.lock().unwrap(),
            DbInitPhase::Unloaded | DbInitPhase::Running
        )
    }

    /// Queue or spawn an init whose record is known to be registered.
    fn dispatch_record_init(&self, init: RecordInit) {
        if let Some(init) = self.queue_record_init(init) {
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

    /// Queue `init` for `iocInit` while one is owed; hand it back, to run
    /// now, on a complete database.
    fn queue_record_init(&self, init: RecordInit) -> Option<RecordInit> {
        let mut phase = self.inner.init_phase.lock().unwrap();
        match &mut *phase {
            DbInitPhase::Loading(queued) | DbInitPhase::Initialising(queued) => {
                queued.push(init);
                None
            }
            DbInitPhase::Unloaded | DbInitPhase::Running => Some(init),
        }
    }

    /// Take every init parked for `record` — by the one site that registers
    /// the name, immediately after the insert that makes it real.
    fn take_record_inits(&self, record: &[u8]) -> Vec<RecordInit> {
        self.inner
            .record_init_waiting
            .lock()
            .unwrap()
            .remove(record)
            .unwrap_or_default()
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
        // (`iocInit.c:216`), before the record init passes of `iocBuild_2`;
        // every `IocBuilder` build path does so there. The `ca_link_init`
        // below starts the worker for a database that reaches this barrier
        // with no build behind it.
        // C's `initDatabase` per-record init pass — bind each LOAD-deferred
        // record's dset and run its `init_record`. The build lifecycle drains
        // this explicitly BEFORE `setup_io_intr` (C's `scanInit`), so a record's
        // dset is bound before I/O Intr wiring reads it; this call is the
        // catch-all for a path that reaches the barrier directly (a unit test, a
        // bare shell). Idempotent — each record's debt is claimed once. Before
        // `ca_link_init` to keep the order the eager path had, where the init
        // passes ran at load, ahead of the link worker.
        //
        // The drain opens with C's `dbInitRecordLinks`: every link is typed
        // and checked at `iocInit`, not at `dbLoadRecords`, and a link the
        // record's device support cannot take is refused, with the record
        // kept. Its `doResolveLinks` then opens each link, which is where a
        // `{state:"NAME"}` link's `dbStateCreate` happens. Measured on softIoc
        // R7.0.10: `dbStateShowAll 1` prints nothing after `dbLoadRecords`
        // alone and lists every name any link mentions after `iocInit`, all
        // FALSE, with nothing processed.
        self.drain_deferred_record_inits(&|line| crate::stderr_println!("{line}"));
        self.ca_link_init();
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
        // Every link's text is final. After the switch, so a record added
        // concurrently is either swept here or initialises its own forwards.
        self.init_forward_links();
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
        // C `scanInit` (`iocInit.c:186`), whose `buildScanLists` creates the
        // events every `SCAN = "Event"` record names.
        self.build_event_table();
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
    /// A link refused here is emptied before `doResolveLinks` opens it, so a
    /// `{state:…}` link on a field whose device support refuses `JSON_LINK`
    /// never creates its `dbState`.
    ///
    /// C runs this pass (`prepareLinks`, `iocInit.c:175`) after
    /// `initHookAfterInitDevSup` and before `initDatabase`, so its refusals
    /// print ahead of every `init_record` report and device support reads the
    /// emptied link. [`Self::drain_deferred_record_inits`] runs it first for
    /// that reason; it runs once, whichever barrier gets there. A record a
    /// programmatic / `dbCreateRecord` caller initialised at creation has
    /// already classified its links, so its cached link status is re-derived
    /// where the text changes (see [`Self::set_link_text`]) and once per record
    /// at the end of the loop, the seam a runtime re-point uses.
    fn db_init_record_links(&self) {
        if self
            .inner
            .links_prepared
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        use crate::server::record::{DbLinkType, declared_link_type, link_type_refusal};
        let empty_assigned =
            std::mem::take(&mut *self.inner.empty_link_assignments.lock().unwrap());
        for (name, rec) in self.iterate_records() {
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
                    crate::server::record::warn_discarded_cp(&name, &field, ftype, text.as_bytes());
                    if let Some(line) = link_type_refusal(
                        &name.to_str_lossy(),
                        &record_type,
                        Some(&dtyp),
                        &field,
                        &text,
                    ) {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "{}: {line}\n",
                            crate::runtime::log::erl_error()
                        ));
                        refused = true;
                    }
                }
                if !assigned || refused {
                    let empty = declared_link_type(&record_type, Some(&dtyp), &field)
                        .map_or("", DbLinkType::empty_link_text);
                    if empty != text {
                        let after = Self::set_link_text(&rec, &field, empty);
                        if let Some(work) = after {
                            work.run(self);
                        }
                    }
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
    ///
    /// Returns the work the field's `special()` left for after the hold.
    fn set_link_text(
        rec: &RecordCell,
        field: &str,
        text: &str,
    ) -> Option<crate::server::record::AfterGate> {
        let value = EpicsValue::String(text.into());
        // Both parses are made before the write guard and both old ones
        // dropped after it.
        let mut common = crate::server::record::PreparedCommonLink::for_put(field, &value);
        let declared =
            crate::server::record::PreparedLink::for_put(rec.rdes().fields, field, &value);
        let after = Self::set_link_text_held(&mut rec.write(), field, value, &mut common, declared);
        drop(common);
        after
    }

    fn set_link_text_held(
        inst: &mut RecordInstance,
        field: &str,
        value: EpicsValue,
        common: &mut Option<crate::server::record::PreparedCommonLink>,
        declared: Option<crate::server::record::PreparedLink>,
    ) -> Option<crate::server::record::AfterGate> {
        // C brackets every link write with the special pair — `dbPutSpecial`
        // pass 0 then pass 1 around `dbSetLink` (`dbAccess.c:1174,1178`) — and
        // pass 1 is where a record re-derives what it caches from a link's
        // text: calcout `INAV..INUV`, transform's and (a)scalcout's link
        // tables. This pass is the LAST writer of every link field's text, so
        // without it the classification the record made at load stands against
        // text that no longer exists. The common-field writer runs pass 1
        // itself; the declared-field writers do not.
        let put = if inst.common_link_text(field).is_some() {
            // A type that declares the field itself (throttle's OUT) owns the
            // text, and `dbCommon` keeps the loader's mirror of it.
            let owned = match declared {
                Some(link) if inst.record.implements_field(field) => {
                    inst.put_link_field(link, value.clone())
                }
                _ => Ok(()),
            };
            owned.and(
                inst.put_common_field_db_load_prepared(field, value, common)
                    .map(|_| ()),
            )
        } else {
            let put = match declared {
                Some(link) => inst.put_link_field(link, value),
                None => inst.put_declared_field(field, value),
            };
            let _ = inst.special(field, true);
            put
        };
        debug_assert!(put.is_ok(), "{field} is a link field with no writer");
        inst.record.take_after_gate()
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
        record: &Arc<RecordCell>,
        field: &str,
        string_view: bool,
    ) -> Option<crate::server::snapshot::Snapshot> {
        self.channel_snapshot_for_field_guarded(
            record,
            field,
            string_view,
            &mut std::collections::HashSet::new(),
            ReadPolicy::Wait,
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
    ///
    /// `policy` says how each record on the way is taken. Under
    /// [`ReadPolicy::NoWait`] none is waited for, so a resolve from inside a
    /// record's own processing cannot close a cycle with another holder.
    pub(crate) fn channel_snapshot_for_field_guarded(
        &self,
        record: &Arc<RecordCell>,
        field: &str,
        string_view: bool,
        visited: &mut std::collections::HashSet<RecordName>,
        policy: ReadPolicy<'_>,
    ) -> Option<crate::server::snapshot::Snapshot> {
        if let Some(own) = policy.own(record) {
            return crate::server::record::record_instance::own_field_snapshot(
                own,
                record.rdes().meta,
                field,
                string_view,
            );
        }
        // 1. Under a short read lock: is this field link-backed at all, and if
        //    so what does its link field currently say. Nothing is resolved
        //    here — resolving needs the target's lock.
        // A field no link backs is served under that same lock.
        let link = match policy.read(record, |inst| {
            match inst.link_backed_metadata_field_of(field) {
                None => std::ops::ControlFlow::Break(inst.channel_snapshot_for_field(
                    field,
                    string_view,
                    LinkBacking::none(),
                )),
                Some(link_field) => std::ops::ControlFlow::Continue(
                    inst.stored_link_shared(&link_field)
                        .cloned()
                        .map(|link| (link_field, link)),
                ),
            }
        })? {
            std::ops::ControlFlow::Continue(link) => link,
            std::ops::ControlFlow::Break(served) => return served,
        };

        // 2. No record lock held. A link that resolves to nothing — CONSTANT,
        //    an unresolvable target, or one the visited guard refused — leaves
        //    the map empty, which serves each slot's C seed.
        let mut resolved = HashMap::new();
        if let Some((link_field, parsed)) = link {
            if let Some(meta) = self.link_metadata_through(&parsed, visited, policy) {
                resolved.insert(link_field, meta);
            }
        }

        // 3. Re-lock and build with what was just resolved. The borrow ends
        //    with this call, so there is nowhere to keep it.
        policy
            .read(record, |inst| {
                inst.channel_snapshot_for_field(
                    field,
                    string_view,
                    LinkBacking::resolved(&resolved),
                )
            })
            .flatten()
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
        record: &Arc<RecordCell>,
    ) -> HashMap<String, LinkMetadata> {
        self.resolve_link_backed_metadata_with(record, &InputLinkTexts::none())
    }

    /// [`Self::resolve_link_backed_metadata`] for a caller that has already
    /// read some of this record's link fields this cycle.
    ///
    /// A record's link-backed metadata links are, for the calc class, exactly
    /// its `INPA`..`INPL` — the same twelve the process cycle's multi-input
    /// fetch reads a few stages later. Reading a field by name is a linear
    /// search of the record type's declared names, so asking for all twelve
    /// twice was the single most expensive thing in the cycle. The caller that
    /// has them passes them in; anything not covered is still read here.
    pub fn resolve_link_backed_metadata_with(
        &self,
        record: &Arc<RecordCell>,
        prefetched: &InputLinkTexts,
    ) -> HashMap<String, LinkMetadata> {
        // Every entry below needs a SET link at its input slot, so a pass that
        // read the links and found none is already done — without the record
        // lock the walk would otherwise take.
        if prefetched.none_set() {
            return HashMap::new();
        }
        let links = {
            let inst = record.read();
            Self::link_backed_metadata_texts(&inst, prefetched)
        };
        self.walk_link_backed_metadata(links)
    }

    /// The link texts to walk, read under the caller's one acquisition of the
    /// record lock. Split from [`Self::walk_link_backed_metadata`] so each door
    /// owns its own lock scope: the gate the poster applies has to be asked in
    /// the same acquisition, and a shared body would have to be told which door
    /// called it — the flag whose two meanings this split removes.
    fn link_backed_metadata_texts(
        inst: &crate::server::record::RecordInstance,
        prefetched: &InputLinkTexts,
    ) -> Vec<(String, Arc<crate::server::record::ParsedLink>)> {
        inst.link_backed_metadata_links()
            .iter()
            // The field name is cloned only for a link that is actually set:
            // `then_some` took its argument eagerly, so every declared link
            // paid a `String` allocation per cycle — and the list this builds
            // is empty for the unwired record that is the common case.
            .zip(inst.link_backed_metadata_input_slots())
            .filter_map(|(lf, slot)| {
                let link = prefetched.link_at(*slot, inst, lf)?;
                Some((lf.clone(), link))
            })
            .collect()
    }

    /// Walk each link to its target's metadata. Runs with NO record lock held —
    /// it reaches for the TARGET's lock, and C's own lock sets are what make
    /// that legal only from here.
    fn walk_link_backed_metadata(
        &self,
        links: Vec<(String, Arc<crate::server::record::ParsedLink>)>,
    ) -> HashMap<String, LinkMetadata> {
        let mut resolved = HashMap::new();
        for (link_field, parsed) in links {
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
    pub async fn add_record(
        &self,
        name: impl AsRef<[u8]>,
        record: Box<dyn Record>,
    ) -> CaResult<()> {
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
    /// reachable until they have. `RecordInstance::run_init_pass0` is
    /// crate-private for the same reason.
    ///
    /// A field the load refuses costs the field, not the record (C
    /// `dbRecordField`'s `yyerror(NULL)`): its line is printed, the record is
    /// added with the field at its default, and the call returns `Err`.
    pub async fn add_loaded_record(
        &self,
        name: impl AsRef<[u8]>,
        record: Box<dyn Record>,
        load: RecordLoad,
    ) -> CaResult<()> {
        let mut refused = None;
        let mut entry = crate::server::db_loader::EntryMessage::default();
        self.add_loaded_record_reporting(name, record, load, &mut entry, |field, diagnostic| {
            diagnostic.print();
            refused.get_or_insert_with(|| field.to_string());
        })
        .await?;
        match refused {
            Some(field) => Err(CaError::InvalidValue(format!(
                "field {field} refused at load"
            ))),
            None => Ok(()),
        }
    }

    /// [`Self::add_loaded_record`] with each refused field's C line handed to
    /// `refused` with the field's name; `Err` only when the record itself could
    /// not be created.
    pub(crate) async fn add_loaded_record_reporting(
        &self,
        name: impl AsRef<[u8]>,
        record: Box<dyn Record>,
        load: RecordLoad,
        entry: &mut crate::server::db_loader::EntryMessage,
        mut refused: impl FnMut(&str, crate::server::db_loader::DbDiagnostic),
    ) -> CaResult<()> {
        let name = RecordName::from(name.as_ref());
        // A record created after `iocInit` needs a lock set, and its links —
        // in both directions — may merge it into existing ones. `None` while
        // the database is still loading, which is every ordinary
        // `dbLoadRecords`: `build_lock_sets` builds the whole graph at
        // `iocInit` instead. Declared ABOVE the gate: the relink takes lock
        // sets, which sit above L46, so it must run once the gate is down.
        let relink = self.lock_set_membership_change(&name);
        let gate = self.lock_registration("add_loaded_record");
        self.check_name_free(&name)?;
        let mut instance = RecordInstance::new_boxed(name.clone(), record);
        links::check_record_links(&instance)?;
        // Hand the record a cycle-free handle to its own database so it can
        // post out-of-band field updates / wire completion-driven re-entry
        // (asyn TRACE callback, sseq WAITn) without owning the database.
        // C records reach `dbCommon::pdba`/the IOC the same way at
        // `dbDefineRecord` init; the framework supplies the back-reference,
        // the record never constructs it. Defaulted no-op for records that
        // do not need it.
        instance
            .record
            .set_async_context(name.clone(), self.async_handle());

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
        // C's `plink->text` for the empty assignment — see
        // `PvDatabaseInner::empty_link_assignments`. Taken from the load's own
        // field list because that is the only place the distinction between
        // `field(INP,"")` and no `INP` line survives.
        let mut empty_links: Vec<String> = Vec::new();
        for (field, value) in load.common_fields {
            if let crate::types::EpicsValue::String(text) = &value {
                if text.is_empty() && instance.common_link_text(&field.to_uppercase()).is_some() {
                    empty_links.push(field.to_uppercase());
                }
            }
            if let crate::types::EpicsValue::String(text) = &value {
                if let Some(refusal) = crate::server::db_loader::menu_value_refusal(
                    instance.record.record_type(),
                    &name.to_str_lossy(),
                    &field,
                    &text.as_str_lossy(),
                ) {
                    let stored = refusal.stored;
                    refused(
                        &field,
                        crate::server::db_loader::DbDiagnostic {
                            message: refusal.line(entry),
                            notice: refusal.notice,
                            suggestion: refusal.suggestion,
                        },
                    );
                    if !stored {
                        continue;
                    }
                }
            }
            // C prints the `.db` text; a numeric field's was converted on the way
            // in, so this is its `DBR_STRING` form.
            let text = crate::server::record::record_instance::value_as_dbr_string(&value)
                .map(|s| s.as_str_lossy().into_owned())
                .unwrap_or_default();
            if let Err(e) = instance.put_common_field_db_load(&field, value) {
                let line = crate::server::db_loader::field_refusal_line(
                    entry,
                    instance.record.as_ref(),
                    &name.to_str_lossy(),
                    &field,
                    &text,
                    &e,
                );
                refused(&field, crate::server::db_loader::DbDiagnostic::new(line));
            }
            // An unpublished record has no async context yet; its init passes
            // classify what this load set.
            drop(instance.record.take_after_gate());
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
                .insert(name.clone(), empty_links);
        }

        // After the load, so the cell starts on the loaded `PRIO`.
        instance.device_done = crate::server::database::DeviceDone::new(self, &instance);
        instance.set_record_db(RecordDb::of(self));

        // C `doInitRecord0` (`iocInit.c:530-536`) binds the dset and only then
        // calls `init_record(pass 0)`. Both lines are here, in that order,
        // because every `<rec>Record.c init_record` opens by testing the dset
        // — `ao`'s `prec->init = TRUE`, `sub`'s MLST/ALST/LALM seed and `ai`'s
        // are all BELOW that test, and running the passes first is what made
        // them reachable for a record C refuses.
        //
        // During the LOAD phase this whole span is OWED to `iocInit` instead:
        // C links a record into `pdbbase` at `dbLoadRecords` but runs
        // `init_record` at `iocInit`, and binding the dset eagerly here bound
        // it against whatever device-support ports existed when the `.db` was
        // parsed — wrong for a record whose port a later `st.cmd` command
        // configures (ADCore's `NDTimeSeriesConfigure` builds the `*_TS` port
        // AFTER `dbLoadRecords(NDStats.template)`). The record is still
        // published below so a name check, an alias and `dbInitRecordLinks` all
        // see it; only its device-support binding and passes wait for the
        // barrier's drain of the records that owe an init. Outside the LOAD
        // phase the debt is paid below, once the record is published.
        let defer = self.is_load_deferring();
        // The record's load sequence — C `pnewnode->order =
        // pdbentry->pdbbase->no_records++` (`dbStaticLib.c:1704`) — drawn here,
        // where the node is built, so it is born with the record. Drawn before
        // the deferral test so the deferred drain preserves load order.
        let seq = self
            .inner
            .load_order_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let index = self.inner.record_table.reserve();
        let rec_arc = RecordCell::new(instance, seq, true, Some(index));
        self.inner.record_table.publish(index, rec_arc.clone());
        // C `createLockRecord` allocates the `lockRecord` INTO the record:
        // the registry's answer for this name is this record's own cell,
        // adopted before the record is reachable by anyone.
        self.inner.record_locks.adopt(&name, rec_arc.lock_record());
        self.inner
            .records
            .write()
            .insert(name.clone(), rec_arc.clone());
        // The record is reachable from this line on, so anything its
        // `set_async_context` parked above may now run. This is the only
        // release site because this is the only site that registers the name.
        let inits = self.take_record_inits(&name);

        if defer {
            for init in inits {
                self.dispatch_record_init(init);
            }
            // The dset binding, init passes, scan-index insert and the
            // `recGblInitSimm`/`wdogInit` tail are all owed to `ioc_init`. The
            // record carries that debt itself (`RecordCell::init_owed`), so the
            // barrier finds it by walking the database it is already going to
            // walk.
            return Ok(());
        }

        // Outside the LOAD phase — programmatic creation, `dbCreateRecord`
        // after `iocInit` — there is no barrier to defer to, so the record
        // pays its debt now, through the barrier's own code. Its pass-1 link
        // reads take other records' locks, which sit above L46, so the gate
        // and the lock-set relink go first. Until pass 1 ends the record is
        // reachable in its LOAD-phase state (UDF, INVALID, PACT 0), as a C
        // record is between `dbLoadRecords` and `iocInit`; a put landing then
        // is overwritten by the init that follows it, as there. Unlike C, the
        // CA server is already up during that window.
        drop(gate);
        drop(relink);
        self.init_owed_records(vec![(name.clone(), rec_arc.clone())], &|line| {
            crate::stderr_println!("{line}")
        });
        // A record added after `iocInit` initialises its own forward links;
        // one added before is swept by the barrier.
        if self.ioc_is_running() {
            self.open_record_links(&rec_arc);
            self.init_record_forwards(&rec_arc);
        }
        // The `init_record` tail: with no barrier to finish it, the record's
        // link statuses are final when this returns, as they are when
        // `iocInit` returns for a loaded one. acalcout's first process reads
        // INAA..INLL by them (`aCalcoutRecord.c:1078`).
        for init in inits {
            if let Some(init) = self.queue_record_init(init) {
                init.await;
            }
        }
        Ok(())
    }

    /// Is the database in its LOAD phase, where a new record is published but
    /// its device-support binding and `init_record` passes are deferred to
    /// [`Self::ioc_init`]? See [`Self::add_loaded_record`] and
    /// [`Self::init_deferred_record`].
    fn is_load_deferring(&self) -> bool {
        matches!(
            *self.inner.init_phase.lock().unwrap(),
            DbInitPhase::Loading(_)
        )
    }

    /// Run every record's OWED init — C's `initDatabase` — for the records the
    /// LOAD phase deferred.
    ///
    /// C's `initDatabase` is three whole-database walks, `doInitRecord0`, then
    /// `doResolveLinks`, then `doInitRecord1` (`iocInit.c:576-578`), each in
    /// `iterateRecords` order, so every pass-0 report precedes every pass-1
    /// report and a record's pass 1 runs after every record's pass 0.
    /// [`RecordCell::claim_owed_init`] hands each debt to exactly one caller,
    /// so a second call is a no-op — the build lifecycle calls this BEFORE
    /// `setup_io_intr`, and [`Self::ioc_init`] calls it again as a catch-all.
    ///
    /// `err` is the building thread's stderr, where aSubRecord.c's epicsStdio
    /// lines go.
    pub(crate) fn drain_deferred_record_inits(&self, err: &dyn Fn(&str)) {
        self.db_init_record_links();
        let records = self.iterate_records();
        self.init_owed_records(records.clone(), err);
        for (_, rec_arc) in &records {
            self.open_record_links(rec_arc);
        }
    }

    /// [`Self::drain_deferred_record_inits`] over `records`: the debt of each
    /// one this call claims is paid in C's three walks.
    fn init_owed_records(&self, records: Vec<(RecordName, Arc<RecordCell>)>, err: &dyn Fn(&str)) {
        let resolver = self.inner.device_support_resolver.load();
        let owed: Vec<_> = records
            .into_iter()
            .filter(|(_, rec_arc)| rec_arc.claim_owed_init())
            .map(|(name, rec_arc)| {
                let mut guard = rec_arc.write();
                let instance = &mut *guard;
                crate::server::ioc_app::attach_device_support(
                    instance,
                    &name.to_str_lossy(),
                    resolver.as_deref(),
                );
                instance.arm_init_subroutines(self.inner.subroutine_registry.load_full());
                let (reached, owed) = instance.run_init_pass0(&name.to_str_lossy());
                let guard = self.pay_device_init(&rec_arc, guard, owed, &name.to_str_lossy(), err);
                // The load has moved the type selectors since the cell was
                // built, and an `add_record` below may ask this record's `$`.
                rec_arc.settle_addr_facts(&guard);
                drop(guard);
                (name, rec_arc, reached)
            })
            .collect();
        // A record created before any load opens its links at the barrier
        // ([`Self::drain_deferred_record_inits`]), once its lsets exist.
        let open_links = !matches!(
            *self.inner.init_phase.lock().unwrap(),
            DbInitPhase::Unloaded
        );
        for (name, rec_arc, reached) in &owed {
            if *reached {
                let add_record_input =
                    crate::server::builtin_devices::soft_callback::add_record_input_of(
                        self, rec_arc, None,
                    );
                rec_arc
                    .write()
                    .run_add_record(&name.to_str_lossy(), add_record_input);
            }
            if open_links {
                self.open_record_links(rec_arc);
            }
        }
        for (name, rec_arc, reached) in &owed {
            self.init_deferred_record(name, rec_arc, *reached, err);
        }
    }

    /// Is `name` still awaiting its deferred init — created during the LOAD
    /// phase and not yet drained by [`Self::drain_deferred_record_inits`]? The
    /// merge arm of the iocsh loader uses this to tell a record it must init in
    /// place (created before the load, already live) from one the barrier will
    /// init against the final merged fields.
    pub(crate) fn record_init_deferred(&self, name: &[u8]) -> bool {
        self.get_record(name).is_some_and(|rec| rec.init_owed())
    }

    /// Run the rest of the OWED init half of [`Self::add_loaded_record`] for a
    /// record whose creation was deferred to `iocInit` (the LOAD-phase arm),
    /// once [`Self::drain_deferred_record_inits`] has run pass 0 and
    /// `add_record` over every owed record: pass 1 when pass 0 `reached` the
    /// record's body, the constant-link seed, then the scan-index insert, the `recGblInitSimm`/`wdogInit` tail, and
    /// the checkLinks / constant-INP seed the two loaders used to run right
    /// after `add_loaded_record`. All in the same order the eager path ran them
    /// across `add_loaded_record` and its caller.
    ///
    /// No registration gate is taken: the barrier drains this list from a
    /// single synchronous loop with no `.await`, so no other creation
    /// interleaves, and the gate's only job is to serialize concurrent
    /// creation. Holding the record's write lock across the passes is safe —
    /// no `init_record` path re-enters the records map for its own record
    /// (the map-reading async-handle calls live only in processing/reentry).
    /// `recGblInitSimm` reaches `update_scan_index`, which takes the gate
    /// itself, so it runs after the write lock is dropped, exactly as the eager
    /// arm runs it after its `drop(gate)`.
    fn init_deferred_record(
        &self,
        name: &RecordName,
        rec_arc: &Arc<RecordCell>,
        reached: bool,
        err: &dyn Fn(&str),
    ) {
        let mut init_reads = reached.then(|| self.read_init_links(rec_arc));
        let (scan, phas, record_type) = {
            let mut guard = rec_arc.write();
            if let Some(reads) = &mut init_reads {
                let owed = guard.run_init_pass1(&name.to_str_lossy(), reads, err);
                guard = self.pay_device_init(rec_arc, guard, owed, &name.to_str_lossy(), err);
            }
            let instance = &mut *guard;
            super::database::processing::seed_constant_links(instance);
            rec_arc.settle_addr_facts(instance);
            (
                instance.common.scan,
                instance.common.phas,
                instance.record.record_type(),
            )
        };
        self.add_to_scan_list(scan, phas, record_type, rec_arc.load_order(), name);
        self.rec_gbl_init_simm(rec_arc);
        self.arm_watchdog(name);
        // The tail both loaders ran right after `add_loaded_record`: C's
        // `init_record` checkLinks (`init_links`), then the constant-INP seed /
        // `dbLoadLinkArray` (`rec_gbl_init_constant_links`). For a deferred
        // record they belong here, after the passes, in the eager path's order —
        // the loaders no longer run them for a record the barrier owns.
        {
            let mut guard = rec_arc.write();
            let inst = &mut *guard;
            inst.record.init_links(&inst.common);
        }
        self.rec_gbl_init_constant_links(rec_arc);
    }

    /// Run the dset init an init pass left owed with `guard`'s borrow let go
    /// and the record lent through an [`InitHold`], as a cycle lends it
    /// around `read_begin`, then the rest of that pass; hand the borrow back.
    /// With nothing owed, `guard` is returned as it is.
    pub(crate) fn pay_device_init<'r>(
        &self,
        rec: &'r Arc<RecordCell>,
        guard: crate::server::record::RecordMut<'r>,
        owed: Option<crate::server::device_support::OwedDeviceInit>,
        name: &str,
        err: &dyn Fn(&str),
    ) -> crate::server::record::RecordMut<'r> {
        let Some(mut owed) = owed else {
            return guard;
        };
        drop(guard);
        {
            let _init = InitPass::enter();
            let outcome = owed.init_held(&mut InitHold::new(self, rec));
            let gate = FrameGate::of_instance(self, rec);
            let frame = gate.frame(rec);
            let mut instance = frame.write();
            owed.finish(&mut instance, outcome, name, err);
            field_io::emit_cycle_posts(&mut instance);
        }
        rec.write()
    }

    /// The answers to `rec_arc`'s [`Record::init_link_reads`], read with no
    /// lock of its own held, so a link to the record itself reads it too.
    ///
    /// [`Record::init_link_reads`]: crate::server::record::Record::init_link_reads
    pub(crate) fn read_init_links(
        &self,
        rec_arc: &Arc<RecordCell>,
    ) -> crate::server::record::InitLinkReads {
        let links: Vec<_> = {
            let instance = rec_arc.read();
            instance
                .record
                .init_link_reads()
                .iter()
                .map(|&(field, read_as)| {
                    // The link the record holds; a field that is not one of
                    // its links is read from its text.
                    let held = instance.stored_link_shared(field).cloned();
                    let text = held.is_none().then(|| instance.link_text(field)).flatten();
                    (field, text, read_as, held)
                })
                .collect()
        };
        crate::server::record::InitLinkReads::new(
            links
                .into_iter()
                .map(|(field, text, read_as, held)| {
                    let read = match held {
                        Some(link) => self.read_link_at_init(rec_arc, &link, read_as),
                        None => self.read_link_at_init(
                            rec_arc,
                            &crate::server::record::parse_link_v2(
                                text.as_ref().map_or(&b""[..], |t| t.as_bytes()),
                            ),
                            read_as,
                        ),
                    };
                    (field, read)
                })
                .collect(),
        )
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
    fn check_name_free(&self, name: &[u8]) -> CaResult<()> {
        let kind = if self.simple_pv_named(name) {
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
                message: format!(
                    "name '{}' is already registered as a {kind}",
                    String::from_utf8_lossy(name)
                ),
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
    pub async fn remove_record(&self, name: impl AsRef<[u8]>) -> bool {
        let name = name.as_ref();
        // C `dbDeleteRecord` frees the record's `lockRecord`, so the set it
        // was in loses a member and may fall apart into several. Declared
        // first so it drops last: the relink runs once the record is out of
        // the map AND once both gates below are down, since it takes lock
        // sets and those sit above L46.
        let _relink = self.lock_set_membership_change(name);
        let Some(removed) = self.remove_record_entry(name) else {
            return false;
        };
        // With both gates down: every other record's handle to the removed
        // one is dropped, one record's lock set at a time. A handle made
        // after this sweep would need the map to still answer the name, and
        // it stopped answering before the sweep began.
        let others: Vec<Arc<RecordCell>> = self.inner.records.read().values().cloned().collect();
        for rec in &others {
            rec.write().release_link_targets_to(&removed);
        }
        true
    }

    /// [`Self::remove_record`] under its gates: the entry out of every map
    /// and index, the record destroyed. The removed cell, for the sweep the
    /// caller runs once the gates are down.
    fn remove_record_entry(&self, name: &[u8]) -> Option<Arc<RecordCell>> {
        // The record's data — its SCAN for the index sweep, `destroy()` at
        // the end — is behind its lock set, which is taken ABOVE L46 in the
        // acquisition order. So the set is taken first, off the handle, and
        // the gate under it; the map is then re-read under the gate to prove
        // the handle locked is the handle registered, as `update_scan_index`
        // does. Reverse declaration order releases the gate first.
        let (rec_arc, _record_gate, _gate) = loop {
            let rec_arc = self.get_record_no_resolve(name)?;
            let record_gate = self.lock_instance(&rec_arc);
            let gate = self.lock_registration("remove_record");
            let registered = self
                .inner
                .records
                .read()
                .get(name)
                .is_some_and(|live| Arc::ptr_eq(live, &rec_arc));
            if registered {
                break (rec_arc, record_gate, gate);
            }
        };
        // 1) Remove from main map.
        self.inner.records.write().remove(name);

        // 2) Drop the CP/CPP edges the record held.
        self.inner
            .external_cp_links
            .update(|cp| cp.remove_record(name));

        // 3) Purge aliases that pointed AT the
        // removed record. Otherwise `find_pv("ALT")` returns None
        // (target gone) but `add_pv("ALT", ...)` still fails with
        // "already registered as an alias" — orphan blocks reuse.
        // The alias nodes go with the record, and their load sequences with
        // them: the sequence is a field of the node, so a sequence outliving
        // the node it describes is unrepresentable.
        self.inner
            .aliases
            .write()
            .retain(|_alias, node| *node.target != *name);

        self.destroy_record(&rec_arc, name);
        Some(rec_arc)
    }

    /// The finalizer of a removed record, and the only one: the record
    /// table keeps the cell for the IOC's life (C never frees a record), so
    /// what a dropped cell used to let go of — its scan-list key, monitors,
    /// lent link addresses, queued put-notifies — is released here.
    ///
    /// Same rule as `remove_simple_pv`: removal IS destruction. The cell
    /// outlives the map entry, so without the mark a downstream monitor or a
    /// bound forward would keep serving a record the database no longer has.
    fn destroy_record(&self, rec: &RecordCell, name: &[u8]) {
        let scan = rec.read().common.scan;
        self.delete_from_scan_list(scan, name);
        rec.write().destroy();
        self.signal_destroyed();
    }

    /// Internal: synchronous lookup without invoking the search resolver.
    async fn find_entry_no_resolve(&self, name: &[u8]) -> Option<PvEntry> {
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
            .get(record_path.as_slice())
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
        if let Some(target) = self.alias_target(base) {
            if let Some(rec) = self.inner.records.read().get(&*target) {
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
    pub async fn add_alias(
        &self,
        alias: impl AsRef<[u8]>,
        target: impl AsRef<[u8]>,
    ) -> CaResult<()> {
        let (alias, target) = (alias.as_ref(), target.as_ref());
        // A link naming `alias` resolved to nothing until now, so the alias
        // can turn a dangling link into a real edge and merge two sets.
        // Declared above the gate so the relink, which takes lock sets, runs
        // after L46 is released.
        let _relink = self.lock_set_membership_change(target);
        let _gate = self.lock_registration("add_alias");
        if !self.inner.records.read().contains_key(target) {
            return Err(CaError::ChannelNotFound(format!(
                "alias target '{}' is not a registered record",
                String::from_utf8_lossy(target)
            )));
        }
        self.check_name_free(alias)?;
        // An alias is a node of the database, so it takes a sequence from the
        // same counter the records draw from — C numbers it identically,
        // `pnewnode->order = pdbentry->pdbbase->no_records++`
        // (`dbStaticLib.c:1704`), which is what puts an alias at its own load
        // position in the list `dbl` and `dbglob` walk.
        let order = self
            .inner
            .load_order_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.aliases.write().insert(
            RecordName::from(alias),
            AliasNode {
                target: RecordName::from(target),
                order,
            },
        );
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

    /// Declare and bind the JSON link type `key`: the binary's own `link()`
    /// line and its `registryJLinkAdd`, ahead of any `.db` this database
    /// loads. It appends: C's expanded `.dbd` sorts by jlif
    /// (`DBD/Output.pm:92-95`), which puts pvxs's `lsetPVX` after every base
    /// `lnk…If` (pvxs 1.5.3 `softIocPVX.dbd:18997-19002`). Reached only
    /// through `register_link_type`, which carries the type's link set too.
    ///
    /// # Panics
    ///
    /// When this port implements no JSON link type `key`: a declaration names a
    /// jlif the binary carries, and there is none to carry.
    pub(crate) fn declare_link_type(&self, key: &str) {
        let jlif = crate::server::record::implemented_jlif(key)
            .unwrap_or_else(|| panic!("this port implements no JSON link type '{key}'"));
        self.inner.link_list.update(|list| {
            list.declare(key, jlif);
            list.bind(key);
        });
    }

    /// The `link()` lines one `dbLoadDatabase` read, through C's
    /// `dbLinkType`, which declares and binds nothing: support is bound only
    /// by registration (`registryJLinks.c:16-23`), so a loaded
    /// `link("pva", "lsetPVX")` leaves `{pva:…}` refused as not loaded.
    pub(crate) fn add_loaded_link_types(&self, types: &[crate::server::db_loader::DbdLinkType]) {
        self.inner.link_list.update(|list| {
            for t in types.iter().filter(|t| !t.key.is_empty()) {
                list.declare(&t.key, &t.lset);
            }
        });
    }

    /// This database's `linkList`, as a lock-free snapshot.
    pub(crate) fn link_list(&self) -> arc_swap::Guard<Arc<crate::server::record::LinkList>> {
        self.inner.link_list.load()
    }

    /// Resolve an alias to its target record name, or `None` when the
    /// name is not an alias.
    pub fn resolve_alias(&self, name: impl AsRef<[u8]>) -> Option<RecordName> {
        self.alias_target(name.as_ref())
    }

    /// The record name the alias node `name` points at, or `None` when the
    /// name is not an alias. The one reader of an alias node's target.
    fn alias_target(&self, name: &[u8]) -> Option<RecordName> {
        self.inner
            .aliases
            .read()
            .get(name)
            .map(|node| node.target.clone())
    }

    /// Queue an `afterIocRunning` line for the `iocRun` that ends `iocInit`.
    pub fn queue_after_ioc_running(
        &self,
        line: impl Into<Vec<u8>>,
    ) -> Result<(), AfterIocRunningRefusal> {
        match &mut *self.inner.after_ioc_running.lock().unwrap() {
            AfterIocRunning::Ran => Err(AfterIocRunningRefusal::AfterIocInit),
            AfterIocRunning::Queuing(queue) => {
                let line = line.into();
                if line.is_empty() {
                    return Err(AfterIocRunningRefusal::Empty);
                }
                queue.push_back(line);
                Ok(())
            }
        }
    }

    /// C `afterIocRunningHook` (`afterIocRunning.c:46-55`): each queued line
    /// to `run` in order — a line one of them queues as well — then the queue
    /// closed, as `initEndFlag = 1` closes it.
    pub fn drain_after_ioc_running(&self, mut run: impl FnMut(Vec<u8>)) {
        loop {
            let next = match &mut *self.inner.after_ioc_running.lock().unwrap() {
                AfterIocRunning::Queuing(queue) => queue.pop_front(),
                AfterIocRunning::Ran => None,
            };
            let Some(line) = next else { break };
            run(line);
        }
        *self.inner.after_ioc_running.lock().unwrap() = AfterIocRunning::Ran;
    }

    /// C `dbDbInitLink`'s `dbChannelCreate` succeeding (`dbDbLink.c:94`): a
    /// local record that names `field`. Otherwise `dbInitLink` falls through
    /// to CA (`dbLink.c:128`), `SRC.desc` and `SRC.NOSUCH` alike. Registry
    /// facts only, as the holder may be held while this is asked.
    pub(crate) fn has_db_link_target(&self, record: &[u8], field: &str) -> bool {
        if self.simple_pv_named(record) {
            return true;
        }
        self.record_or_alias(record)
            .is_some_and(|rec| self.names_field(&rec, field))
    }

    /// `dbChannelCreate`'s field half for `rec`: `pvNameLookup` finds `field`
    /// (`dbChannel.c:313-330`) and, with the `$` modifier, the field takes it
    /// (`dbChannel.c:486-503`). A declared `DBF_NOACCESS` name is found, as
    /// `dbNameToAddr` finds `MLOK`. Registry facts only: the search side and
    /// a caller under a put gate ask it, so it takes no record lock.
    pub(crate) fn channel_field_found(
        &self,
        rec: &RecordCell,
        field: &str,
        string_view: bool,
    ) -> bool {
        if !string_view {
            return self.names_field(rec, field);
        }
        let addr = crate::server::record::record_instance::FieldAddr::resolve_in(rec, field);
        filters::spells_a_field(field)
            && match addr.desc {
                Some(d) => rec.takes_string_view(d),
                // `dbGetAttributePart`'s descriptor is a `DBF_STRING`.
                None => self.attribute_field_type(rec, field).is_some(),
            }
    }

    /// `dbNameToAddr`'s field half for `rec`: a declared name, else an
    /// attribute (`dbGetAttributePart`), both compared as written.
    pub(crate) fn names_field(&self, rec: &RecordCell, field: &str) -> bool {
        rec.declares_name(field) || self.attribute_field_type(rec, field).is_some()
    }

    /// `dbNameToAddr`'s `field_type` where `dbFindFieldPart` misses and
    /// `dbGetAttributePart` finds a record attribute (`dbAccess.c:673-675`):
    /// `dbEntryToAddr` copies `dbPutRecordAttribute`'s `DBF_STRING`
    /// descriptor, one element (`dbAccess.c:636-639`, `dbStaticLib.c:1268-1271`).
    pub(crate) fn attribute_field_type(
        &self,
        rec: &RecordCell,
        field: &str,
    ) -> Option<crate::types::DbfCode> {
        (!rec.declares_name(field) && self.has_record_type_attribute(rec.rdes().record_type, field))
            .then_some(crate::types::DbfCode::String)
    }

    /// `getAttrValue`'s text of the attribute [`Self::attribute_field_type`]
    /// finds (`dbAccess.c:950-953`).
    pub(crate) fn attribute(&self, rec: &RecordCell, field: &str) -> Option<String> {
        self.attribute_field_type(rec, field)?;
        self.record_type_attribute(rec.rdes().record_type, field)
    }

    /// [`RecordCell::addressed_field_type`], a record attribute included.
    pub(crate) fn addressed_field_type(
        &self,
        rec: &RecordCell,
        field: &str,
        own: Option<&dyn Record>,
    ) -> Option<crate::types::DbfCode> {
        rec.addressed_field_type(field, own)
            .or_else(|| self.attribute_field_type(rec, field))
    }

    /// [`RecordCell::no_elements`], a record attribute's one element included.
    pub(crate) fn no_elements(&self, rec: &RecordCell, field: &str) -> Option<u32> {
        rec.no_elements(field)
            .or_else(|| self.attribute_field_type(rec, field).map(|_| 1))
    }

    /// [`RecordCell::resolve_addr`], a record attribute's one element
    /// included: its `special` is `SPC_ATTRIBUTE`, so no `cvt_dbaddr` runs.
    pub(crate) fn resolve_addr(&self, rec: &Arc<RecordCell>, field: &str) -> Option<u32> {
        rec.resolve_addr(field)
            .or_else(|| self.attribute_field_type(rec, field).map(|_| 1))
    }

    /// `dbNameToAddr`'s `$` branch (`dbAccess.c:681-701`) on the address
    /// [`Self::addressed_field_type`] gives `field`: its `field_type` and
    /// `no_elements` then. A `DBF_STRING`, an attribute among them, becomes
    /// `field_size` `DBF_CHAR`s (`:686-690`); a link keeps its type with
    /// `PVLINK_STRINGSZ` (`dbDefs.h:79`) elements (`:692-696`); any other
    /// field is `S_dbLib_fieldNotFound` (`:698-700`), `None`.
    pub(crate) fn char_addressed(
        &self,
        rec: &RecordCell,
        field: &str,
        own: Option<&dyn Record>,
    ) -> Option<(crate::types::DbfCode, u32)> {
        let addr = char_view::CharAddr::of(self, rec, field)?;
        let field_type = match addr.source {
            char_view::CharSource::Link => self.addressed_field_type(rec, field, own)?,
            _ => crate::types::DbfCode::Char,
        };
        Some((field_type, addr.no_elements as u32))
    }

    /// Synchronous existence check without resolver, taking no record lock:
    /// a simple PV, or a channel [`Self::channel_test`] finds.
    pub fn has_name_no_resolve(&self, name: &[u8]) -> bool {
        // A filtered simple PV (`SP.{"arr":...}`) answers by its base name.
        self.simple_pv_named(&filters::split_channel_name(name).record_path)
            || self.channel_test(name)
    }

    /// C `dbChannelTest` (`dbChannel.c:331-342`), records and aliases only,
    /// taking no record lock so an lset's [`LinkSet::open_link`] may ask it
    /// under a put's hold: the record name up to the first `.`, then the
    /// field name's identifier prefix (`dbStaticLib.c:1800-1802`); what
    /// follows is not read, so `R.VAL$garbage` is found and refused at
    /// create.
    pub fn channel_test(&self, name: &[u8]) -> bool {
        let (base, rest) = match name.iter().position(|&b| b == b'.') {
            Some(dot) => (&name[..dot], &name[dot + 1..]),
            None => (name, &b""[..]),
        };
        let Some(rec) = self.record_or_alias(base) else {
            return false;
        };
        let len = match rest.first() {
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => rest
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                .unwrap_or(rest.len()),
            // An absent field name binds `VAL`.
            _ => return true,
        };
        // The identifier prefix is ASCII by construction.
        self.names_field(&rec, std::str::from_utf8(&rest[..len]).unwrap_or_default())
    }

    fn record_or_alias(&self, name: &[u8]) -> Option<Arc<RecordCell>> {
        self.inner.records.read().get(name).cloned().or_else(|| {
            let target = self.alias_target(name);
            target.and_then(|t| self.inner.records.read().get(&*t).cloned())
        })
    }

    fn simple_pv_named(&self, name: &[u8]) -> bool {
        self.inner.simple_pvs.lock().contains_key(name)
    }

    /// The simple PV registered as `name`.
    pub(crate) fn simple_pv_bytes(&self, name: &[u8]) -> Option<Arc<ProcessVariable>> {
        self.inner.simple_pvs.lock().get(name).cloned()
    }

    /// `dbChannelCreate`'s address half: `pvNameLookup` finds the field as
    /// written and `dbEntryToAddr` runs its `cvt_dbaddr` (`dbChannel.c:460-478`)
    /// before the `$` modifier is judged against it (`:486-503`), so a
    /// create the `$` or the filter then refuses has charged it too. The
    /// search side, `dbChannelTest`, runs neither.
    pub(crate) fn resolve_channel_create(&self, name: &[u8]) -> bool {
        // A filtered channel (`REC.{"dbnd":{"d":0.5}}`) resolves to its
        // underlying PV, as in `find_entry_no_resolve`.
        let record_path = filters::split_channel_name(name).record_path;
        if self.simple_pv_named(&record_path) {
            return true;
        }
        let (base, explicit_field) = match record_path.iter().rposition(|&b| b == b'.') {
            Some(dot) => (&record_path[..dot], Some(parse_pv_name(&record_path).1)),
            None => (record_path.as_slice(), None),
        };
        let Some(rec) = self.record_or_alias(base) else {
            return false;
        };
        let Some(field) = explicit_field else {
            self.resolve_addr(&rec, "VAL");
            return true;
        };
        let core = field.strip_suffix('$');
        let found = self.names_field(&rec, core.unwrap_or(field));
        if found {
            self.resolve_addr(&rec, core.unwrap_or(field));
        }
        match core {
            Some(core) => self.channel_field_found(&rec, core, true),
            None => found,
        }
    }

    /// Look up an entry by name. Supports "record.FIELD" syntax.
    ///
    /// If the name is not found and a search resolver is installed,
    /// the resolver is invoked once. If the resolver returns true, the
    /// database is re-checked.
    pub async fn find_entry(&self, name: impl AsRef<[u8]>) -> Option<PvEntry> {
        self.find_entry_from(name, None).await
    }

    /// Like [`Self::find_entry`], but threads the downstream client's
    /// socket address into the search resolver. The CA TCP CREATE_CHANNEL
    /// handler passes the connection peer so the gateway can apply
    /// host-scoped `.pvlist` admission.
    pub async fn find_entry_from(
        &self,
        name: impl AsRef<[u8]>,
        peer: Option<std::net::SocketAddr>,
    ) -> Option<PvEntry> {
        let name = name.as_ref();
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
        if self.search_resolver_admits(name, peer).await {
            return self.find_entry_no_resolve(name).await;
        }
        None
    }

    /// Check if a base name exists (for UDP search).
    ///
    /// If the name is not in the database and a search resolver is installed,
    /// the resolver is invoked. The resolver may populate the database
    /// (e.g., subscribe to an upstream IOC and add a placeholder PV) and
    /// return true; this method then re-checks.
    pub async fn has_name(&self, name: impl AsRef<[u8]>) -> bool {
        self.has_name_from(name, None).await
    }

    /// Like [`Self::has_name`], but threads the downstream client's
    /// socket address into the search resolver. The CA UDP search
    /// responder passes the datagram source address so the gateway can
    /// apply host-scoped `.pvlist` admission.
    pub async fn has_name_from(
        &self,
        name: impl AsRef<[u8]>,
        peer: Option<std::net::SocketAddr>,
    ) -> bool {
        let name = name.as_ref();
        if self.has_name_no_resolve(name) {
            // Same per-request gate as `find_entry_from`: a cached simple
            // PV the gateway's host/state admission denies must answer
            // does-not-exist at search time. Records/aliases bypass.
            if self.simple_pv_gate_denies(name, peer).await {
                return false;
            }
            return true;
        }
        if self.search_resolver_admits(name, peer).await {
            return self.has_name_no_resolve(name);
        }
        false
    }

    /// Ask the installed [`SearchResolver`] to make `name` resolvable.
    async fn search_resolver_admits(
        &self,
        name: &[u8],
        peer: Option<std::net::SocketAddr>,
    ) -> bool {
        let Some(resolver) = self.inner.search_resolver.load_full().map(|r| (*r).clone()) else {
            return false;
        };
        resolver(name.into(), peer).await
    }

    /// Look up a simple PV by name (backward-compatible).
    pub fn find_pv(&self, name: impl AsRef<[u8]>) -> Option<Arc<ProcessVariable>> {
        self.inner.simple_pvs.lock().get(name.as_ref()).cloned()
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
    pub fn get_record(&self, name: impl AsRef<[u8]>) -> Option<Arc<RecordCell>> {
        let name = name.as_ref();
        if let Some(rec) = self.inner.records.read().get(name).cloned() {
            return Some(rec);
        }
        let target = self.alias_target(name)?;
        self.inner.records.read().get(&*target).cloned()
    }

    /// The record behind `name` AND the canonical name it is registered
    /// under — the one place a caller's name becomes the shared [`RecordName`]
    /// that a process chain then carries from hop to hop.
    ///
    /// The records map is asked first and the alias table only on a miss. A
    /// canonical name is the overwhelming common case, and the two namespaces
    /// are disjoint — `add_loaded_record` refuses a name an alias already
    /// holds — so asking the alias table first would hash, on every scan step
    /// of every record, a name that is never in it.
    ///
    /// The `Arc` handed back is the map's own KEY, so nothing downstream
    /// allocates the name again: the cycle guard, the scan key and the
    /// lock-set lookup all share this one allocation, made when the record was
    /// registered. Alias-aware for the same reason
    /// [`Self::get_record`] is (epics-base PR #336) — and the name it returns
    /// for an alias is the TARGET's, which is what `visited` must key on if
    /// alias and canonical are not to count as two different records.
    pub(crate) fn lookup_record(
        &self,
        name: impl AsRef<[u8]>,
    ) -> Option<(RecordName, Arc<RecordCell>)> {
        let name = name.as_ref();
        {
            let records = self.inner.records.read();
            if let Some((canonical, rec)) = records.get_key_value(name) {
                return Some((canonical.clone(), rec.clone()));
            }
        }
        let target = self.alias_target(name)?;
        let records = self.inner.records.read();
        let (canonical, rec) = records.get_key_value(&*target)?;
        Some((canonical.clone(), rec.clone()))
    }

    /// The record registered at `index` — a removed one too, which its own
    /// `is_destroyed` refuses.
    #[inline(always)]
    pub(crate) fn record_at(&self, index: RecordIdx) -> Option<&Arc<RecordCell>> {
        self.inner.record_table.get(index)
    }

    /// Strict variant of [`Self::get_record`] — does NOT consult the
    /// alias table. Returns `Some` only when a canonical record with
    /// that exact name exists.
    pub fn get_record_no_resolve(&self, name: impl AsRef<[u8]>) -> Option<Arc<RecordCell>> {
        self.inner.records.read().get(name.as_ref()).cloned()
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
    /// The order key is the record's own `load_order` sequence (the
    /// scan-index's secondary sort key), so this ordering and the scan lists'
    /// ordering are the same fact, not two. A record cannot be missing one: it
    /// is a field of its cell, set where the cell is built.
    pub fn all_record_names(&self) -> Vec<RecordName> {
        // One map, one guard: the sequence travels with the record, so the
        // snapshot and the sort key come from the same read and there is no
        // second structure to order a lock against.
        let mut names: Vec<(u64, RecordName)> = {
            let records = self.inner.records.read();
            records
                .iter()
                .map(|(name, rec)| (rec.load_order(), name.clone()))
                .collect()
        };
        names.sort_unstable();
        names.into_iter().map(|(_order, name)| name).collect()
    }

    /// C's `iterateRecords` (`iocInit.c:484-505`): type-major over
    /// `recordTypeList`, which is name-sorted (see `ScanKey::record_type`),
    /// each type's records in load order (`dbStaticLib.c:1446`). An `iocInit`
    /// pass that prints walks this, so its lines come out in C's order.
    pub(crate) async fn iterate_record_names(&self) -> Vec<RecordName> {
        let mut names: Vec<(&'static str, RecordName)> = self
            .all_record_names()
            .into_iter()
            .filter_map(|name| {
                let record_type = self.get_record(&name)?.read().record.record_type();
                Some((record_type, name))
            })
            .collect();
        // Stable, so load order holds within a type.
        names.sort_by_key(|(record_type, _)| *record_type);
        names.into_iter().map(|(_, name)| name).collect()
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
        // Each map is read under its own guard, which dies with the statement,
        // so no two are ever held at once. Each node carries its own sequence,
        // so the two reads need not be one snapshot to be ordered together.
        let mut nodes: Vec<(u64, DbNode)> = {
            let records = self.inner.records.read();
            records
                .iter()
                .map(|(name, rec)| {
                    (
                        rec.load_order(),
                        DbNode {
                            name: name.clone(),
                            alias_of: None,
                        },
                    )
                })
                .collect()
        };
        nodes.extend({
            let aliases = self.inner.aliases.read();
            aliases
                .iter()
                .map(|(alias, node)| {
                    (
                        node.order,
                        DbNode {
                            name: alias.clone(),
                            alias_of: Some(node.target.clone()),
                        },
                    )
                })
                .collect::<Vec<_>>()
        });
        nodes.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        nodes.into_iter().map(|(_order, node)| node).collect()
    }

    /// Get all alias names registered against existing records.
    /// Mirrors the alias-half of base's `dbFirstRecord` iteration —
    /// `dbgrep` / `dbglob` / `dbsr` walk both record names and
    /// aliases when matching a glob.
    pub fn all_alias_names(&self) -> Vec<RecordName> {
        self.inner.aliases.read().keys().cloned().collect()
    }

    /// Return every alias that points at `canonical`. Sorted for
    /// stable output; empty when the record has no aliases. Used by
    /// `dbpr` to surface alias-form names so admins can see how
    /// clients may reach the record.
    pub fn aliases_for_record(&self, canonical: impl AsRef<[u8]>) -> Vec<RecordName> {
        let canonical = canonical.as_ref();
        let aliases = self.inner.aliases.read();
        let mut hits: Vec<RecordName> = aliases
            .iter()
            .filter_map(|(alias, node)| {
                if *node.target == *canonical {
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
    pub fn all_simple_pv_names(&self) -> Vec<RecordName> {
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

    /// C lists one lock of what `PvDatabase` holds directly, `dbPvd`'s
    /// (`dbPvdLib.c:119`): the registration gate and the attribute writer
    /// have no C mutex, so every entry this file creates is `simple_pvs`'.
    #[test]
    fn the_database_lists_only_its_dbpvd_lock() {
        let db = PvDatabase::new();
        let shown = crate::runtime::sync::mutex_report(false).shown;
        let pvd = db.inner.simple_pvs.registered_addr();
        let pvd_line = shown
            .iter()
            .find(|e| e.addr() == pvd)
            .expect("simple_pvs is listed")
            .line();
        let extra: Vec<_> = shown
            .iter()
            .filter(|e| e.file() == file!() && e.line() != pvd_line)
            .collect();
        assert!(extra.is_empty(), "{extra:?}");
    }

    /// C `recGblGetTimeStampSimm` (recGbl.c:310-343) maps TSE values
    /// to epicsTime sources via the constants in `epicsTime.h:102-104`.
    /// The Rust port previously misread TSE=-1 as "device-provided
    /// with BestTime fallback" and gated the BestTime call on a
    /// UNIX_EPOCH check. C calls `epicsTimeGetEvent(-1)`
    /// unconditionally; only TSE=-2 (epicsTimeEventDeviceTime) leaves
    /// `precord->time` untouched.
    ///
    /// Regression: a stale device write (any non-epoch crate::types::WallTime)
    /// suppressed every BestTime refresh thereafter.
    #[test]
    fn apply_timestamp_tse_minus_one_always_overwrites_with_best_time() {
        use crate::server::record::CommonFields;
        use std::time::Duration;

        // Pre-populate `time` with a stale but non-epoch sentinel.
        let stale = crate::types::WallTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut common = CommonFields::default();
        common.set_tse(-1);
        common.time = stale;

        TselStamp::None.stamp(b"REC", &mut common, || false);

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
        use std::time::Duration;

        let device_time = crate::types::WallTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let mut common = CommonFields::default();
        common.set_tse(-2);
        common.time = device_time;

        TselStamp::None.stamp(b"REC", &mut common, || false);

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
        use std::time::Duration;
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

        let stale = crate::types::WallTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut common = CommonFields::default();
        common.set_tse(-3);
        common.time = stale;

        TselStamp::None.stamp(b"X", &mut common, || false);

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
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 0, 0, 0, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), (0..16).collect::<Vec<_>>());
        assert!(r.alarm.is_none());

        // Specified, 0-based: SELN=0 selects LNK0 (C parity, fanout).
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 0, 0, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![0]);
        // Specified with OFFS bias: SELN=2 + OFFS=3 → index 5.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 2, 3, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![5]);
        // Out-of-range Specified → INVALID alarm, no links.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 20, 0, 0);
        assert_eq!(r.selected, 0);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // Negative resolved index (SELN + negative OFFS) → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, 0, -1, 0);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));

        // Mask: SELN=0b101 → bits 0 and 2.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![0, 2]);
        // Mask with SHFT: SELN=0b101 >> 1 = 0b10 → bit 1.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 1);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![1]);
        // Mask with negative SHFT: SELN=0b101 << 1 = 0b1010 → bits 1,3.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, -1);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![1, 3]);
        // SHFT out of [-15,15] → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, 5, 0, 16);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));

        // Unknown SELM → INVALID.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 9, 0, 0, 0);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
    }

    #[test]
    fn select_link_indices_dfanout_specified_is_one_based() {
        use crate::server::record::AlarmSeverity;
        // dfanout Specified is 1-based: SELN=1 → OUTA (index 0).
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 1, 0, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![0]);
        // SELN=2 → OUTB (index 1).
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 2, 0, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![1]);
        // SELN=0 → drive nothing, NO alarm.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 0, 0, 0);
        assert_eq!(r.selected, 0);
        assert!(r.alarm.is_none());
        // SELN > 16 → INVALID.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, 17, 0, 0);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // dfanout Mask has no SHFT — SHFT arg ignored.
        let r = select_link_indices_ex(SelmKind::Dfanout, 2, 5, 0, 7);
        assert_eq!(r.indices().collect::<Vec<_>>(), vec![0, 2]);
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
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 1, seln_max, 0, 0);
        assert_eq!(r.selected, 0);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
        // fanout/seq Mask: 65535 → all 16 low bits set → every link. A signed
        // read would produce an empty mask.
        let r = select_link_indices_ex(SelmKind::FanoutSeq, 2, seln_max, 0, 0);
        assert_eq!(r.indices().collect::<Vec<_>>(), (0..16).collect::<Vec<_>>());
        // dfanout Specified: 65535 > count → INVALID. A signed read would see
        // -1 ≤ 0 → drive nothing, with no alarm.
        let r = select_link_indices_ex(SelmKind::Dfanout, 1, seln_max, 0, 0);
        assert_eq!(r.selected, 0);
        assert_eq!(r.alarm, Some((15, AlarmSeverity::Invalid)));
    }

    /// Lset that flips to "connected" at `connect_at`.
    struct DelayedConnectLset {
        names: Vec<String>,
        connect_at: crate::runtime::task::Instant,
    }

    #[async_trait::async_trait]
    impl link_set::LinkSet for DelayedConnectLset {
        fn is_connected(&self, _: link_set::LinkRef<'_>) -> bool {
            crate::runtime::task::Instant::now() >= self.connect_at
        }
        fn get_cached_value(&self, _: link_set::LinkRef<'_>) -> Option<EpicsValue> {
            None
        }
        async fn get_value(&self, link: link_set::LinkRef<'_>) -> Option<EpicsValue> {
            self.get_cached_value(link)
        }
        fn link_names(&self) -> Vec<crate::types::RecordName> {
            self.names.iter().map(|n| n.as_str().into()).collect()
        }
    }

    /// C's `pdbentry->message` is one per `record()` block: a dbCommon field
    /// refused in the creation sink prints what the record's own fields of
    /// the same block left there.
    #[epics_macros_rs::epics_test]
    async fn a_common_field_refusal_reads_the_block_s_message() {
        let db = PvDatabase::new();
        let mut entry = crate::server::db_loader::EntryMessage::default();
        entry.cant_set_line("P", "SCAN", "Bogus", Some("using menu menuScan"), "x");
        let mut lines = Vec::new();
        db.add_loaded_record_reporting(
            "P",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
            RecordLoad::from_common_fields(vec![(
                "DISV".to_string(),
                EpicsValue::String("x".into()),
            )]),
            &mut entry,
            |_, d| lines.push(d.message),
        )
        .await
        .unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0]
                .ends_with("Can't set 'P.DISV' to 'x' using menu menuScan : No digits to convert"),
            "{}",
            lines[0]
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
        // `dbChannelTest` stops at the `$` modifier: whether the field takes
        // it is `dbChannelCreate`'s question (`dbChannel.c:486-505`), so the
        // search answers for VAL$ on a DOUBLE as softIocPVX does.
        assert!(db.has_name("TARGET.EGU$").await);
        assert!(db.has_name("TARGET.VAL$").await);
        assert!(db.has_name("WF.VAL$").await);
        assert!(!db.has_name("TARGET.NOSUCH$").await);
        // Nor does it read past the field name's identifier prefix.
        assert!(db.has_name("TARGET.VAL$garbage").await);
        assert!(db.has_name("TARGET.VAL.{bad").await);
        assert!(db.has_name("TARGET.1abc").await);
        assert!(!db.has_name("TARGET.NOSUCH$garbage").await);
        db.add_record(
            "WS",
            Box::new(crate::server::records::waveform::WaveformRecord::new(
                2,
                crate::types::DbFieldType::String,
            )),
        )
        .await
        .unwrap();
        assert!(db.has_name("WS.VAL$").await);
        assert!(!db.has_name("TARGET.egu").await);
    }

    /// The search side answers from registry facts while another thread
    /// holds the record, as C's `dbChannelTest` takes no record lock.
    #[epics_macros_rs::epics_test]
    async fn the_search_answers_while_the_record_is_held() {
        let db = PvDatabase::new();
        db.add_record(
            "TARGET",
            Box::new(crate::server::records::ai::AiRecord::new(42.0)),
        )
        .await
        .unwrap();
        let rec = db.get_record("TARGET").unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _guard = rec.write();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let asker = {
            let db = db.clone();
            std::thread::spawn(move || {
                let answers: Vec<bool> =
                    ["TARGET.VAL", "TARGET.MLOK", "TARGET.EGU$", "TARGET.NOSUCH"]
                        .iter()
                        .map(|name| db.has_name_no_resolve(name.as_bytes()))
                        .collect();
                answer_tx.send(answers).unwrap();
            })
        };
        let answers = answer_rx.recv_timeout(fact_budget::FACT_BUDGET);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        asker.join().unwrap();
        assert_eq!(answers, Ok(vec![true, true, true, false]));
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
    /// A link target handle holds its target's `Arc`, so two records that
    /// read each other hold each other; the database's drop must still free
    /// both. The strong count proves the handle is strong, the `Weak`
    /// proves the sweep breaks the cycle.
    #[epics_macros_rs::epics_test]
    async fn dropping_the_database_frees_records_that_link_to_each_other() {
        let db = PvDatabase::new();
        for (name, other) in [("CYC:A", "CYC:B"), ("CYC:B", "CYC:A")] {
            let mut rec = crate::server::records::calc::CalcRecord::new("A+1");
            rec.put_field(
                "INPA",
                EpicsValue::String(format!("{other} NPP NMS").into()),
            )
            .unwrap();
            db.add_record(name, Box::new(rec)).await.unwrap();
        }
        db.process_record("CYC:A").await.unwrap();
        db.process_record("CYC:B").await.unwrap();
        let a = db.get_record("CYC:A").unwrap();
        let b = db.get_record("CYC:B").unwrap();
        assert_eq!(
            (Arc::strong_count(&a), Arc::strong_count(&b)),
            (4, 4),
            "the map, the record table, this test and the other record's handle \
             each hold the cell"
        );
        let (weak_a, weak_b) = (Arc::downgrade(&a), Arc::downgrade(&b));
        drop((a, b, db));
        assert!(
            weak_a.upgrade().is_none() && weak_b.upgrade().is_none(),
            "the cycle of handles outlived the database"
        );
    }

    /// The other half of the rule: a record's removal drops every handle
    /// that named it, so a holder that never processes again does not keep
    /// the removed record alive.
    #[epics_macros_rs::epics_test]
    async fn removing_a_record_releases_every_handle_to_it() {
        let db = PvDatabase::new();
        let mut reader = crate::server::records::calc::CalcRecord::new("A");
        reader
            .put_field("INPA", EpicsValue::String("REL:SRC NPP NMS".into()))
            .unwrap();
        db.add_record(
            "REL:SRC",
            Box::new(crate::server::records::calc::CalcRecord::new("1")),
        )
        .await
        .unwrap();
        db.add_record("REL:READER", Box::new(reader)).await.unwrap();
        db.process_record("REL:READER").await.unwrap();
        let src = db.get_record("REL:SRC").unwrap();
        assert_eq!(
            Arc::strong_count(&src),
            4,
            "the map, the record table, the reader's handle, this test"
        );
        assert!(db.remove_record("REL:SRC").await);
        assert_eq!(
            Arc::strong_count(&src),
            2,
            "the reader's handle outlived the removal"
        );
    }

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
        assert!(!aliases.iter().any(|a| a == "TARGET"));
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
        let mut visited = ProcStack::new();
        db.process_record_with_links("ALIAS", &mut visited)
            .await
            .unwrap();
        assert!(
            visited.is_empty(),
            "a finished frame leaves no marker behind: {visited:?}",
        );

        let mut seeded = epics_base_rs::server::database::ProcStack::new();
        let target = db.get_record("TARGET").unwrap();
        seeded.claim(&target);
        db.process_record_with_links("ALIAS", &mut seeded)
            .await
            .unwrap();
        assert!(
            seeded.holds(&target),
            "the alias resolves to the seeded cell: {seeded:?}",
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
        let noop_hook: crate::server::pv::WriteHook = std::sync::Arc::new(|_v, _ctx| {
            Box::pin(async {
                let sent: crate::server::pv::WriteForward = Box::new(|_| Ok(()));
                Ok(sent)
            })
        });
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
        assert_eq!(db.resolve_alias("KEEPER"), Some("OTHER".into()));
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

    /// An owed `init_record` is the record's OWN debt, and it is claimed
    /// exactly once.
    ///
    /// Boundaries: a record created outside the LOAD phase owes nothing (it
    /// inited in place), one created inside it owes its init until the
    /// barrier, and a second crossing of the barrier finds nothing left to
    /// claim — the build lifecycle crosses it twice, before `setup_io_intr`
    /// and again from `ioc_init`, and C's `initDatabase` pass runs once.
    #[epics_macros_rs::epics_test]
    async fn a_load_phase_record_owes_its_init_until_the_barrier_claims_it() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("EAGER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        assert!(!db.record_init_deferred(b"EAGER"));

        db.begin_load().unwrap();
        db.add_record("OWED", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        assert!(db.record_init_deferred(b"OWED"));
        assert!(!db.record_init_deferred(b"EAGER"));

        let owed = db.get_record("OWED").expect("the record is published");
        db.drain_deferred_record_inits(&|line| eprintln!("{line}"));
        assert!(!db.record_init_deferred(b"OWED"));
        assert!(
            !owed.claim_owed_init(),
            "the barrier's claim was not the only one"
        );

        db.drain_deferred_record_inits(&|line| eprintln!("{line}"));
        assert!(!db.record_init_deferred(b"OWED"));
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
        let r1 = crate::runtime::task::timeout(fact_budget::FACT_BUDGET, h1)
            .await
            .expect("add_pv must not block on add_record");
        let r2 = crate::runtime::task::timeout(fact_budget::FACT_BUDGET, h2)
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
    /// and `links::CP_INPUT_LINK_FIELDS`; the first and third are deleted, so
    /// their content is repeated here as the oracle rather than read from it.
    /// The third carried one
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

        for field in ["INP", "OUT", "TSEL", "SDIS", "FLNK"] {
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
    /// `common.text.inp` — it is not a `DbFieldType::String` entry in
    /// `field_list()` — so the earlier `field_list()` scan filtered by
    /// `String` silently dropped it. The pvalink install scan walks this
    /// method, so a Passive `ai` carrying a CP/CPP pvalink `INP` never had
    /// its monitor opened at iocInit. Enumerating the canonical
    /// `common.text.inp` storage fixes it; C `dbpvar`/`dbcar` likewise dump
    /// every link field including device-support INP/OUT.
    #[epics_macros_rs::epics_test]
    async fn record_link_fields_surfaces_device_support_inp() {
        use crate::server::record::ParsedLink;
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        // A `pva` link set has to be installed for a pva link to survive
        // `db_init_link_locality`, which refuses a link whose scheme nothing
        // can service. This test is about the ENUMERATION reaching
        // `common.text.inp`, so it installs the lset the link names rather than
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
        // Device-support INP lives in `common.text.inp` (DBF_INLINK), the
        // exact storage a `field_list()` String scan cannot reach.
        {
            let rec = db.get_record("AI").unwrap();
            rec.write().common.text.inp = r#"{pva:"mini:current?proc=CP"}"#.into();
        }

        let links = db.record_link_fields("AI");
        let inp = links
            .iter()
            .find(|(f, _, _)| f == "INP")
            .unwrap_or_else(|| panic!("INP link must be surfaced, got {links:?}"));
        assert_eq!(inp.1, r#"{pva:"mini:current?proc=CP"}"#);
        assert!(
            matches!(inp.2, ParsedLink::Pva(_)),
            "a pva INP must parse to ParsedLink::Pva, got {:?}",
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

        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while !table().contains("dbCaLink") {
            assert!(
                std::time::Instant::now() < deadline,
                "`dbCaLink` never reached the watchdog table:\n{}",
                table()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// C `dbCaLinkInit` waits for `dbCaTask` to start (`dbCa.c:355-357`):
    /// the `dbCaLink` owner has registered by the time `ca_link_init`
    /// returns, so its first poll is not left to land in record processing.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ca_link_init_returns_with_the_owner_registered() {
        let db = PvDatabase::new();
        assert!(db.ca_link_init(), "precondition: a captured reactor");
        let out = std::cell::RefCell::new(String::new());
        crate::runtime::taskwd::taskwd_show(1, &|line| {
            out.borrow_mut().push_str(line);
            out.borrow_mut().push('\n');
        });
        let out = out.into_inner();
        assert!(
            out.contains("dbCaLink"),
            "the owner was not registered when ca_link_init returned:\n{out}"
        );
    }

    /// C `dbCaShutdown` then a rebuild's `dbCaLinkInit` (`dbCa.c:324-355`):
    /// the stopped worker is gone from the watchdog table and the rebuild
    /// starts a fresh one.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rebuild_after_ca_link_shutdown_gets_a_fresh_worker() {
        fn up() -> bool {
            let up = std::cell::Cell::new(false);
            crate::runtime::taskwd::taskwd_show(1, &|line| {
                up.set(up.get() || line.contains("dbCaLink"))
            });
            up.get()
        }
        let db = PvDatabase::new();
        assert!(db.ca_link_init());
        assert!(up());
        db.ca_link_shutdown();
        assert!(!up(), "the stopped worker is still registered");
        assert!(db.ca_link_init());
        assert!(up(), "the rebuild started no worker");
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
