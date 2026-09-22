/// The records whose `dbProcess` frame is live on the current chain.
///
/// C keeps this marker on the record — `processTarget` claims
/// `dbRec2Pvt(pdst)->procThread` before `dbProcess(pdst)`
/// (`dbDbLink.c:500-503`) and the frame that claimed it clears it on unwind
/// (`:523-526`). The port cannot put it there: its frame has released the
/// record lock by the time it unwinds, and re-taking it to clear a flag would
/// cost more than the marker saves. So the marker travels with the chain.
///
/// What it travelled in was a `HashSet<Arc<str>>`, which hashed the record
/// name on the way in and again on the way out and allocated a table to hold,
/// at the depth a scan cycle actually reaches, one entry. The depth is the
/// point: a scan of a record whose links are unwired is depth one, so the
/// first claim lives in a field and only a real cascade allocates.
///
/// The entry is the record cell's identity, as C's marker is on the record
/// itself: an alias claims the same entry as its target, and a claim costs no
/// name clone or compare.
#[derive(Debug, Default)]
pub struct ProcStack {
    /// Depth one.
    head: Option<CellId>,
    /// Depth two and beyond.
    rest: Vec<CellId>,
}

/// A record cell's address, compared and never dereferenced. It stays unique
/// while it is on the stack because the frame that claimed it holds the
/// cell's `Arc` until it releases it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellId(usize);

impl CellId {
    fn of(rec: &Arc<RecordCell>) -> Self {
        CellId(Arc::as_ptr(rec) as usize)
    }
}

impl ProcStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim `name` for the calling frame. `false` when it is already on the
    /// chain — C's cycle — and then the caller has claimed nothing and must
    /// not release anything.
    pub fn claim(&mut self, rec: &Arc<RecordCell>) -> bool {
        if self.holds(rec) {
            return false;
        }
        let id = CellId::of(rec);
        match self.head {
            None => self.head = Some(id),
            Some(_) => self.rest.push(id),
        }
        true
    }

    /// Release what [`Self::claim`] took, on the frame's unwind.
    pub(crate) fn release(&mut self, rec: &Arc<RecordCell>) {
        let id = CellId::of(rec);
        if let Some(i) = self.rest.iter().rposition(|n| *n == id) {
            self.rest.remove(i);
        } else if self.head == Some(id) {
            self.head = None;
        }
    }

    /// How many frames are live on this chain.
    pub fn len(&self) -> usize {
        usize::from(self.head.is_some()) + self.rest.len()
    }

    /// Whether no frame is live on this chain — the entry is the outermost.
    pub fn is_empty(&self) -> bool {
        self.head.is_none() && self.rest.is_empty()
    }

    /// Whether a frame for `rec` is live on this chain.
    pub fn holds(&self, rec: &Arc<RecordCell>) -> bool {
        let id = CellId::of(rec);
        self.head == Some(id) || self.rest.contains(&id)
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{CaError, CaResult};
use crate::server::record::{
    InputFetchPolicy, NotifyWaitSet, PactExit, RawSoftEntry, RecordCell, RecordInstance,
};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::{MetadataPlan, PvDatabase};

/// C `sCalcoutRecord.c` `STRING_SIZE` (:198) — the 40-byte buffer behind every
/// string field a string-input link writes into. The text therefore carries at
/// most 39 bytes plus the NUL, which is what `epicsSnprintf(..., STRING_SIZE-1,
/// ...)` and `epicsStrSnPrintEscaped(..., STRING_SIZE-1, ...)` enforce in C.
const STRING_FIELD_MAX_LEN: usize = 39;

/// **The single owner of "this record's processing cycle was refused."**
///
/// C publishes a refused cycle exactly once, in `dbProcess`'s `MAX_LOCK`
/// branch (`dbAccess.c:544-556`):
///
/// ```c
/// recGblSetSevrMsg(precord, SCAN_ALARM, INVALID_ALARM, "Async in progress");
/// monitor_mask = recGblResetAlarms(precord);
/// monitor_mask |= DBE_VALUE|DBE_LOG;
/// db_post_events(precord, ((char *)precord) + pdbFldDes->offset, monitor_mask);
/// ```
///
/// so a refusal is never a silent success: the record carries SCAN_ALARM /
/// INVALID with the reason in `AMSG`, and the transition is posted. The one
/// refusal the port can make, C's `MAX_LOCK` re-entry, routes through here;
/// like C, the port has no link-depth bound.
///
/// Returns the post set for the caller to hand to `notify_from_snapshot` after
/// releasing the write guard, or `None` when the record already carries this
/// refusal — C's `if (precord->stat == SCAN_ALARM) goto all_done`, which is
/// what keeps a repeatedly refused record from re-posting every cycle.
fn scan_alarm_refusal(
    instance: &mut RecordInstance,
    msg: &str,
) -> Option<crate::server::record::ProcessSnapshot> {
    use crate::server::recgbl::EventMask;
    if instance.common.stat == crate::server::recgbl::alarm_status::SCAN_ALARM
        && instance.common.sevr >= crate::server::record::AlarmSeverity::Invalid
    {
        return None;
    }
    crate::server::recgbl::rec_gbl_set_sevr_msg(
        &mut instance.common,
        crate::server::recgbl::alarm_status::SCAN_ALARM,
        crate::server::record::AlarmSeverity::Invalid,
        msg,
    );
    let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);
    // Post VAL with VALUE|LOG|ALARM (C `db_post_events(prec, &VAL,
    // DBE_VALUE|DBE_LOG)` plus recGblResetAlarms' `val_mask = DBE_ALARM` for
    // the fresh transition). The alarm fields carry their C per-field masks
    // (recGbl.c:202-222): this only runs on a fresh SCAN_ALARM/INVALID raise,
    // so sevr AND stat both moved — SEVR posts DBE_VALUE, STAT/AMSG post the
    // shared `stat_mask` = DBE_ALARM|DBE_VALUE.
    let stat_mask = EventMask::ALARM | EventMask::VALUE;
    let mut changed_fields = crate::server::record::ProcessSnapshot::new();
    if let Some(val) = instance.record.val() {
        changed_fields.push((
            "VAL".into(),
            val,
            EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
        ));
    }
    changed_fields.push((
        "SEVR".into(),
        EpicsValue::Short(instance.common.sevr as i16),
        EventMask::VALUE,
    ));
    changed_fields.push((
        "STAT".into(),
        EpicsValue::Short(instance.common.stat as i16),
        stat_mask,
    ));
    // Include AMSG so subscribers reading the alarm text observe the reason
    // alongside the SCAN_ALARM transition (C `recGbl.c:210-211` posts STAT and
    // AMSG together when `stat_mask` is non-zero).
    changed_fields.push((
        "AMSG".into(),
        EpicsValue::String(instance.common.amsg.as_str().into()),
        stat_mask,
    ));
    Some(changed_fields)
}

/// Cut a string-link value to the C field width (see [`STRING_FIELD_MAX_LEN`]).
fn truncate_string_field(s: PvString) -> PvString {
    let bytes = s.as_bytes();
    if bytes.len() <= STRING_FIELD_MAX_LEN {
        return s;
    }
    PvString::from_bytes(&bytes[..STRING_FIELD_MAX_LEN])
}

/// The DBR_STRING view of a [`Record::string_input_links`](crate::server::record::Record::string_input_links) source, C
/// `sCalcoutRecord.c::fetch_values` (895-937).
///
/// A `DBF_CHAR`/`DBF_UCHAR` source of more than one element is the one type C
/// does NOT read as DBR_STRING (which would render element 0 as a number):
/// it reads the array as text and escapes it with `epicsStrSnPrintEscaped`
/// (`epicsString.c:230-261`), which is how a string longer than a DBR_STRING —
/// or one carrying control characters — reaches a string calc. C caps the
/// request at `STRING_SIZE-1` elements before the get and treats the result as
/// a C string (`strlen(tmpstr)`), so the source is cut at 39 bytes and at the
/// first NUL. Every other source type takes the plain `dbGetLink(DBR_STRING)`
/// branch, i.e. the framework's own `DbFieldType::String` coercion.
fn string_link_text(value: &EpicsValue) -> PvString {
    let char_array_bytes = match value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) if b.len() > 1 => Some(b),
        _ => None,
    };
    if let Some(bytes) = char_array_bytes {
        let src = &bytes[..bytes.len().min(STRING_FIELD_MAX_LEN)];
        let src = &src[..src.iter().position(|&b| b == 0).unwrap_or(src.len())];
        let mut out = String::with_capacity(src.len());
        for &b in src {
            match b {
                0x07 => out.push_str("\\a"),
                0x08 => out.push_str("\\b"),
                0x0c => out.push_str("\\f"),
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                b'\t' => out.push_str("\\t"),
                0x0b => out.push_str("\\v"),
                b'\\' => out.push_str("\\\\"),
                b'\'' => out.push_str("\\'"),
                b'"' => out.push_str("\\\""),
                // C `isprint` in the "C" locale: ASCII 0x20..0x7e. Everything
                // else — including the high half — is escaped `\xHH`.
                _ if b.is_ascii_graphic() || b == b' ' => out.push(b as char),
                _ => out.push_str(&format!("\\x{b:02x}")),
            }
        }
        return truncate_string_field(PvString::from(out));
    }
    match value.convert_to(DbFieldType::String) {
        EpicsValue::String(s) => truncate_string_field(s),
        _ => PvString::new(),
    }
}

/// A cancellable, generation-gated handle that re-enters an async record's
/// `process()` exactly once.
///
/// C parity: epics-base `callbackRequest` / `callbackRequestDelayed`
/// (`callback.c`) post a one-shot callback that later runs the record's
/// `(*prset->process)(precord)` directly, bypassing `dbProcess`'s PACT
/// entry guard. Here, firing the token re-enters via
/// [`PvDatabase::process_record_continuation`] (the owner-driven
/// continuation that also bypasses the PACT guard).
///
/// # Cancellation is structural, not a runtime check
///
/// The record owns a monotonic generation counter (`reprocess_generation`).
/// Minting a token snapshots that counter as the token's `epoch` *after*
/// bumping it, so:
///
/// - minting a newer token for the same record (C `callbackRequestDelayed`
///   replacing an outstanding delayed callback), or
/// - [`PvDatabase::cancel_async_reentry`] (C `callbackCancelDelayed`),
///
/// each advance the counter past every outstanding token's `epoch`. A
/// stale token therefore re-enters *nothing*: [`AsyncToken::fire`] is the
/// sole re-entry path, the epoch comparison is owned in one place, and the
/// token is consumed (`self` by value) so it cannot fire twice. A consumer
/// never writes an `if generation == ...` guard — it holds the token and
/// calls `fire`; the no-op-when-stale is guaranteed by construction.
pub struct AsyncToken {
    /// Canonical record name to re-enter.
    name: String,
    /// Shared generation counter owned by the record
    /// (`RecordInstance::reprocess_generation`).
    generation: Arc<AtomicU64>,
    /// Generation value captured at mint time. The token is current iff
    /// `generation == epoch`.
    epoch: u64,
}

impl AsyncToken {
    /// The record this token re-enters.
    pub fn record_name(&self) -> &str {
        &self.name
    }

    /// True iff this token is still the current generation — no newer
    /// token was minted and no [`PvDatabase::cancel_async_reentry`] has
    /// run for the record since this token was minted. Read-only.
    pub fn is_current(&self) -> bool {
        self.generation.load(Ordering::Acquire) == self.epoch
    }

    /// Cancel this token (C `callbackCancelDelayed` for the holder's own
    /// pending re-entry): advance the generation so this and any other
    /// outstanding token for the record become stale, then consume the
    /// token. Use when the holder itself decides not to re-enter; use
    /// [`PvDatabase::cancel_async_reentry`] to cancel a token already
    /// handed to a timer / notify task.
    pub fn cancel(self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Fire the continuation: if still current, re-enter the record's
    /// `process()` via [`PvDatabase::process_record_continuation`]. A
    /// stale (superseded / cancelled) token is a no-op. Consumes the
    /// token so it cannot fire twice.
    pub async fn fire(self, db: &PvDatabase) -> CaResult<()> {
        if self.generation.load(Ordering::Acquire) != self.epoch {
            return Ok(());
        }
        let mut visited = ProcStack::new();
        db.process_record_continuation(&self.name, &mut visited)
            .await
    }
}

/// A cycle-free handle for driving async-side database updates from
/// OUTSIDE a record's `process()` cycle.
///
/// Wraps a [`std::sync::Weak`] reference to the database: a record stashes
/// it (via [`crate::server::record::Record::set_async_context`]) without
/// creating an ownership cycle — the database owns the record, so a strong
/// `Arc<PvDatabaseInner>` stored on the record would leak the whole
/// database. Every call upgrades the `Weak` to a temporary [`PvDatabase`];
/// once the last strong owner drops, the upgrade fails and the call is a
/// no-op (nothing is stranded).
///
/// This is the out-of-band counterpart to the in-band re-entry
/// [`crate::server::record::ProcessAction`]s: a driver / callback thread
/// (asyn TRACE post, AQR cancel, motor intermediate readback) holds the
/// handle and pushes field updates or wires a completion-driven re-entry
/// without going through `process()`. It exposes exactly the c401e2f0
/// PACT primitive surface, each call guarded by the live-database check.
#[derive(Clone)]
pub struct AsyncDbHandle {
    inner: std::sync::Weak<super::PvDatabaseInner>,
}

impl AsyncDbHandle {
    /// Upgrade to a temporary owning [`PvDatabase`], or `None` if the
    /// database has been dropped.
    fn db(&self) -> Option<PvDatabase> {
        self.inner.upgrade().map(|inner| PvDatabase { inner })
    }

    /// True while the backing database is still alive.
    pub fn is_alive(&self) -> bool {
        self.inner.strong_count() > 0
    }

    /// Out-of-band field post — see [`PvDatabase::post_fields`]. Returns an
    /// empty `Vec` (no-op) if the database has been dropped.
    pub fn post_fields(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
    ) -> CaResult<Vec<String>> {
        match self.db() {
            Some(db) => db.post_fields(name, fields),
            None => Ok(Vec::new()),
        }
    }

    /// Out-of-band field post under the caller's own event mask — see
    /// [`PvDatabase::post_fields_with_mask`]. Returns an empty `Vec` (no-op)
    /// if the database has been dropped.
    pub(crate) fn post_fields_with_mask(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
        mask: crate::server::recgbl::EventMask,
    ) -> CaResult<Vec<String>> {
        match self.db() {
            Some(db) => db.post_fields_with_mask(name, fields, mask),
            None => Ok(Vec::new()),
        }
    }

    /// C `dbCaPutLinkCallback`'s return status, asked before the put is
    /// issued: would a put-WITH-completion to `link` be admitted right now?
    ///
    /// The gate is `if (!pca->isConnected || !pca->hasWriteAccess) return -1;`
    /// (`dbCa.c:529-532`), and `PvDatabase::external_put_admitted` is the same
    /// owner [`Self::put_link_notify`]'s write path consults, so the two cannot
    /// disagree. Non-blocking and does no I/O — it reads the link set's cached
    /// connection state, which is why it can be asked from inside `process()`
    /// while the put itself must be deferred.
    ///
    /// A link whose target is a LOCAL record is C's non-`CA_LINK` case, which
    /// never reaches that gate (`dbPutLink`, no callback): `true`. So is a
    /// database that has been dropped — nothing is left to refuse.
    pub fn put_link_admitted(&self, link: &str) -> bool {
        let Some(db) = self.db() else {
            return true;
        };
        match crate::server::record::parse_output_link_v2(link) {
            crate::server::record::ParsedLink::Db(target) => {
                // C `dbInitLink` locality (`dbLink.c:118-130`): a record this
                // IOC does not hold is a CA link, and the port routes its write
                // through the same external path.
                if db.has_name_no_resolve(&target.target().record) {
                    return true;
                }
                db.external_put_admitted(&target.pvname()).is_ok()
            }
            other => match other.external_pv_name() {
                Some(name) => db.external_put_admitted(&name).is_ok(),
                // Constant / empty: C's switch makes no put at all, so there is
                // no status to read.
                None => true,
            },
        }
    }

    /// Resolve a link's target field type for the sseq link-status
    /// diagnostics — see `PvDatabase::link_target_field_type`. `None` if
    /// the link is constant / external / unresolvable, or the database is
    /// gone. (Distinct from the free `server::record::link_field_type`,
    /// which returns the link *class* `LinkType`, not the target's type.)
    pub fn link_target_field_type(&self, link: &str) -> Option<crate::types::DbFieldType> {
        match self.db() {
            Some(db) => db.link_target_field_type(link),
            None => None,
        }
    }

    /// Schedule a record's link-status classification — see
    /// `PvDatabase::schedule_record_init`. This is the ONE owner every
    /// record's `refresh_link_status` goes through: during the LOAD phase the
    /// classification is queued for `iocInit` (so it never reads a half-built
    /// database, and its result is final when `iocInit` returns), and on a
    /// complete database it is spawned at once. Dropped, unrun, if the database
    /// is gone.
    pub fn schedule_record_init(
        &self,
        record: &str,
        init: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        if let Some(db) = self.db() {
            db.schedule_record_init(record, init);
        }
    }

    /// Read a link's value WITHOUT processing its source record — the C
    /// `dbGetLink` semantics. Parses `link` and reads it via
    /// `PvDatabase::read_link_value_no_process`; `None` if the link is
    /// constant-less / external-unresolvable or the database has been
    /// dropped. Used by module-crate records (e.g. std `throttle` SYNC →
    /// `SINP`→`VAL`) that must pull an input link from `special()` without
    /// triggering a process cycle.
    pub async fn read_link_value(&self, link: &str) -> Option<EpicsValue> {
        let db = self.db()?;
        let parsed = crate::server::record::parse_link_v2(link);
        db.read_link_value_no_process(&parsed)
    }

    /// Out-of-band `dbPutField` on any record field, common fields included —
    /// see [`PvDatabase::put_pv`]. `Ok(())` (no-op) if the database has been
    /// dropped.
    ///
    /// Unlike [`Self::post_fields`] (which writes through `put_field_internal`
    /// and only posts), this is the full put path: a `SCAN` write moves the
    /// record between scan buckets and fires the `get_ioint_info` hook. C
    /// records call `dbPutField` on their own fields exactly this way — asynRecord's
    /// `cancelIOInterruptScan` does `dbPutField(&scanAddr, DBR_LONG,
    /// &passiveScan, 1)` on its own `.SCAN` (asynRecord.c:794-806).
    pub async fn put_pv(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        match self.db() {
            Some(db) => db.put_pv(name, value).await,
            None => Ok(()),
        }
    }

    /// Mint an async re-entry token — see [`PvDatabase::mint_async_token`].
    /// `None` if the record is absent or the database has been dropped.
    pub fn mint_async_token(&self, name: &str) -> Option<AsyncToken> {
        match self.db() {
            Some(db) => db.mint_async_token(name),
            None => None,
        }
    }

    /// Cancel an outstanding async re-entry — see
    /// [`PvDatabase::cancel_async_reentry`]. No-op if the database is gone.
    pub fn cancel_async_reentry(&self, name: &str) {
        if let Some(db) = self.db() {
            db.cancel_async_reentry(name);
        }
    }

    /// Arm a put-notify wait-set — see [`PvDatabase::new_put_notify`].
    /// Database-independent (re-exported associated fn).
    pub fn new_put_notify() -> (
        Arc<NotifyWaitSet>,
        crate::runtime::sync::oneshot::Receiver<()>,
    ) {
        PvDatabase::new_put_notify()
    }

    /// Wire a completion oneshot to an async re-entry — see
    /// [`PvDatabase::reprocess_on_notify`]. `None` if the database is gone
    /// (the `completion` receiver is dropped, stranding nothing).
    pub fn reprocess_on_notify(
        &self,
        token: AsyncToken,
        completion: crate::runtime::sync::oneshot::Receiver<()>,
    ) -> Option<crate::runtime::task::BackgroundTaskHandle<()>> {
        self.db()
            .map(|db| db.reprocess_on_notify(token, completion))
    }

    /// Issue a non-blocking put-with-completion to an OUT link — see
    /// [`PvDatabase::put_link_notify`]. `None` if the database is gone or
    /// the source record is missing.
    pub async fn put_link_notify(
        &self,
        record_name: &str,
        link_field: &str,
        link_str: &str,
        value: EpicsValue,
    ) -> Option<crate::runtime::sync::oneshot::Receiver<()>> {
        match self.db() {
            Some(db) => {
                db.put_link_notify(record_name, link_field, link_str, value)
                    .await
            }
            None => None,
        }
    }
}

/// C `dbNotifyCompletion` (`dbNotify.c:445`) reached the way a process cycle
/// reaches it — through `recGblFwdLink` (`recGbl.c:295`), record support's only
/// route to it. Take this record's wait-set membership and leave; the
/// completion oneshot fires on the `leave` that empties the set.
///
/// # Invariant (CONTRACT)
///
/// A cycle completes an outstanding put-notify IF AND ONLY IF it runs
/// `recGblFwdLink`. Two things stop it, and **both are read here** so no cycle
/// tail can consult one and forget the other:
///
/// - [`Record::is_put_complete`](crate::server::record::Record::is_put_complete)
///   — device support took the write async
///   (`if (!pact && prec->pact) return(0)`), so this pass never reaches the
///   tail.
/// - [`Record::should_fire_forward_link`](crate::server::record::Record::should_fire_forward_link)
///   — the tail was reached and the record type declined it.
///   `dbNotifyCompletion` sits INSIDE the skipped call, so a suppressed
///   forward link withholds the `ca_put_callback` too.
///   `busy` is the clearest case: `busyRecord.c:271` runs the tail only for
///   `val == 0 || oval == 0`, which is why `caput -c` on a busy record that
///   stays at 1 is meant to hang until something writes "Done".
///
/// Reading them together HERE, rather than at each cycle tail, is what keeps a
/// record type's C gate to one override: a type states its gate in
/// `should_fire_forward_link` and gets the notify behaviour for free.
///
/// The SDIS-disable bail is C's OTHER `dbNotifyCompletion` caller
/// (`dbAccess.c:623`), outside `recGblFwdLink` and so deliberately ungated —
/// it open-codes the take/leave in `process_record_with_links_inner` and does
/// not come through here.
///
/// Idempotent: a record in no put-notify is a no-op.
/// Cycle-end bookkeeping under the record's data lock.
///
/// C `recGblFwdLink:302` clears `putf = FALSE` at the tail of every
/// synchronous cycle, NOT just the foreign-entry path: a record driven
/// through an OUT-link propagation (`write_db_link_value` set its putf)
/// must clear it before returning. Async-pending records skip the clear —
/// their FLNK / putf-clear happen later, in `complete_async_record_inner`,
/// once the device round-trip completes.
///
/// The record `leave`s the wait-set only here, after its full
/// OUT/FLNK/process-action tail has run — so every PP target it drove has
/// already joined (`enter`ed). Whether this cycle may leave at all is
/// `complete_put_notify`'s decision, not this site's: a record reporting
/// more work (motor mid-move) or declining its forward link (busy at
/// VAL=1) keeps its membership and leaves on the later cycle that reaches
/// C's `recGblFwdLink`.
fn finish_cycle(inst: &mut RecordInstance) {
    if !inst.is_processing() {
        inst.common.putf = false;
    }
    complete_put_notify(inst);
}

fn complete_put_notify(inst: &mut RecordInstance) {
    // No wait-set, nothing to leave: the usual cycle answers here, ahead of
    // the two record queries below, which are pure reads either way.
    if inst.notify.is_none() {
        return;
    }
    if !inst.record.is_put_complete() || !inst.record.should_fire_forward_link() {
        return;
    }
    if let Some(ws) = inst.notify.take() {
        ws.leave();
    }
}

/// Result of an aSub LFLG=READ subroutine re-resolution
/// (C `aSubRecord.c::fetch_values`). Computed outside the record's process
/// lock (the SUBL link read may touch another record) and applied inside it.
struct AsubDynamicSub {
    /// SNAM read from the SUBL link this cycle — written back to the record
    /// (C `dbGetLink` writes SNAM every READ cycle). `None` only when the
    /// link read failed (C `if (status) return status`), leaving SNAM as-is.
    snam: Option<String>,
    /// `Some` → swap the live subroutine and set ONAM to `snam` (the name
    /// changed and was found in the registry).
    swap: Option<Arc<crate::server::record::SubroutineFn>>,
    /// `true` → do not run the subroutine this cycle, matching C skipping
    /// `do_sub`: the link read failed, or the changed name was not registered
    /// (`S_db_BadSub`).
    skip_run: bool,
}

/// Apply an aSub LFLG=READ resolution (from
/// [`PvDatabase::resolve_asub_dynamic_subroutine`]) to a locked record: write
/// the read-back SNAM, swap the subroutine + set ONAM when the name changed,
/// and arm the one-shot suppress flag when the name was bad. The single apply
/// owner, shared by the engine path ([`PvDatabase::process_record_with_links_inner`])
/// and the foreign path ([`PvDatabase::process_record`]); the skip is consumed
/// uniformly by `RecordInstance::run_registered_subroutine`.
fn apply_asub_dynamic_sub(instance: &mut RecordInstance, ds: &AsubDynamicSub) {
    if let Some(snam) = &ds.snam {
        let _ = instance
            .record
            .put_field("SNAM", EpicsValue::String(snam.as_str().into()));
    }
    if let Some(func) = &ds.swap {
        instance.subroutine = Some(func.clone());
        if let Some(snam) = &ds.snam {
            let _ = instance
                .record
                .put_field("ONAM", EpicsValue::String(snam.as_str().into()));
        }
    }
    // One-shot, armed by any reason and cleared only by its owner
    // (`run_registered_subroutine`): OR-ed in, so a failed `fetch_values`
    // that armed it before this hook still skips the run.
    instance.suppress_subroutine_run |= ds.skip_run;
}

/// If a CA TSEL link's pvname targets a record's `.TIME` field, return
/// the record name with the `.TIME` suffix stripped; otherwise `None`.
///
/// Mirrors C `TSEL_modified` (dbLink.c:80-86): a `PV_LINK` tsel whose
/// pvname contains `.TIME` is flagged `DBLINK_FLAG_TSELisTIME` and the
/// name is truncated at `.TIME` to address the record. Matched on the
/// `.TIME` suffix (the realistic spelling) case-insensitively, to stay
/// consistent with the DB branch's `field.eq_ignore_ascii_case("TIME")`.
fn ca_tsel_time_record(pv: &str) -> Option<&str> {
    let idx = pv.len().checked_sub(".TIME".len())?;
    pv[idx..]
        .eq_ignore_ascii_case(".TIME")
        .then_some(&pv[..idx])
}

/// Convert an lset `(seconds_past_epoch, nanos, userTag)` timestamp
/// triple into the record-side `(SystemTime, userTag)` pair, clamping
/// seconds/nanos to the valid `Duration` range. Shared by the TSEL
/// `.TIME` Ca arm and the non-local Db arm — both read a `ca://` `.TIME`
/// source through `external_link_time` and adopt the result identically.
fn ext_time_pair((secs, ns, utag): (i64, i32, u64)) -> (std::time::SystemTime, u64) {
    let secs = secs.max(0) as u64;
    let ns = (ns.max(0) as u32).min(999_999_999);
    (
        std::time::UNIX_EPOCH + std::time::Duration::new(secs, ns),
        utag,
    )
}

/// The alarm-field events `recGblResetAlarms` posts (recGbl.c:202-222), each
/// with its own per-field mask:
///
/// * `SEVR` — `DBE_VALUE`, ONLY when `prev_sevr != new_sevr`.
/// * `STAT`/`AMSG` — `stat_mask` = `DBE_ALARM` (on sevr- or amsg-change) |
///   `DBE_VALUE` (on stat-change).
/// * `ACKS` — `DBE_VALUE`, only when `stat_mask != 0` and `recGblResetAlarms`
///   raised it.
///
/// NOT the single owner of these masks, despite an earlier comment here that
/// claimed so. Two of the five `recGblResetAlarms` post sites call this helper
/// — the synchronous process epilogue (`process_record_with_links_inner`) and
/// the `CompleteAlarmOnly` cycle that skips that epilogue (transform
/// IVLA="Do Nothing"). The other three still open-code the identical mask
/// arithmetic and can therefore drift from it:
///
/// * `complete_async_record_inner` — the async-completion epilogue;
/// * `sim_process_tail` — the SIMM-mode input tail;
/// * `RecordInstance::process_local` — the foreign-process / QSRV-group path.
///
/// (The SDIS-disable post in `process_record_with_links_inner` and the
/// fanout/seq SELN post in `links::apply_selm_alarm` are NOT clients: they
/// carry C's `dbAccess.c:586-593` and `fanoutRecord.c:116` masks, not
/// `recGblResetAlarms`'.)
/// The publication half of a process cycle: fan the snapshot out to
/// subscribers, post the alarm fields under their individual C masks, and
/// report which classes the cycle emitted.
///
/// Takes the guard the segment already holds. C publishes from inside
/// `monitor()`, which runs under the same `dbScanLock` that built the values
/// being published; the port used to drop its guard at the segment boundary
/// and immediately re-acquire it for this, paying a second acquisition per
/// cycle for a window in which it did nothing.
fn publish_cycle(
    instance: &mut RecordInstance,
    snapshot: &crate::server::record::ProcessSnapshot,
    backing: crate::server::database::LinkBacking<'_>,
    alarm_posts: AlarmPosts,
) -> CyclePosts {
    // A value-class post advances the record's already-published state
    // (`RecordInstance::record_value_post`), so this is a `&mut` operation.
    instance.notify_from_snapshot(snapshot, backing);
    let mut posts = CyclePosts::of(snapshot);
    alarm_posts.for_each(|field, mask| {
        instance.notify_field(field, mask);
        posts = posts.with(mask);
    });
    posts
}

pub(crate) fn alarm_field_posts(
    common: &crate::server::record::CommonFields,
    alarm_result: &crate::server::recgbl::AlarmResetResult,
) -> AlarmPosts {
    use crate::server::recgbl::EventMask;

    let sevr_changed = common.sevr != alarm_result.prev_sevr;
    let stat_changed = common.stat != alarm_result.prev_stat;
    let stat_mask = {
        let mut m = EventMask::NONE;
        if sevr_changed || alarm_result.amsg_changed {
            m |= EventMask::ALARM;
        }
        if stat_changed {
            m |= EventMask::VALUE;
        }
        m
    };
    AlarmPosts {
        sevr: sevr_changed,
        stat_mask,
        acks: alarm_result.acks_posted,
    }
}

/// The alarm-field posts of one `recGblResetAlarms`, as the three facts that
/// decide them (see [`alarm_field_posts`]). The posts are a fixed rule over
/// these facts, so this carries the facts and replays the rule on demand —
/// a list held them before, and the cycle that posts nothing, which is most
/// of them, built and dropped it every time.
#[derive(Clone, Copy, Debug)]
pub struct AlarmPosts {
    sevr: bool,
    stat_mask: crate::server::recgbl::EventMask,
    acks: bool,
}

impl AlarmPosts {
    /// Each post in the order `recGblResetAlarms` makes them — `SEVR`, `STAT`,
    /// `AMSG`, `ACKS` — with its own C mask.
    pub fn for_each(&self, mut f: impl FnMut(&'static str, crate::server::recgbl::EventMask)) {
        use crate::server::recgbl::EventMask;
        if self.sevr {
            f("SEVR", EventMask::VALUE);
        }
        if !self.stat_mask.is_empty() {
            f("STAT", self.stat_mask);
            f("AMSG", self.stat_mask);
        }
        if self.acks {
            f("ACKS", EventMask::VALUE);
        }
    }

    /// The posts as a list, for the callers that hold them.
    pub fn to_vec(self) -> Vec<(&'static str, crate::server::recgbl::EventMask)> {
        let mut out = Vec::new();
        self.for_each(|field, mask| out.push((field, mask)));
        out
    }
}

/// What one process cycle hands to its forward-link tail.
///
/// The CP/CPP dispatch at the tail needs what the cycle PUBLISHED (see
/// [`CyclePosts`]); the FLNK's own PUTF and put-notify wait-set ride inside
/// the [`ForwardTarget`](crate::server::record::record_instance::ForwardTarget)
/// the tail is handed, so only the target that needs them carries them.
#[derive(Clone, Copy)]
struct TailCtx<'a> {
    posts: CyclePosts,
    /// The cycle's `ProcessPlan`, so the tail's two type-static
    /// dispatchers can be skipped without re-taking the record's lock to ask
    /// what type it is.
    plan: &'a crate::server::record::record_instance::ProcessPlan,
}

/// What one process cycle published to monitors: the union of every `DBE_*`
/// class it posted, across the value snapshot and the `recGblResetAlarms`
/// fields.
///
/// This exists so the CP/CPP trigger reads a *post*, never a *process*. C
/// serves every CP/CPP link — local target or not — through a CA
/// subscription taken with `DBE_VALUE | DBE_ALARM` (`dbCa.c:1225-1229` →
/// `cadef.h:2010-2011`), and only its `eventCallback` adds `CA_DBPROCESS`
/// (`dbCa.c:955-963`, run at `:1249-1257`). A cycle that posts nothing —
/// an unchanged value inside `MDEL`, no alarm movement — therefore leaves
/// the holder unprocessed. Passing this value into the forward-link tail is
/// what makes "dispatch a CP edge without a post" unrepresentable at the
/// call site: there is no argument-less way to reach
/// [`PvDatabase::dispatch_cp_targets`].
#[derive(Clone, Copy)]
struct CyclePosts(crate::server::recgbl::EventMask);

impl CyclePosts {
    /// The classes a value snapshot published.
    fn of(snapshot: &crate::server::record::ProcessSnapshot) -> Self {
        Self(snapshot.published_mask())
    }

    /// Fold in one more posted field (the `recGblResetAlarms` posts, which
    /// are emitted outside the snapshot).
    fn with(self, mask: crate::server::recgbl::EventMask) -> Self {
        Self(self.0 | mask)
    }

    /// True when this cycle published a class a CP/CPP subscription selects.
    fn triggers_cp(self) -> bool {
        use crate::server::recgbl::EventMask;
        self.0.intersects(EventMask::VALUE | EventMask::ALARM)
    }
}

/// Result of the simulation-mode check.
///
/// C handles simulation entirely inside `readValue()` / `writeValue()` —
/// the device-I/O step — and `process()` ALWAYS runs the rest of the body
/// (`convert`/OROC/the record's own state machine) plus
/// `checkAlarms`/`monitor`/`recGblFwdLink(prec)`. SIMM replaces ONLY the
/// device read/write with the SIOL link, never the record-support body.
/// The two substitution points differ by direction: an INPUT record's
/// `readValue()` runs at the START of `process()` (before the body), so
/// [`SimOutcome::Simulated`] does the SIOL read here and short-circuits;
/// an OUTPUT record's `writeValue()` runs at the END (after the body has
/// computed OVAL / armed bo HIGH), so [`SimOutcome::RedirectOutputToSiol`]
/// lets the uniform flow run the body and redirects only the final write.
enum SimOutcome {
    /// SIMM disabled / no simulation link configured: run the record
    /// body normally.
    NotSimulated,
    /// Simulated INPUT record: the SIOL read + convert already ran here
    /// (`readValue` precedes the body). The caller must still run the
    /// forward-link / CP / RPRO tail exactly as `recGblFwdLink` does for a
    /// real process cycle, but skips the (already-substituted) body.
    ///
    /// Carries the cycle's [`CyclePosts`] because `sim_process_tail` already
    /// published this cycle's monitors here; only this arm has a post set to
    /// report, which is why it is on the variant rather than on the tuple.
    Simulated(CyclePosts),
    /// Simulated record whose simulation replaces only the INPUT STAGE of its
    /// body ([`Record::simulation_substitutes_input_stage`](crate::server::record::Record::simulation_substitutes_input_stage)) — swait. The SIOL
    /// read, the `VAL = SVAL` / `UDF = FALSE` write and the SIMM_ALARM raise
    /// have already happened here (C `swaitRecord.c:415-422`, which precedes the
    /// OOPT switch); the caller runs the record body with its input-link fetch
    /// suppressed, then the ordinary alarm/monitor/forward-link tail — none of
    /// which C's simulation branch skips.
    SimulatedInputStage,
    /// The `default:` arm of C's `switch (prec->simm)` — a SIMM value outside
    /// the record's own menu (`SimMode::Illegal`):
    ///
    /// ```c
    /// default:
    ///     recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM);
    ///     status = -1;
    /// ```
    ///
    /// SOFT_ALARM/INVALID is already raised into the record's PENDING alarm by
    /// `check_simulation_mode`. What is left is what C's `readValue`/
    /// `writeValue` does NOT do on this arm: no device read, no device write, no
    /// SIOL round-trip, no SIMM_ALARM, no VAL/UDF change. The `-1` it returns is
    /// not a control-flow abort — the record's `process()` ignores it and still
    /// runs `checkAlarms`, `monitor` and `recGblFwdLink` — so the cycle's tail
    /// runs either way. The two record shapes differ only in where the
    /// suppressed I/O sat: an INPUT's `readValue` precedes the body (nothing of
    /// the body is left to run), an OUTPUT's `writeValue` follows it (the body
    /// runs, only the write is suppressed).
    IllegalMode { is_output: bool },
    /// The SIML read FAILED and the record's support ABORTS on it — C
    /// `writeValue` returns before performing any I/O
    /// ([`Record::aborts_on_failed_siml_read`](crate::server::record::Record::aborts_on_failed_siml_read); `busy` is the only one):
    ///
    /// ```c
    /// status=dbGetLink(&prec->siml,DBR_USHORT, &prec->simm,0,0);
    /// if (status)
    ///     return(status);      /* before write_busy AND before the SIOL dbPutLink */
    /// ```
    ///
    /// Like [`Self::IllegalMode`] with `is_output`, this suppresses the cycle's
    /// output and nothing else: the body runs and `process()` still does
    /// `checkAlarms` / `monitor` / `recGblFwdLink`. It differs in the alarm — the
    /// LINK_ALARM that `dbGetLink`'s `setLinkAlarm` already raised is the only
    /// one; no SOFT_ALARM and no SIMM_ALARM is added, because C never reaches the
    /// `switch (prec->simm)` that would raise them.
    AbortedBeforeWrite,
    /// Simulated OUTPUT record (`SIMM`=YES/RAW, not deferring). C
    /// `writeValue` substitutes the device write with
    /// `dbPutLink(&prec->siol, ..., &prec->oval)` — but at the END of
    /// `process()`, AFTER the body (OROC, bo HIGH momentary reset, OVAL).
    /// Unlike the input read, the output write cannot be done up-front, so
    /// the caller runs the uniform record body and redirects only the final
    /// output write to SIOL. Carries the SIOL link, the SIMS severity, and
    /// the RAW-mode flag (write RVAL vs OVAL).
    RedirectOutputToSiol {
        siol: crate::server::record::ParsedLink,
        sims: i16,
        raw_mode: bool,
    },
    /// Asynchronous simulation: `SIMM`=YES/RAW with `SDLY` >= 0 on the
    /// fresh (non-continuation) cycle. C `aiRecord.c::readValue` (488-508)
    /// / `aoRecord.c::writeValue` (571-587) `callbackRequestProcessCallbackDelayed`:
    /// hold PACT, schedule a re-process `SDLY` seconds out, and post nothing
    /// this cycle (C `process()` returns 0 on the async-start pass). The
    /// SIOL round-trip + alarm/monitor tail run on the continuation, which
    /// re-enters with `is_continuation = true` and takes the synchronous
    /// branch. The wrapped [`Duration`](std::time::Duration) is the `SDLY` delay.
    DeferRead(std::time::Duration),
}

/// Which link fields of a [`Record::multi_input_links`](crate::server::record::Record::multi_input_links) list are SET, read
/// once at the top of a process cycle and shared by both stages that want
/// them.
///
/// A mask, and nothing else. A `calc` declares twenty-one input links and a
/// stock database wires none of them, so any per-link entry — a text, a
/// parse, a set-link record — was a heap allocation per record per pass. The
/// parse and target of a set link live in the record's own cache
/// (`RecordInstance::parsed_inputs`) and are read from there, under the
/// record's guard, by the fetch that uses them.
pub struct InputLinkTexts {
    /// The list the mask indexes — a record's
    /// [`Record::multi_input_links`](crate::server::record::Record::multi_input_links), or the subset it selected for this pass.
    /// Carried WITH the mask, and the only list a reader is offered, so no
    /// reader can pair one list's slots with another list's bits.
    links: &'static [(&'static str, &'static str)],
    /// The record's own [`Record::multi_input_links`](crate::server::record::Record::multi_input_links) — the list the parse
    /// cache is indexed by, and what [`Self::links`] is unless the record
    /// narrowed it for this pass. Asked of the record once, here: the fetch
    /// loop and the resolved-links report take it from this value.
    own: &'static [(&'static str, &'static str)],
    /// Bit `slot` set when `links[slot]` holds a link text. Every declared
    /// list fits: the widest, `aSub`'s, has twenty-one entries.
    wired: u64,
    /// Whether the links were read at all. For a reader, a clear bit then
    /// means "unset"; without the flag it would also mean "never asked",
    /// which is the put paths below, and they must go to the record instead.
    read: bool,
}

impl InputLinkTexts {
    /// A caller that read nothing. The put paths and the async-completion path
    /// resolve link-backed metadata without running a multi-input fetch, so
    /// they have nothing to hand over.
    pub fn none() -> Self {
        Self {
            links: &[],
            own: &[],
            wired: 0,
            read: false,
        }
    }

    /// The record's own set links, as `instance` holds them now.
    pub(crate) fn read_own(instance: &RecordInstance) -> Self {
        let own = instance.record.multi_input_links();
        Self::read_from(instance, own, own)
    }

    /// The set links of `links` — [`Self::own`] narrowed to the subset the
    /// record selected for this pass — as `instance` holds them now.
    pub(crate) fn read_narrowed(
        &self,
        instance: &RecordInstance,
        links: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self::read_from(instance, self.own, links)
    }

    fn read_from(
        instance: &RecordInstance,
        own: &'static [(&'static str, &'static str)],
        links: &'static [(&'static str, &'static str)],
    ) -> Self {
        debug_assert!(links.len() <= u64::BITS as usize);
        // Which links are wired is asked of the record as ONE question —
        // `Record::set_input_link_slots`, whose default body is codegen'd per
        // record type and so reads the type's own fields inline. Walking the
        // list here instead put one vtable call per declared link in the
        // cycle: 21 for a `calc`, to learn that a stock database wires none
        // of them.
        //
        // Only for the record's OWN list. A caller that narrowed it — the
        // `sel` selected-input pass — is asking about a different list than
        // the record answered for, so it falls through to the walk.
        let masks = std::ptr::eq(links, own)
            .then(|| instance.record.set_input_link_slots())
            .flatten();
        let wired = match masks {
            Some((set, mut unknown)) => {
                let mut wired = set;
                while unknown != 0 {
                    let slot = unknown.trailing_zeros() as usize;
                    unknown &= unknown - 1;
                    if instance.link_is_set(links[slot].0) {
                        wired |= 1 << slot;
                    }
                }
                wired
            }
            None => links
                .iter()
                .enumerate()
                .filter(|(_, (link_field, _))| instance.link_is_set(link_field))
                .fold(0, |wired, (slot, _)| wired | 1 << slot),
        };
        Self {
            links,
            own,
            wired,
            read: true,
        }
    }

    /// Whether this pass read the links and found none of them set — the
    /// answer for a stock database, where a `calc`'s 21 declared inputs are
    /// all unwired. A reader that needs a set link for every one of its own
    /// entries is finished before it starts.
    pub(crate) fn none_set(&self) -> bool {
        self.read && self.wired == 0
    }

    /// The `(link_field, value_field)` pairs these texts were read from — the
    /// list to walk when fetching them.
    pub(crate) fn links(&self) -> &'static [(&'static str, &'static str)] {
        self.links
    }

    /// The record's own list — what the parse cache and the resolved-links
    /// report are indexed by, whether or not [`Self::links`] was narrowed.
    pub(crate) fn own(&self) -> &'static [(&'static str, &'static str)] {
        self.own
    }

    /// The set slots of [`Self::links`], as a mask — what a fetch walks, as
    /// C's `fetch_values` loop is over the record's links but its work is
    /// only on the set ones (`dbGetLink` on a constant link is a no-op
    /// success).
    pub(crate) fn wired(&self) -> u64 {
        self.wired
    }

    /// Whether `slot` of [`Self::links`] holds a link.
    pub(crate) fn is_set(&self, slot: usize) -> bool {
        1u64.checked_shl(slot as u32)
            .is_some_and(|bit| self.wired & bit != 0)
    }

    /// The link at `slot` of the multi-input list: what was pre-read if the
    /// cycle pre-read it, and otherwise what the record says now. `None` is
    /// an unset link on both paths — the one meaning this answers. The slot
    /// comes from [`RecordInstance::link_backed_metadata_input_slots`], fixed
    /// by the record type, so no caller searches this list by name.
    pub(crate) fn link_at(
        &self,
        slot: Option<usize>,
        instance: &RecordInstance,
        field: &str,
    ) -> Option<Arc<crate::server::record::ParsedLink>> {
        match slot.filter(|_| self.read) {
            Some(slot) => {
                debug_assert_eq!(
                    self.links.get(slot).map(|(lf, _)| *lf),
                    Some(field),
                    "a metadata slot must name the link it was taken for"
                );
                if !self.is_set(slot) {
                    return None;
                }
                instance.cached_multi_input(slot, field)
            }
            None => instance
                .link_text(field)
                .map(|text| Arc::new(crate::server::record::parse_link_v2(&text))),
        }
    }
}

/// One set link of the multi-input fetch, as the loop hands it to
/// [`PvDatabase::land_multi_input`].
struct MultiInputLink<'a> {
    link_field: &'static str,
    val_field: &'static str,
    parsed: &'a crate::server::record::ParsedLink,
    /// [`Record::input_link_request`](crate::server::record::Record::input_link_request) for the link — C's `dbrType` argument
    /// to `dbGetLink`.
    request: crate::server::record::InputLinkRequest,
    /// [`Record::input_link_failure_is_inert`](crate::server::record::Record::input_link_failure_is_inert) for the link.
    failure_is_inert: bool,
}

/// What one link's read came to, for the fetch policy after it.
struct LinkOutcome {
    read_failed: bool,
    /// Failed, and the type declared that failure inert.
    skipped: bool,
}

impl PvDatabase {
    /// The tail of one `dbGetLink` into the reader: the read converted to
    /// the record's request, the LINK alarm on failure, the value stored,
    /// the source's severity inherited — in that order, C's, and in one frame
    /// so the value is moved once.
    #[inline]
    fn land_multi_input(
        &self,
        record: &mut dyn crate::server::record::Record,
        common: &mut crate::server::record::CommonFields,
        reader: &Arc<RecordCell>,
        link: &MultiInputLink<'_>,
        plan: &crate::server::record::record_instance::ProcessPlan,
        (mut fetch, alarm): (
            crate::server::recgbl::simm::LinkFetch,
            Option<super::links::SourceAlarm>,
        ),
    ) -> LinkOutcome {
        use crate::server::recgbl::simm::LinkFetch;
        let store_raw = self.convert_link_fetch_as(
            record,
            link.link_field,
            link.parsed,
            link.request,
            &mut fetch,
        );
        let read_failed = !fetch.is_ok();
        // C `dbGetLink` on failure: `recGblSetSevrMsg(LINK_ALARM)` — for the
        // types whose fetch IS `dbGetLink`, and not for a link whose failure
        // the type declares inert.
        if read_failed && plan.multi_input_is_db_get_link && !link.failure_is_inert {
            crate::server::recgbl::rec_gbl_set_link_alarm(common, link.link_field);
        }
        let value = match fetch {
            LinkFetch::Value(v) => Some(v),
            LinkFetch::NoData if plan.constants_deliver_at_process => {
                crate::server::recgbl::simm::constant_load_value(link.parsed)
            }
            _ => None,
        };
        if let Some(value) = value {
            deliver_multi_input(record, link.val_field, value, store_raw);
        }
        // MS/NMS propagation from the source record, C `recGblInheritSevrMsg`
        // inside a successful `dbGetLink`.
        if let Some(alarm) = alarm {
            self.fold_input_link_alarm(common, reader, link.parsed, alarm);
        }
        LinkOutcome {
            read_failed,
            skipped: read_failed && link.failure_is_inert,
        }
    }

    /// C `dbGetLink` on a DB link to a held local record, read at the
    /// reader's own type with no filter chain — the whole of `dbDbGetValue`'s
    /// scalar arm (`dbDbLink.c:220-232`) in one frame: the target's field
    /// read under its lock, the reader's `LINK_ALARM` on failure, the value
    /// stored, the target's committed severity inherited. Returns whether
    /// the read failed.
    ///
    /// The general path is [`Self::read_db_link_at`] into
    /// [`Self::land_multi_input`], which answers the same questions for every
    /// caller and so packs the value and alarm into a fetch and a source
    /// alarm on the way. This frame asks nothing the general one does not;
    /// it only keeps the value where it is consumed. Which is why its
    /// callers are gated on `ProcessPlan::multi_inputs_read_native`: the
    /// conversion step it omits is a no-op for a native request, and the
    /// inert-failure test it omits is settled `false` by the same plan bit.
    /// Self-reads are excluded by the caller as C excludes them from
    /// `recGblInheritSevrMsg` (`precord != dbChannelRecord(chan)`), so the
    /// inheritance needs no record test here.
    #[inline(always)]
    fn fetch_native_db_input(
        &self,
        record: &mut dyn crate::server::record::Record,
        common: &mut crate::server::record::CommonFields,
        held: &crate::server::database::SetGuard,
        (db, at, native): (
            &crate::server::record::DbLink,
            &crate::server::record::record_instance::ResolvedTarget,
            crate::server::record::record_instance::NativeRead,
        ),
        (declared, cache_slot): (&'static [(&'static str, &'static str)], usize),
        sets_link_alarm: bool,
    ) -> bool {
        use super::links::{LinkAlarm, inherit_sevr_msg, local_name};
        let target = db.target();
        let field: &str = &target.field;
        // The simple PV that shadows the field's spelling, asked as
        // `read_field_of` asks it — and never for `VAL`, whose spelling is
        // the record's own name.
        if native.shadowed {
            let pv_name = local_name(&target.record, field);
            if let Some(pv) = self.inner.simple_pvs.lock().get(&pv_name).cloned() {
                deliver_multi_input(record, declared[cache_slot].1, pv.get(), false);
                return false;
            }
        }
        let inherits = native.inherits;
        let instance = at.rec.read_in(held);
        // A field the target hands out a slot for is read as the `f64` the
        // numeric funnel would make of it; any other goes by name and
        // through the funnel.
        let value = match at.field.slot {
            Some(slot) => instance.record.get_slot_f64(slot).map(Ok),
            None => self
                .read_field_at(&instance, field, at.field)
                .map(|value| value.into_double()),
        };
        let alarm = inherits.then(|| LinkAlarm::committed(&instance.common));
        drop(instance);
        let Some(value) = value else {
            if sets_link_alarm {
                crate::server::recgbl::rec_gbl_set_link_alarm(common, declared[cache_slot].0);
            }
            return true;
        };
        let stored = match value {
            Ok(f) => Some(f),
            Err(value) => deliver_converted(record, declared[cache_slot].1, value),
        };
        if let Some(f) = stored
            && !native
                .val_slot
                .is_some_and(|slot| record.put_slot_f64(slot, f))
        {
            let _ = record.put_multi_input_f64(declared[cache_slot].1, f);
        }
        if let Some(alarm) = alarm {
            inherit_sevr_msg(common, db.monitor_switch, &alarm);
        }
        false
    }
}

/// What one link of the multi-input fetch came to, before the reader's
/// fields were touched: unset, read under the reader's own hold, or not a
/// read that hold can make.
enum HeldFetch {
    /// The link is unset — C `dbConstGetValue` with nothing to deliver.
    Unset,
    Done(LinkOutcome),
    /// A read the general path owns: a target in no local record, the
    /// reader's own field, a `PP` source, a filtered target.
    NotHeld,
}

/// The fetch loop's fold of its links' outcomes — C `fetch_values`'
/// `status` under the record's [`InputFetchPolicy`], and the mask of links
/// that delivered (`RTN_SUCCESS(dbGetLink)` per link). One fold for both
/// shapes of the loop, so a policy is applied in one place.
#[derive(Default)]
struct FetchFold {
    resolved: u64,
    /// This cycle's `fetch_values()` outcome — non-zero status in C, i.e.
    /// "the record body must not run" — under every policy but the sel one.
    fetch_values_failed: bool,
    /// C `fetch_values`' `status` local, for the record types that return
    /// it rather than an early/first-failure fold: assigned on EVERY pass of
    /// the loop, so at `return(status)` it holds the LAST link's status and
    /// an empty/constant link counts as a success.
    last_input_read_failed: bool,
}

impl FetchFold {
    /// Note one link's outcome; `true` when the policy ends the loop here.
    fn note(
        &mut self,
        policy: InputFetchPolicy,
        cache_slot: usize,
        is_last: bool,
        LinkOutcome {
            read_failed,
            skipped,
        }: LinkOutcome,
    ) -> bool {
        self.last_input_read_failed = read_failed && !skipped && is_last;
        if !read_failed && let Some(bit) = 1u64.checked_shl(cache_slot as u32) {
            self.resolved |= bit;
        }
        if read_failed && !skipped {
            match policy {
                InputFetchPolicy::ReadAll => {}
                InputFetchPolicy::ReadAllGateOnFailure => {
                    self.fetch_values_failed = true;
                }
                InputFetchPolicy::AbortOnFirstFailure => {
                    self.fetch_values_failed = true;
                    self.last_input_read_failed = true;
                    return true;
                }
                // `sel`: the LAST link's status decides, and the loop
                // carries that decision to the gate after the loop.
                InputFetchPolicy::ReadAllGateOnLastFailure => {}
            }
        }
        false
    }

    /// C `fetch_values`' return, folded with the sel gate into the ONE
    /// boolean delivered to `Record::set_fetch_gate_failed` (calc/calcout/
    /// scalcout/acalcout/swait/sel) and `suppress_subroutine_run` (sub/aSub).
    ///
    /// C `selRecord.c::fetch_values` returns the status of its LAST
    /// `dbGetLink` (`:434-437` assigns `status` unguarded every pass), and
    /// `process` (`:114-116`) gates `do_sel` on it in EVERY mode. The gate is
    /// "the last link read FAILED" — never "a link delivered no value":
    /// `dbGetLink` on an unset OR constant link returns success
    /// (`dbConstGetValue`), and the field it would have written keeps its
    /// init-seeded value, which flows into `do_sel`.
    fn failed(&self, policy: InputFetchPolicy, sel_nvl_read_failed: bool) -> bool {
        let failed = if matches!(policy, InputFetchPolicy::ReadAllGateOnLastFailure) {
            self.last_input_read_failed
        } else {
            self.fetch_values_failed
        };
        failed || sel_nvl_read_failed
    }
}

impl PvDatabase {
    /// One `dbGetLink` of the multi-input fetch, whichever way the link
    /// reads: the held native read where the plan allows it and the link is
    /// one, else the general read. `None` for an unset link, which C's loop
    /// visits as a `dbConstGetValue` success with nothing to deliver.
    ///
    /// `always`: the two loops are its callers, and the frame it would
    /// otherwise be — the held path's arguments moved in and the outcome
    /// moved out — is what the held path exists to avoid.
    #[inline(always)]
    fn fetch_multi_input(
        &self,
        guard: &mut DataGuard<'_>,
        plan: &crate::server::record::record_instance::ProcessPlan,
        declared: &'static [(&'static str, &'static str)],
        cache_slot: usize,
        visited: &mut ProcStack,
    ) -> Option<LinkOutcome> {
        if plan.multi_inputs_read_native {
            match self.fetch_native_input_held(guard, plan, declared, cache_slot) {
                HeldFetch::Done(outcome) => return Some(outcome),
                HeldFetch::Unset => return None,
                HeldFetch::NotHeld => {}
            }
        }
        self.fetch_multi_input_general(guard, plan, declared, cache_slot, visited)
    }

    /// The common link of a wired record — a held local target read at its
    /// own type, no filter — through [`Self::fetch_native_db_input`], with
    /// the reader's guard held throughout. Decides from the cached parse and
    /// target alone, so a link it declines has cost the general path one
    /// cache hit.
    #[inline(always)]
    fn fetch_native_input_held(
        &self,
        guard: &mut DataGuard<'_>,
        plan: &crate::server::record::record_instance::ProcessPlan,
        declared: &'static [(&'static str, &'static str)],
        cache_slot: usize,
    ) -> HeldFetch {
        use crate::server::record::record_instance::ParsedInputLink;
        let rec = guard.rec;
        let (inst, held) = guard.hold_in();
        // Under THIS hold, as the parse it validates is: a link before this
        // one may have released the guard, and a put to the text in that
        // window moved the count.
        let generation = inst.record.input_links_generation();
        let Some(entry) = ParsedInputLink::validated(
            &mut inst.parsed_inputs,
            &*inst.record,
            cache_slot,
            declared,
            generation,
        ) else {
            return HeldFetch::Unset;
        };
        // C `dbGetLink`: a `ProcessPassive` DB input link processes its
        // passive source record before the value is read. The source's
        // cycle may read THIS record back through a link of its own, so it
        // runs with the guard released — as does a read of this record's
        // own field. Neither is this frame's, and the handle knows which
        // it is.
        let Some((db, at, native)) = entry.native_read(self, rec, cache_slot) else {
            return HeldFetch::NotHeld;
        };
        HeldFetch::Done(LinkOutcome {
            read_failed: self.fetch_native_db_input(
                &mut *inst.record,
                &mut inst.common,
                held,
                (db, at, native),
                (declared, cache_slot),
                plan.multi_input_is_db_get_link,
            ),
            skipped: false,
        })
    }

    /// One `dbGetLink` of the multi-input fetch in its general form: the
    /// read converted to the record's request, a `PP` source processed
    /// first, a target found by name, the reader's own field read with the
    /// guard released — every case, through [`Self::land_multi_input`].
    /// Out of the loop's line so the loop carries the common case's frame
    /// alone.
    #[inline(never)]
    fn fetch_multi_input_general(
        &self,
        guard: &mut DataGuard<'_>,
        plan: &crate::server::record::record_instance::ProcessPlan,
        declared: &'static [(&'static str, &'static str)],
        cache_slot: usize,
        visited: &mut ProcStack,
    ) -> Option<LinkOutcome> {
        use crate::server::record::record_instance::ParsedInputLink;
        use crate::server::record::{InputLinkRequest, LinkProcessPolicy, LinkReadAs, ParsedLink};
        let (link_field, val_field) = declared[cache_slot];
        let rec = guard.rec;
        let inst = guard.hold();
        let (failure_is_inert, request) = if plan.multi_inputs_read_native {
            debug_assert!(
                !inst.record.input_link_failure_is_inert(link_field)
                    && inst.record.input_link_request(link_field)
                        == InputLinkRequest::As(LinkReadAs::Native),
                "{}: input_link_answers_fixed_at_type must be false for a \
                 per-instance input_link_request / input_link_failure_is_inert",
                inst.record.record_type()
            );
            (false, InputLinkRequest::As(LinkReadAs::Native))
        } else {
            (
                inst.record.input_link_failure_is_inert(link_field),
                inst.record.input_link_request(link_field),
            )
        };
        let generation = inst.record.input_links_generation();
        let entry = ParsedInputLink::validated(
            &mut inst.parsed_inputs,
            &*inst.record,
            cache_slot,
            declared,
            generation,
        )?;
        let (parsed, target) = entry.target(self, rec, cache_slot);
        // C `dbGetLink`: a `ProcessPassive` DB input link processes its
        // passive source record before the value is read. The source's cycle
        // may read THIS record back through a link of its own, so it runs
        // with the guard released — as does a read of this record's own
        // field, and a read that has to find its target by name.
        let held_target = match (target, parsed) {
            (Some(at), ParsedLink::Db(db))
                if !Arc::ptr_eq(&at.rec, rec) && db.policy != LinkProcessPolicy::ProcessPassive =>
            {
                Some((db, at))
            }
            _ => None,
        };
        // One landing site, so the read is written once into the frame it
        // is consumed from; the arms differ only in whether the parse and
        // the target are borrowed from the cache (the guard held, so nothing
        // can replace them) or cloned out to survive the release.
        let owned;
        let owned_target;
        let (record, common, parsed, read) = if let Some((db, at)) = held_target {
            let read = self.read_db_link_at(db, at);
            (&mut *inst.record, &mut inst.common, parsed, read)
        } else {
            owned_target = target.cloned();
            owned = entry.parsed().clone();
            guard.release();
            if let ParsedLink::Db(db) = &*owned {
                self.process_passive_db_source(db, visited);
            }
            let read = self.read_link_with_alarm_at(&owned, owned_target.as_ref());
            let inst = guard.hold();
            (&mut *inst.record, &mut inst.common, &*owned, read)
        };
        let link = MultiInputLink {
            link_field,
            val_field,
            parsed,
            request,
            failure_is_inert,
        };
        Some(self.land_multi_input(record, common, rec, &link, plan, read))
    }

    /// The input stage of a cycle that reads nothing but the record's own
    /// input links, each at its own type — see the shape test in
    /// [`Self::fetch_input_stage`]. The multi-input loop alone, over the
    /// record's own list, under the guard the caller holds; returns the
    /// resolved mask, the one thing the body wants of such a cycle.
    fn fetch_own_native_inputs(
        &self,
        guard: &mut DataGuard<'_>,
        plan: &crate::server::record::record_instance::ProcessPlan,
        link_texts: &InputLinkTexts,
        visited: &mut ProcStack,
    ) -> u64 {
        let declared = link_texts.own();
        let policy = plan.input_fetch_policy;
        let mut fold = FetchFold::default();
        // Over the set links only: C's loop visits every declared link, but
        // an unset one is a `dbConstGetValue` success with nothing to
        // deliver, so the passes it would make here are no-ops.
        let mut wired = link_texts.wired();
        while wired != 0 {
            let slot = wired.trailing_zeros() as usize;
            wired &= wired - 1;
            let Some(outcome) = self.fetch_multi_input(guard, plan, declared, slot, visited) else {
                continue;
            };
            if fold.note(policy, slot, slot + 1 == declared.len(), outcome) {
                break;
            }
        }
        let fetch_values_failed = fold.failed(policy, false);
        let inst = guard.hold();
        inst.record.set_fetch_gate_failed(fetch_values_failed);
        if fetch_values_failed {
            inst.suppress_subroutine_run = true;
        }
        fold.resolved
    }
}

/// One `fetch_values` result into its value field — C's `dbGetLink(plink,
/// DBR_DOUBLE, &prec->a, ...)` store, for the types that funnel through the
/// numeric put and the ones that take the value as read.
#[inline]
fn deliver_multi_input(
    record: &mut dyn crate::server::record::Record,
    val_field: &'static str,
    value: EpicsValue,
    store_raw: bool,
) {
    if store_raw {
        // A string-class declared request (printf `%s`) already produced the
        // value the record asked for — the numeric funnel below is the OTHER
        // records' `DBR_DOUBLE` request, not a store rule.
        let _ = record.put_field_internal(val_field, value);
        return;
    }
    // What a numeric source's `DBR_DOUBLE` fetch is; everything else
    // converts in its own frame.
    let f = match value.into_double() {
        Ok(f) => f,
        Err(value) => match deliver_converted(record, val_field, value) {
            Some(f) => f,
            None => return,
        },
    };
    let _ = record.put_multi_input_f64(val_field, f);
}

/// [`deliver_multi_input`]'s non-`Double` arm: an array stored whole when
/// the field takes one, else the scalar the numeric funnel makes of it.
fn deliver_converted(
    record: &mut dyn crate::server::record::Record,
    val_field: &'static str,
    value: EpicsValue,
) -> Option<f64> {
    if value.is_array() {
        if record.put_field_internal(val_field, value.clone()).is_ok() {
            return None;
        }
        // The target is a scalar field: element 0, as C's one-element
        // destination takes.
        return value.first_element().and_then(|v| v.get_convert_f64());
    }
    value.get_convert_f64()
}

/// The record's data guard across the guarded segments of one process cycle.
///
/// C holds `dbScanLock` for the whole of `dbProcess`. The port's segments each
/// re-took the lock because the work between them — link reads, link writes,
/// device output, forward-link and CP dispatch — may lock another record, or
/// this one again through a cyclic link, and so must run unlocked. That work
/// exists on a minority of cycles. One rule at every boundary: release only
/// across a boundary that performs such work, decided from state the previous
/// segment read under the guard; otherwise the next segment continues under
/// the guard the previous one held.
/// What a process frame is asked to run: a name still to be looked up, or a
/// scan-list entry already resolved to its canonical name and cell.
enum ProcessTarget<'a> {
    Name(&'a str),
    Resolved(&'a str, Arc<RecordCell>),
}

/// The frame's record name: borrowed from the caller's scan snapshot when the
/// entry came resolved, shared out of the registry when the frame looked it
/// up. Either way no name is copied per cycle.
enum FrameName<'a> {
    Borrowed(&'a str),
    Shared(Arc<str>),
}

impl std::ops::Deref for FrameName<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            FrameName::Borrowed(s) => s,
            FrameName::Shared(s) => s,
        }
    }
}

struct DataGuard<'a> {
    rec: &'a Arc<RecordCell>,
    held: Option<crate::server::record::RecordMut<'a>>,
}

impl<'a> DataGuard<'a> {
    fn new(rec: &'a Arc<RecordCell>) -> Self {
        Self { rec, held: None }
    }

    /// The instance under the guard — taken now if the last boundary released it.
    ///
    /// `always`, as is [`Self::hold_in`]: the body asks a dozen times per
    /// cycle and the answer is a loaded pointer whenever the guard is held.
    /// Left to the inliner, one more caller of [`RecordCell::write`] flipped
    /// it out of line, at 150 instructions of frame per cycle.
    #[inline(always)]
    fn hold(&mut self) -> &mut RecordInstance {
        let rec = self.rec;
        self.held.get_or_insert_with(|| rec.write())
    }

    /// [`Self::hold`] with the set guard alongside, for the link reads of
    /// the same set ([`RecordCell::read_in`]).
    #[inline(always)]
    fn hold_in(&mut self) -> (&mut RecordInstance, &crate::server::database::SetGuard) {
        let rec = self.rec;
        self.held.get_or_insert_with(|| rec.write()).split()
    }

    /// Give the guard up ahead of work that may lock another record, or this one.
    fn release(&mut self) {
        self.held = None;
    }
}

/// What the input stage hands the rest of the cycle. See
/// [`PvDatabase::fetch_input_stage`].
struct InputStage {
    is_soft: bool,
    /// The multi-input links whose fetch produced a value, one bit per slot
    /// of the record's own `multi_input_links` — C's `RTN_SUCCESS(dbGetLink)`
    /// per link, recorded at no cost and delivered as the bits it is
    /// ([`crate::server::record::ResolvedInputLinks`]), so every type's
    /// report is made.
    resolved: u64,
    /// What the link reads produced. `None` is the cycle that had nothing to
    /// read — a stock `calc` — and costs that cycle one tag, where a struct
    /// of empty results cost it every field's write and drop.
    links: Option<LinkInputs>,
}

/// The per-link results of one input stage, present only for a cycle that
/// read at least one link.
struct LinkInputs {
    inp_value: Option<EpicsValue>,
    inp_source_time: Option<std::time::SystemTime>,
    inp_source_utag: Option<u64>,
    inp_link_remote_time: Option<(i64, i32, u64)>,
    dol_info: Option<(crate::server::record::ParsedLink, i16)>,
    dol_fetch: Option<crate::server::recgbl::simm::LinkFetch>,
    dol_read_failed: bool,
    sel_nvl_value: Option<EpicsValue>,
    string_input_values: Vec<(String, EpicsValue)>,
    asub_dynamic: Option<AsubDynamicSub>,
    resolved_link_fields: Vec<&'static str>,
    link_alarms: Vec<(
        crate::server::record::MonitorSwitch,
        super::links::LinkAlarm,
    )>,
}

impl InputStage {
    /// The stage's result for a cycle that had nothing to read: what the
    /// fetch produces when every link it would ask is unset.
    fn none(is_soft: bool, resolved: u64) -> Self {
        Self {
            is_soft,
            resolved,
            links: None,
        }
    }
}

impl LinkInputs {
    /// No link read anything — the shape a later stage fills in when it has a
    /// result of its own to record (the pre-process `ReadDbLink` reads).
    fn none() -> Self {
        Self {
            inp_value: None,
            inp_source_time: None,
            inp_source_utag: None,
            inp_link_remote_time: None,
            dol_info: None,
            dol_fetch: None,
            dol_read_failed: false,
            sel_nvl_value: None,
            string_input_values: Vec::new(),
            asub_dynamic: None,
            resolved_link_fields: Vec::new(),
            link_alarms: Vec::new(),
        }
    }
}

impl PvDatabase {
    /// Process a record by name (process_local + notify).
    /// Alias-aware (epics-base PR #336).
    pub async fn process_record(&self, name: &str) -> CaResult<()> {
        // Delegate to the canonical engine path so a direct process fetches
        // input links (DOL/INPx), runs the record body, evaluates alarms,
        // writes outputs and dispatches FLNK exactly as a C `dbProcess` does.
        // The reduced `process_local` path this used to call fetched no links,
        // so a direct process of a calc/sub/aSub used stale A..U inputs; that
        // path now exists only as an internal record-body unit-test helper.
        // Acquires the entry record's advisory write gate (foreign caller).
        let mut visited = ProcStack::new();
        self.process_record_with_links(name, &mut visited).await
    }

    /// `process_record` variant for a caller that already
    /// owns the record's advisory write gate — the QSRV atomic group
    /// PUT applying a `+proc` member. The gate is not
    /// reentrant; the atomic group path MUST use this entry. See
    /// [`crate::server::database::PvDatabase::lock_records`].
    pub async fn process_record_already_locked(&self, name: &str) -> CaResult<()> {
        // Same delegation as [`Self::process_record`], but to the gate-held
        // engine entry since the caller already owns the advisory write gate.
        let mut visited = ProcStack::new();
        self.process_record_with_links_already_locked(name, &mut visited)
    }

    /// Process a record with full link handling (INP -> process -> alarms -> OUT -> FLNK).
    /// Uses the visited set for cycle detection.
    ///
    /// Foreign-caller entry: FLNK dispatch, scan loop, scan_event, CA put,
    /// process(PROC=1) etc. Hits the PACT entry guard (mirrors C `dbProcess`
    /// at `dbAccess.c:537-559`) when the record is mid-async.
    ///
    /// this is a *foreign* full-processing entry, so it acquires
    /// the record's advisory write gate (`dbScanLock` analogue) for the
    /// entry record before processing. A QSRV atomic group or pvalink
    /// atomic scan-on-update epoch that holds `lock_records` over the
    /// same record blocks a foreign scan/event/FLNK-dispatch caller
    /// here, and vice versa — restoring the `DBManyLock` exclusion. The
    /// recursive FLNK / OUT / CP fan-out within one chain does NOT
    /// re-acquire the gate (`process_record_with_links_recursive`),
    /// mirroring C `processTarget` (`dbDbLink.c:436`) which asserts the
    /// target's lock set is already owned by the calling thread; the
    /// `visited` cycle guard prevents re-processing the entry record.
    pub fn process_record_with_links<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut ProcStack,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move { self.process_record_with_links_sync(name, visited) })
    }

    /// The same frame as [`Self::process_record_with_links`], called directly.
    ///
    /// `run_process_frame` and everything under it is synchronous — the H6
    /// contract the body's doc states — so the future the entry above hands
    /// back resolves without ever yielding, and its `Box::pin` is an
    /// allocation per record per scan cycle for nothing. A sweep already runs
    /// on a thread it is allowed to occupy, so it takes the frame directly.
    pub(crate) fn process_record_with_links_sync(
        &self,
        name: &str,
        visited: &mut ProcStack,
    ) -> CaResult<()> {
        self.run_process_frame(ProcessTarget::Name(name), visited, true, false, false)
    }

    /// [`Self::process_record_with_links_sync`] for a caller that already
    /// holds the instance — a scan sweep, whose list carries the handle beside
    /// the name it is walking.
    pub(crate) fn process_record_with_links_resolved(
        &self,
        name: &str,
        rec: Arc<RecordCell>,
        visited: &mut ProcStack,
    ) -> CaResult<()> {
        self.run_process_frame(
            ProcessTarget::Resolved(name, rec),
            visited,
            true,
            false,
            false,
        )
    }

    /// Driver-callback (`asyn:READBACK`) full-processing entry.
    ///
    /// The single owner of this entry is the I/O Intr wiring
    /// (`crate::server::ioc_app::setup_io_intr` and its `ioc_builder`
    /// twin): the spawned task processes a record because the driver
    /// fired an interrupt callback, not because of a client put / FLNK /
    /// scan. `device_callback = true` tells
    /// `Self::process_record_with_links_inner` that, for an *output*
    /// record, this cycle must READ the callback value back into VAL and
    /// MUST NOT write it to the driver — C `devAsynInt32.c::processBo`
    /// (and `processAo`/`processLongout`/…) take the readback branch when
    /// `newOutputCallbackValue` is set, never `processCallbackOutput`'s
    /// `write()`. Without this, the readback re-asserts the setpoint and
    /// re-triggers the driver (e.g. AD `Acquire` looping). Input records
    /// (`!can_device_write`) are unaffected: their read stage already
    /// runs, and the no-write gate is keyed on the record being an output.
    ///
    /// Acquires the entry record's advisory write gate exactly like
    /// [`Self::process_record_with_links`] — the callback task is a
    /// foreign caller w.r.t. any QSRV atomic group / pvalink epoch.
    pub fn process_record_readback<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut ProcStack,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            // C `devAsynInt32.c::outputCallbackCallback` (asyn devEpics):
            // arm the output-callback "expected pop" before dbProcess, then
            // reconcile after. If this pass never reaches the device read
            // stage — the PACT entry guard bails because a put / FLNK cycle
            // still owns the record (e.g. the readback racing the bo's own
            // put that started the driver) — the callback ring would keep the
            // entry forever and desync the wakeup count from the pop count.
            // The AD `Acquire` bo getting stuck at 1 after a fast acquire is
            // exactly that: the start callback's readback bails on PACT, the
            // finalize callback's pop then consumes the stale start value, and
            // the finalize 0 is never popped. reconcile discards the stale
            // entry (C fallback `getCallbackValue`) so 1 callback == 1 pop.
            self.arm_readback_callback(name);
            let result = self
                .process_record_with_links_inner(name, visited, false, true, true)
                .await;
            self.reconcile_readback_callback(name);
            result
        })
    }

    /// Arm the entry record's output driver-callback cycle before a readback
    /// process pass — see [`crate::server::device_support::DeviceSupport::arm_readback_callback`].
    fn arm_readback_callback(&self, name: &str) {
        let canonical = self.resolve_alias(name);
        let key: &str = canonical.as_deref().unwrap_or(name);
        // Collect-then-act: clone the instance handle under a brief map read,
        // then drop the map lock before taking the per-record write. Never
        // hold `records.read()` across `rec.write()` — same lock discipline
        // as `add_breaktables` / `all_record_names`.
        let rec = {
            let records = self.inner.records.read();
            records.get(key).cloned()
        };
        if let Some(rec) = rec {
            if let Some(dev) = rec.write().device.as_mut() {
                dev.arm_readback_callback();
            }
        }
    }

    /// Reconcile the entry record's output driver-callback cycle after a
    /// readback process pass — see
    /// [`crate::server::device_support::DeviceSupport::reconcile_readback_callback`].
    fn reconcile_readback_callback(&self, name: &str) {
        let canonical = self.resolve_alias(name);
        let key: &str = canonical.as_deref().unwrap_or(name);
        // Collect-then-act: clone the handle under a brief map read, drop the
        // map lock, then take the per-record write — see `arm_readback_callback`.
        let rec = {
            let records = self.inner.records.read();
            records.get(key).cloned()
        };
        if let Some(rec) = rec {
            if let Some(dev) = rec.write().device.as_mut() {
                dev.reconcile_readback_callback();
            }
        }
    }

    /// full-processing entry for a caller that already owns the
    /// record's advisory write gate via [`PvDatabase::lock_records`] —
    /// the QSRV atomic group GET/PUT and the pvalink atomic
    /// scan-on-update epoch. The advisory gate is not
    /// reentrant; a transaction owner holding `lock_records` over the
    /// member set MUST use this entry to scan a member record, or it
    /// would deadlock against its own epoch guard. Foreign (non-owner)
    /// callers must use [`Self::process_record_with_links`] so the gate
    /// is taken.
    ///
    /// Synchronous: the gate is already held by the caller, so this entry has
    /// nothing to wait for. It goes straight to
    /// `process_record_with_links_body`, which is where the H6
    /// no-suspension contract lives.
    pub fn process_record_with_links_already_locked(
        &self,
        name: &str,
        visited: &mut ProcStack,
    ) -> CaResult<()> {
        self.run_process_frame(ProcessTarget::Name(name), visited, false, false, false)
    }

    /// One record's process frame: entry bookkeeping, the optional advisory
    /// write gate, the cycle, and the unwind that takes this frame's cycle
    /// marker back out of `visited`.
    ///
    /// **Invariant:** a name is in `visited` exactly while its frame is on the
    /// CURRENT PROCESS STACK — never "somewhere earlier in this cascade".
    /// Both of C's equivalents are stack conditions and nothing else:
    /// `processTarget` claims `procThread` at `dbDbLink.c:502-504` and clears
    /// it at `:521-526`, around one `dbProcess`; `dbProcess` itself tests
    /// `precord->pact` (`dbAccess.c:537`), set for the duration of a cycle.
    /// There is no set of already-processed records anywhere in C, and
    /// `dbProcess(pdst)` at `dbDbLink.c:511` is unconditional.
    ///
    /// **Owner/gate:** this function. [`Self::process_entry_prelude`]
    /// returning `Some` means THIS frame inserted the name, and this is the
    /// only place that takes it out again. A `Some` returning through any
    /// other path would leave a marker outliving the stack it describes, and
    /// the guard would start refusing records C processes again — which is
    /// exactly what a diamond FLNK (`F` → `A`,`B`; `A` → `C`; `B` → `C`) hit.
    fn run_process_frame(
        &self,
        target: ProcessTarget<'_>,
        visited: &mut ProcStack,
        acquire_gate: bool,
        is_continuation: bool,
        device_callback: bool,
    ) -> CaResult<()> {
        // A `None` here found the name already present, so the marker is the
        // outer frame's and there is nothing to unwind.
        let Some((name, rec)) = self.process_entry_prelude(target, visited)? else {
            return Ok(());
        };

        // advisory write gate (`dbScanLock(precord)` analogue).
        // A foreign full-processing entry (scan loop, scan_event, FLNK
        // dispatch from another chain, CA put, PINI/startup) acquires
        // the entry record's gate so it cannot interleave with a QSRV
        // atomic group or a pvalink atomic scan epoch holding
        // `lock_records` over the same record. `name` is already the
        // alias-resolved canonical name, the same key `lock_records`
        // uses. Not acquired when `acquire_gate` is false: either a
        // transaction owner already holds the gate via `lock_records`
        // (`process_record_with_links_already_locked`), or this is a
        // recursive FLNK/OUT/CP call within one chain
        // (`process_record_with_links_recursive`) — C `processTarget`
        // processes a link target under the lock set the caller already
        // owns, and re-acquiring would deadlock the non-reentrant gate.
        let _record_gate = if acquire_gate {
            Some(self.lock_instance(&rec))
        } else {
            None
        };

        // Breakpoint hook, C `dbAccess.c:504-515`:
        //
        //     if (lset_stack_count != 0) {
        //         if (dbBkpt(precord)) goto all_done;
        //     }
        //
        // guarding both the hook and its "skip record support" answer. Here
        // the guard is the `ArcSwapOption` load: `None` for a database nobody
        // is debugging, so this costs one relaxed atomic per processed record
        // where C costs one comparison.
        //
        // Under the gate, as C's `dbProcess` runs under `dbScanLock`: the
        // hook reads the record and orders the lock set before the
        // breakpoint stack, the one order every debugger path uses. A stop
        // parks the calling thread with the set given up — C drops
        // `dbScanLock` before `epicsThreadSuspendSelf` (`dbBkpt.c:794-796`)
        // — so `dbb`/`dbd`/`dbc`/`dbs` keep working and the set's other
        // records keep processing while one is stopped. The thread that
        // parks is never a runtime worker: the hook hands foreign processing
        // to the lock set's own continuation thread and returns `Skip`, and
        // only that thread reaches the parking arm.
        let breakpoints = self.breakpoints_if_debugging();
        if let Some(table) = breakpoints.as_ref() {
            if table.before_process(self, &name)
                == crate::server::database::breakpoint::Before::Skip
            {
                // C's `goto all_done`, which unwinds the same way the normal
                // path does. `visited` was inserted by the prelude above and
                // this frame owns it, so it comes out here as it would below.
                visited.release(&rec);
                return Ok(());
            }
        }

        // NO `.await` may appear below this line while `_record_gate` is
        // live — see the module note on `process_record_with_links_body`.
        let result = self.process_record_with_links_body(
            &name,
            &rec,
            visited,
            is_continuation,
            device_callback,
        );

        // Breakpoint auto-print, C `dbAccess.c:614-616` — after record
        // support, under the same `lset_stack_count` guard. Reloaded rather
        // than reusing the handle above: a `dbd` during this record's own
        // processing can have retired the observer, and C re-tests the count.
        if let Some(table) = self.breakpoints_if_debugging() {
            table.after_process(self, &name);
        }

        // The unwind. C `dbDbLink.c:521-526`, `if (claim_dst)
        // dbRec2Pvt(pdst)->procThread = NULL;` — after `dbProcess`, whatever
        // it returned.
        visited.release(&rec);
        result
    }

    /// One entry point processed by a breakpoint continuation thread — C
    /// `dbBkptCont`'s `dbScanLock(precord); dbProcess(pqe->entrypoint);
    /// dbScanUnlock(precord);` (`dbBkpt.c:604-606`).
    ///
    /// The gate is acquired here, as for any other foreign entry, and the
    /// chain below may park inside the breakpoint hook. That is legal on this
    /// call and on no other: the caller is the lock set's dedicated thread,
    /// which exists to be parked, never a runtime worker.
    pub(crate) fn process_record_for_breakpoint(&self, name: &str) -> CaResult<()> {
        self.run_process_frame(
            ProcessTarget::Name(name),
            &mut ProcStack::new(),
            true,
            false,
            false,
        )
    }

    /// recursive FLNK / OUT / CP fan-out entry within a single
    /// processing chain. Does NOT re-acquire the advisory write gate:
    /// the chain is one transaction whose entry record's gate is
    /// already held by the foreign entry, and C `processTarget`
    /// (`dbDbLink.c:436`) processes a link target under the lock set
    /// already owned by the calling thread. Re-acquiring per chain
    /// member would also create a lock-ordering deadlock between
    /// reverse FLNK chains.
    ///
    /// Synchronous, and recursive as a plain call: the chain runs inside the
    /// entry record's gate-held region, so it must not suspend. C's
    /// `processTarget` is likewise a direct call under the caller's lock set.
    pub(crate) fn process_record_with_links_recursive(
        &self,
        name: &str,
        visited: &mut ProcStack,
    ) -> CaResult<()> {
        self.run_process_frame(ProcessTarget::Name(name), visited, false, false, false)
    }

    /// Owner-driven continuation re-entry — bypasses the PACT entry guard.
    ///
    /// Used by `ProcessAction::ReprocessAfter` timer fires: the spawned
    /// re-entry task IS the owner of the async cycle, equivalent to C
    /// `callbackRequestDelayed`'s direct call to the record's `process()`
    /// (which bypasses `dbProcess`). Foreign callers must still go through
    /// `process_record_with_links` so FLNK / scan / CA put cannot race
    /// during the wait window.
    ///
    /// the timer fire is a fresh task — the original cycle's
    /// advisory gate was released when `process_record_with_links`
    /// returned async-pending. In C, `callbackRequestDelayed` dispatches
    /// through a callback that re-takes `dbScanLock(precord)` for the
    /// completion `process()`. This entry therefore re-acquires the
    /// advisory write gate, so the continuation cannot interleave with a
    /// QSRV atomic group or another foreign scan of the same record.
    pub fn process_record_continuation<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut ProcStack,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.process_record_with_links_inner(name, visited, true, true, false)
                .await
        })
    }

    /// A cycle-free [`AsyncDbHandle`] for this database, handed to each
    /// record via [`crate::server::record::Record::set_async_context`] at
    /// registration. Holds only a `Weak` reference, so a record stashing
    /// it never keeps the database alive.
    pub fn async_handle(&self) -> AsyncDbHandle {
        AsyncDbHandle {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Mint a fresh async re-entry [`AsyncToken`] for `name`.
    ///
    /// Minting advances the record's generation counter, so any
    /// previously-minted token for the same record is superseded — its
    /// [`AsyncToken::fire`] becomes a structural no-op. This mirrors C
    /// `callbackRequestDelayed` replacing an outstanding delayed callback
    /// for a record. `name` must be the canonical record name (the value
    /// of `RecordInstance::name`). Returns `None` if the record is absent.
    pub fn mint_async_token(&self, name: &str) -> Option<AsyncToken> {
        let rec = self.get_record_no_resolve(name)?;
        let generation = rec.read().reprocess_generation.clone();
        let epoch = generation.fetch_add(1, Ordering::AcqRel) + 1;
        Some(AsyncToken {
            name: name.to_string(),
            generation,
            epoch,
        })
    }

    /// Cancel any outstanding async re-entry token for `name` (C
    /// `callbackCancelDelayed`): advance the record's generation counter so
    /// every previously-minted [`AsyncToken`] for it becomes stale and its
    /// `fire` is a no-op. A subsequent [`Self::mint_async_token`] produces a
    /// fresh, current token. No-op if the record is absent.
    pub fn cancel_async_reentry(&self, name: &str) {
        if let Some(rec) = self.get_record_no_resolve(name) {
            rec.read()
                .reprocess_generation
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The callback band `name`'s `PRIO` selects — C
    /// `callbackSetPriority(prec->prio, &pcb->callback)` (`seqRecord.c:146`).
    ///
    /// For the deferral sites that hold a record *name* rather than a locked
    /// instance. Takes the record's read lock, so it must not be called from
    /// inside that record's own `process()`/`special()` — those run under the
    /// instance write lock and read the band off
    /// [`ProcessContext::callback_priority`](crate::server::record::ProcessContext)
    /// instead. A record that is gone answers `Low`, the band an unwritten
    /// `PRIO` already has; the work being scheduled for it is a no-op anyway.
    pub fn record_callback_priority(&self, name: &str) -> crate::runtime::task::CallbackPriority {
        match self.get_record_no_resolve(name) {
            Some(rec) => rec.read().common.callback_priority(),
            None => crate::runtime::task::CallbackPriority::Low,
        }
    }

    /// Schedule a delayed re-process of `name` — the single owner of the
    /// "mint a fresh [`AsyncToken`], sleep, then fire" pattern. Used by both
    /// [`ProcessAction::ReprocessAfter`](crate::server::record::ProcessAction::ReprocessAfter) (record-driven owner re-entry: ODLY
    /// output delay, swait, sequence DLYn) and the `SDLY` async-simulation
    /// defer ([`SimOutcome::DeferRead`]). Minting advances the record's
    /// generation so a newer schedule supersedes any pending one; a stale
    /// token's `fire` is a structural no-op. No-op if the record is absent.
    fn schedule_delayed_reprocess(&self, name: &str, delay: std::time::Duration) {
        let token = match self.mint_async_token(name) {
            Some(t) => t,
            None => return,
        };
        let prio = self.record_callback_priority(name);
        let db = self.clone();
        crate::runtime::task::spawn_background(prio, async move {
            crate::runtime::task::sleep_background(delay).await;
            let _ = token.fire(&db).await;
        });
    }

    /// Schedule C `callbackRequestDelayed` with a record-owned handler body —
    /// the single owner of [`ProcessAction::DelayedCallbackAfter`](crate::server::record::ProcessAction::DelayedCallbackAfter)
    /// and the port of `boRecord.c::myCallbackFunc` (:105-118).
    ///
    /// The fire takes the record gate (C `dbScanLock`), runs
    /// [`Record::delayed_callback_fire`](crate::server::record::Record::delayed_callback_fire)
    /// and only then re-enters `process()`. The handler's mutation is therefore
    /// reachable from the timer alone: no record flag survives the arm, so no
    /// other process cycle can consume the one-shot. Re-arming mints a fresh
    /// token, exactly as C's re-`callbackRequestDelayed` replaces the pending
    /// delayed callback.
    fn schedule_delayed_callback(&self, name: &str, delay: std::time::Duration) {
        let Some(token) = self.mint_async_token(name) else {
            return;
        };
        let prio = self.record_callback_priority(name);
        let db = self.clone();
        let name = name.to_string();
        crate::runtime::task::spawn_background(prio, async move {
            let mut token = token;
            let mut delay = delay;
            loop {
                crate::runtime::task::sleep_background(delay).await;
                // A newer arm (or a cancel) superseded this timer while it
                // slept — the same `AsyncToken` gate `ReprocessAfter` uses.
                if !token.is_current() {
                    return;
                }
                let outcome = {
                    let records = db.inner.records.read();
                    let Some(rec) = records.get(name.as_str()) else {
                        return;
                    };
                    let rec = rec.clone();
                    drop(records);
                    let mut instance = rec.write();
                    let pact = instance.is_processing();
                    instance.record.delayed_callback_fire(pact)
                };
                match outcome {
                    crate::server::record::DelayedCallbackOutcome::Reprocess => {
                        let _ = token.fire(&db).await;
                        return;
                    }
                    crate::server::record::DelayedCallbackOutcome::Rearm(again) => {
                        let Some(fresh) = db.mint_async_token(&name) else {
                            return;
                        };
                        token = fresh;
                        delay = again;
                    }
                    crate::server::record::DelayedCallbackOutcome::Drop => return,
                }
            }
        });
    }

    /// (Re)arm a record's monitor watchdog — the single owner of the
    /// [`Record::watchdog_interval`](crate::server::record::Record::watchdog_interval) / [`Record::watchdog_fire`](crate::server::record::Record::watchdog_fire) tick, and the
    /// port of C `histogramRecord.c::wdogInit` + `wdogCallback` (:102-152).
    ///
    /// Called from exactly two places, C's own two `wdogInit` call sites: once
    /// per record at `iocInit` (C `init_record` pass 1, `:168`) and from
    /// [`ProcessAction::ArmWatchdog`](crate::server::record::ProcessAction::ArmWatchdog), which a record's `special()` emits when
    /// a put changed the period (histogram SDEL, `:266-268`).
    ///
    /// Arming bumps the record's `watchdog_generation`, so a tick already in
    /// flight is superseded and simply exits — C's `callbackRequestDelayed`
    /// replacing an outstanding delayed callback. The task re-reads the
    /// interval on every iteration, so an SDEL put to 0 stops the watchdog at
    /// its next fire without a separate cancel path.
    ///
    /// The tick is NOT a process cycle: it takes the record lock (C
    /// `dbScanLock`), lets the record perform its own state change, stamps the
    /// record (C `recGblGetTimeStamp`) and posts `DBE_VALUE | DBE_LOG` monitors
    /// for the fields the record named — no `add_count`, no alarm tail, no
    /// FLNK. A record with no watchdog (`watchdog_interval() == None`) spawns
    /// nothing.
    pub(crate) fn arm_watchdog(&self, name: &str) {
        let (rec, generation, epoch, prio) = {
            let Some(rec) = self.get_record_no_resolve(name) else {
                return;
            };
            let instance = rec.read();
            if instance.record.watchdog_interval().is_none() {
                // Bumping the generation still cancels a watchdog left running
                // by an earlier arm — an SDEL put to 0 comes through here.
                instance
                    .watchdog_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                return;
            }
            let generation = instance.watchdog_generation.clone();
            let epoch = generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            let prio = instance.common.callback_priority();
            drop(instance);
            (rec, generation, epoch, prio)
        };

        let is_soft = {
            let instance = rec.read();
            instance.device.is_none()
        };
        // C `histogramRecord.c::wdogCallback` stamps with `recGblGetTimeStamp`
        // (`:113`), TSEL and all, so the tick owes the TSEL read too. Weak, so
        // a live watchdog never keeps the database alive — the tick simply
        // stamps without TSEL if the database is already gone.
        let db = self.async_handle();
        crate::runtime::task::spawn_background(prio, async move {
            loop {
                let interval = {
                    let instance = rec.read();
                    match instance.record.watchdog_interval() {
                        Some(d) => d,
                        // C: `if (prec->sdel > 0)` fails -> no re-arm.
                        None => return,
                    }
                };
                crate::runtime::task::sleep_background(interval).await;
                // A newer arm superseded this task while it slept.
                if generation.load(std::sync::atomic::Ordering::Acquire) != epoch {
                    return;
                }
                let fields = {
                    let mut instance = rec.write();
                    instance.record.watchdog_fire()
                };
                if fields.is_empty() {
                    // C `wdogCallback`: `mcnt == 0` -> no stamp, no post; the
                    // timer still re-arms. C tests `prec->mcnt` before it even
                    // takes `dbScanLock` (`histogramRecord.c:111-112`), so no
                    // TSEL read happens on an empty tick either.
                    continue;
                }
                // Between the two guards, like every other stamp point: the
                // TSEL read takes its own locks.
                let tsel = match db.db() {
                    Some(db) => db.read_tsel(&rec),
                    None => super::TselStamp::None,
                };
                let mut instance = rec.write();
                let inst = &mut *instance;
                tsel.stamp(&inst.name, &mut inst.common, is_soft);
                for field in fields {
                    instance.notify_field(
                        field,
                        crate::server::recgbl::EventMask::VALUE
                            | crate::server::recgbl::EventMask::LOG,
                    );
                }
            }
        });
    }

    /// Post an async-side field update for `name` — the C `db_post_events`
    /// analogue called from device-support / async-callback context.
    ///
    /// Each `(field, value)` is written through the internal put (bypassing
    /// the read-only field gate, like a record's own `process()` writes)
    /// and a monitor event is posted with `DBE_VALUE | DBE_LOG` — the mask C
    /// device support uses for an out-of-process value post
    /// (`db_post_events(precord, &prec->field, DBE_VALUE | DBE_LOG)`).
    /// Metadata-class writes invalidate the metadata cache via
    /// `notify_field_written`, honouring the snapshot-cache contract.
    ///
    /// Unlike [`Self::complete_async_record`], this runs *no* alarm /
    /// timestamp / FLNK tail: it is the immediate "push these fields to
    /// monitors now" primitive (e.g. asyn TRACE info, motor intermediate
    /// readback) that is independent of any process cycle. Returns the
    /// field names actually posted, or [`CaError::ChannelNotFound`] if the
    /// record is absent.
    pub fn post_fields(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
    ) -> CaResult<Vec<String>> {
        self.post_fields_with_mask(
            name,
            fields,
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
        )
    }

    /// Out-of-band PROPERTY-class post — the C
    /// `db_post_events(precord, &precord->val, DBE_PROPERTY)` analogue used
    /// for enum-string table re-propagation (asyn `callbackEnum`,
    /// devAsynInt32.c:712-766). Stores [`crate::server::device_support::PropertyPost::writes`] through the
    /// internal put, invalidates the metadata cache, and posts a single
    /// `DBE_PROPERTY` event on [`crate::server::device_support::PropertyPost::post_field`] so subscribers
    /// re-read enum choices / control metadata.
    ///
    /// The written fields are NOT posted on: C's `setEnums` re-keys
    /// ZRST/ZRVL/ZRSV… silently and the one `db_post_events` names
    /// `&pr->val`. See [`crate::server::device_support::PropertyPost`] for why the two sets are separate.
    ///
    /// Unlike [`Self::post_fields`] (which posts `DBE_VALUE | DBE_LOG`) this
    /// signals a *property* change, not a value change: a driver that re-keys
    /// its enum strings has not produced a new reading, only new choice
    /// labels. Returns the field names actually written.
    pub fn post_property(
        &self,
        name: &str,
        post: crate::server::device_support::PropertyPost,
    ) -> CaResult<Vec<String>> {
        let rec = {
            let records = self.inner.records.read();
            records.get(name).cloned()
        };
        let rec = rec.ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?;
        let link_backing = self.resolve_link_backed_metadata_for_posts(&rec);
        let link_backing = link_backing.as_link_backing();
        let mut inst = rec.write();
        let mut written = Vec::with_capacity(post.writes.len());
        for (field, value) in post.writes {
            inst.record.put_field_internal(&field, value)?;
            // Snapshot-cache contract: a metadata-class write must invalidate
            // the cache before the monitor snapshot below is built, or the
            // property event would carry the pre-change enum choices.
            inst.notify_field_written(&field);
            written.push(field);
        }
        inst.notify_field_backed(
            &post.post_field,
            crate::server::recgbl::EventMask::PROPERTY,
            link_backing,
        );
        Ok(written)
    }

    /// Shared body of [`Self::post_fields`] and the record-owned posters:
    /// write+notify each field under one record-write lock, posting `mask`.
    ///
    /// Reachable from the records module because a record's own
    /// `db_post_events` mask is the record's to choose — see
    /// [`crate::server::records::link_status::post_link_status`], where three
    /// records post `DBE_VALUE` and a fourth posts `DBE_VALUE|DBE_LOG`.
    pub(crate) fn post_fields_with_mask(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
        mask: crate::server::recgbl::EventMask,
    ) -> CaResult<Vec<String>> {
        let rec = {
            let records = self.inner.records.read();
            records.get(name).cloned()
        };
        let rec = rec.ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?;
        // A link-backed field reaches this poster: `seq` posts `DOn` here
        // (`links.rs`, C `seqRecord.c:266-268`) and `DOn`'s metadata comes
        // from `DOLn`. Resolved before the write guard, as everywhere.
        let link_backing = self.resolve_link_backed_metadata_for_posts(&rec);
        let link_backing = link_backing.as_link_backing();
        let mut inst = rec.write();
        let mut posted = Vec::with_capacity(fields.len());
        for (field, value) in fields {
            inst.record.put_field_internal(&field, value)?;
            // Snapshot-cache contract: a metadata-class write must
            // invalidate the cache before the monitor snapshot is built.
            inst.notify_field_written(&field);
            inst.notify_field_backed(&field, mask, link_backing);
            posted.push(field);
        }
        Ok(posted)
    }

    /// The single owner of [`crate::server::record::ProcessOutcome::post_write_fields`]: apply the
    /// field stores a `process()` withheld until its queued link writes had
    /// run, and post each at `DBE_VALUE`.
    ///
    /// Called on every arm that leaves a process cycle, immediately after that
    /// arm has executed the cycle's [`crate::server::record::ProcessAction::WriteDbLink`] and before
    /// its snapshot notification — which is where C's single `dbScanLock`
    /// makes the clear visible (`sseqRecord.c::asyncFinish` after
    /// `processCallback`'s `dbPutLink`s; `scalerRecord.c:370` under the same
    /// lock as `:457`/`:463`). A reader that takes the record between
    /// `process()` returning and this call sees the flag still SET, which is
    /// the conservative half of C's two observable states.
    ///
    /// Each field is applied independently. The group is one transition and a
    /// field that fails to store must not strand the rest of it — a partial
    /// apply that abandoned `BUSY` would leave the record busy forever.
    ///
    /// `DBE_VALUE` alone. C's masks, measured: `asyncFinish` (`sseqRecord.c:461`)
    /// posts `abort` at `:481`, `aborting` at `:482` and `busy` at `:505`, all
    /// three with `MonitorMask` — `DBE_VALUE | recGblResetAlarms(pR)` (`:471`),
    /// i.e. `DBE_VALUE` plus the alarm bit only when the alarm changed. Bare
    /// `DBE_VALUE` is what the other posts use: `waiting` (`:343`, `:559`,
    /// `:728`, `:1185` — never inside `asyncFinish`), the second `aborting`
    /// post (`:1192`), and `scalerRecord.c:372` (`cnt`). So a member published
    /// in the same cycle as an alarm transition omits a `DBE_ALARM` C sets.
    pub(crate) fn publish_post_write_fields(
        &self,
        name: &str,
        fields: crate::server::record::CycleList<(String, EpicsValue)>,
    ) {
        if fields.is_empty() {
            return;
        }
        let Some(rec) = self.get_record(name) else {
            return;
        };
        let mut inst = rec.write();
        for (field, value) in fields {
            if let Err(e) = inst.record.put_field_internal(&field, value) {
                eprintln!("{name}.{field}: post-write publication failed: {e:?}");
                continue;
            }
            // Snapshot-cache contract, as `post_fields_with_mask`: invalidate
            // before the monitor snapshot is built.
            inst.notify_field_written(&field);
            inst.notify_field(&field, crate::server::recgbl::EventMask::VALUE);
        }
    }

    /// Resolve a link's target field [`DbFieldType`] for a LOCAL `DB_LINK`,
    /// or `None` for a constant / external / unresolvable link.
    ///
    /// Parity of C `dbGetLinkDBFtype` as `sseqRecord.c:checkLinks`
    /// (sseqRecord.c:884-941) uses it to fill the `DTn`/`LTn` diagnostics:
    /// a `DB_LINK` whose target record is on this IOC reports its addressed
    /// field's type (C `dbNameToAddr` → `pAddr->field_type`). A constant or
    /// `CA`/`PVA` (external) link returns `None` — epics-base-rs has no
    /// client-side introspection of a remote field's type, so the caller
    /// renders those as the `DBF_unknown` sentinel.
    pub(crate) fn link_target_field_type(&self, link: &str) -> Option<crate::types::DbFieldType> {
        let db = match crate::server::record::parse_link_v2(link) {
            crate::server::record::ParsedLink::Db(db) => db,
            _ => return None,
        };
        // Through the split: a filtered link's raw halves name a record that
        // does not exist (`SRC.VAL[0]` whole, field `VAL`), so `get_record`
        // missed and every filtered DB link reported no type at all.
        let addressed = db.target();
        let rec = self.get_record(&addressed.record)?;
        let inst = rec.read();
        let field = if addressed.field.is_empty() {
            "VAL"
        } else {
            addressed.field.as_str()
        };
        crate::server::record::record_instance::declared_field_type_of(inst.record.as_ref(), field)
    }

    /// Create a put-notify wait-set for a downstream operation a record is
    /// about to drive, returning the wait-set (to attach to the downstream
    /// target instance's `notify`) and the completion receiver.
    ///
    /// C `dbNotify.c` `processNotify`: the set arms `pending = 1` for the
    /// downstream operation and fires the oneshot when that slot (plus any
    /// FLNK/OUT chain members that `enter` it) drains to zero — i.e. on
    /// `dbNotifyCompletion`. Pair with [`Self::reprocess_on_notify`] to
    /// re-enter a waiting record when the downstream completes (SSEQ
    /// `WAITn`).
    pub fn new_put_notify() -> (
        Arc<NotifyWaitSet>,
        crate::runtime::sync::oneshot::Receiver<()>,
    ) {
        let (tx, rx) = crate::runtime::sync::oneshot::channel();
        (NotifyWaitSet::new(tx), rx)
    }

    /// Wire a downstream put-notify completion to an async re-entry: spawn a
    /// task that awaits `completion` (the oneshot from
    /// [`Self::new_put_notify`], fired on `dbNotifyCompletion`) and then
    /// `token.fire`s, re-entering the waiting record's `process()`. A
    /// superseded / cancelled token re-enters nothing. Returns the spawned
    /// task handle; fire-and-forget callers may drop it.
    pub fn reprocess_on_notify(
        &self,
        token: AsyncToken,
        completion: crate::runtime::sync::oneshot::Receiver<()>,
    ) -> crate::runtime::task::BackgroundTaskHandle<()> {
        let prio = self.record_callback_priority(token.record_name());
        let db = self.clone();
        crate::runtime::task::spawn_background(prio, async move {
            // `Err` means the sender was dropped without firing (the
            // downstream op vanished); treat it the same as completion so a
            // waiting record is never stranded — `fire` is a no-op if the
            // token was meanwhile superseded.
            let _ = completion.await;
            let _ = token.fire(&db).await;
        })
    }

    /// Issue a put-WITH-completion to an OUT link and hand the caller only
    /// the completion receiver — the non-blocking sibling of
    /// [`Self::reprocess_on_notify`].
    ///
    /// Each call mints its own put-notify wait-set (C `dbProcessNotify`),
    /// writes the link through it with the source record's committed PUTF /
    /// alarm propagated (C `recGblInheritSevrMsg`), releases the initiator
    /// count, and returns the oneshot that fires on `dbNotifyCompletion`.
    /// The caller owns when (and whether) to await each receiver, so several
    /// puts can be outstanding at once — unlike
    /// [`crate::server::record::ProcessAction::WriteDbLinkNotify`], which wires the completion
    /// straight to a single superseding async re-entry token and so allows
    /// only one outstanding put per record. This is the seam C
    /// `calcApp/src/sseqRecord.c` needs to run multiple `WAITn` put-callbacks
    /// concurrently in flight (`processNextLink`).
    ///
    /// `record_name` is the source whose PUTF/alarm propagate into the
    /// target, `link_str` the already-resolved OUT link spelling, `value`
    /// the value to write. `None` if the source record is gone; an empty
    /// `link_str` returns a receiver that fires immediately (nothing joined
    /// the set).
    pub async fn put_link_notify(
        &self,
        record_name: &str,
        link_field: &str,
        link_str: &str,
        value: EpicsValue,
    ) -> Option<crate::runtime::sync::oneshot::Receiver<()>> {
        let rec = {
            let records = self.inner.records.read();
            records.get(record_name)?.clone()
        };
        let (src_putf, src_alarm) = {
            let instance = rec.read();
            // sseq's WAITn puts run from its async machine while the record
            // is still PACT — C `sseqRecord.c` issues `dbPutLink` in
            // `processCallback` (:734/756/787) and commits the alarm only in
            // `asyncFinish` (`recGblResetAlarms`, :471). The put therefore
            // inherits the source's PENDING alarm.
            (
                instance.common.putf,
                super::links::LinkAlarm::pending(&instance.common),
            )
        };
        let (waitset, completion) = Self::new_put_notify();
        if !link_str.is_empty() {
            let parsed = crate::server::record::parse_output_link_v2(link_str);
            // Seed the cycle-guard with the source so a target linking back
            // does not re-process it, exactly as a top-level OUT-link write
            // does (`process_record_with_links_inner` inserts its own name).
            let mut visited = ProcStack::new();
            visited.claim(&rec);
            // Through the put owner: C `dbPutLinkAsync` raises the source's
            // LINK_ALARM/INVALID on a failed put exactly as the synchronous
            // `dbPutLink` does (dbLink.c:469-471).
            self.write_out_link_value(
                &rec,
                &parsed,
                value,
                super::links::OutLinkSrc {
                    putf: src_putf,
                    notify: Some(&waitset),
                    alarm: &src_alarm,
                    field: link_field,
                },
                &mut visited,
            );
        }
        // Release the initiator's own count (C `dbProcessNotify` holds one
        // count for the requester and drops it after issuing the put). The
        // set then drains — firing `completion` — when the downstream
        // target(s) that joined via `join_put_notify` finish, or immediately
        // when the link was empty / the target completed synchronously.
        waitset.leave();
        Some(completion)
    }

    /// aSub LFLG=READ: read the subroutine name from the SUBL link and, when
    /// it changed, re-resolve the function from the registry. C
    /// `aSubRecord.c::fetch_values`. Returns `None` for any record that is
    /// not an aSub in READ mode (the common case), so the caller pays only a
    /// single brief read lock. Run BEFORE the process write lock so the SUBL
    /// link read cannot deadlock against this record.
    fn resolve_asub_dynamic_subroutine(&self, rec: &Arc<RecordCell>) -> Option<AsubDynamicSub> {
        let (subl, onam, snam) = {
            let inst = rec.read();
            if inst.record.record_type() != "aSub" {
                return None;
            }
            // LFLG: IGNORE=0 (static, resolved at init), READ=1 (dynamic).
            let lflg = inst
                .record
                .get_field("LFLG")
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0) as i16;
            if lflg != 1 {
                return None;
            }
            let read_str = |f: &str| match inst.record.get_field(f) {
                Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
                _ => String::new(),
            };
            (read_str("SUBL"), read_str("ONAM"), read_str("SNAM"))
        };

        // C `aSubRecord.c:256`: `dbGetLink(&prec->subl, DBR_STRING,
        // prec->snam, 0, 0)` — a plain read into SNAM. A CONSTANT (or unset)
        // SUBL delivers NOTHING here, so SNAM keeps the name
        // `recGblInitConstantLink(&subl, DBF_STRING, prec->snam)`
        // (`aSubRecord.c:126`) loaded at init — which is also what a `caput
        // REC.SNAM other` leaves in place.
        use crate::server::recgbl::simm::LinkFetch;
        let name: Option<String> =
            match self.db_get_link(rec, "SUBL", &crate::server::record::parse_link_v2(&subl)) {
                LinkFetch::Value(v) => Some(match v {
                    EpicsValue::String(s) => s.as_str_lossy().into_owned(),
                    o => o.to_f64().map(|f| f.to_string()).unwrap_or_default(),
                }),
                LinkFetch::NoData => Some(snam),
                LinkFetch::Failed => None,
            };

        let Some(name) = name else {
            // Link read failed — C `if (status) return status` skips do_sub.
            return Some(AsubDynamicSub {
                snam: None,
                swap: None,
                skip_run: true,
            });
        };

        // Re-resolve only when the name changed (C `strcmp(snam, onam)`); an
        // empty name never resolves (do_sub's `snam[0]==0` short-circuit).
        if !name.is_empty() && name != onam {
            match self.find_subroutine_named(&name) {
                Some(f) => Some(AsubDynamicSub {
                    snam: Some(name),
                    swap: Some(f),
                    skip_run: false,
                }),
                // Name changed but not registered — C returns S_db_BadSub,
                // skipping do_sub; ONAM is left unchanged so it retries.
                None => Some(AsubDynamicSub {
                    snam: Some(name),
                    swap: None,
                    skip_run: true,
                }),
            }
        } else {
            Some(AsubDynamicSub {
                snam: Some(name),
                swap: None,
                skip_run: false,
            })
        }
    }

    /// The entry bookkeeping every process entry shares, before the advisory
    /// write gate is (or is not) taken: alias normalisation, the `visited`
    /// cycle guard and the records-map lookup.
    ///
    /// Factored out so the gate-taking entry
    /// ([`Self::process_record_with_links_inner`]) and the two gate-free
    /// entries (`process_record_with_links_body`'s direct callers)
    /// run it in the SAME order relative to the gate: bail decisions are made
    /// before any waiting, exactly as they were when this was open-coded.
    ///
    /// `Ok(None)` is "this entry did not run"; `Err` is C's `S_db_notFound`.
    ///
    /// A non-run is not silent: the cycle guard goes through
    /// [`Self::count_refused_active_entry`], which is C's already-active arm,
    /// so it cannot be written as a bare `return Ok(None)` here. There is no
    /// other non-run — C's `dbProcess` (`dbAccess.c:485`) has no link-depth
    /// counter, and neither does the port.
    ///
    /// Every `Ok(None)` is built by [`Self::entry_did_not_run`], which is
    /// also where the put-notify wait-set is released, so a non-run cannot
    /// strand a CA `WRITE_NOTIFY`.
    fn process_entry_prelude<'a>(
        &self,
        target: ProcessTarget<'a>,
        visited: &mut ProcStack,
    ) -> CaResult<Option<(FrameName<'a>, Arc<RecordCell>)>> {
        // Normalise to the canonical record name once at entry — both
        // for cycle-detection (`visited` would otherwise treat alias
        // and canonical as distinct entries) and for the records-map
        // lookup below. Mirrors epics-base PR #336.
        //
        // This is the chain's ONE name resolution: the `Arc` is the records
        // map's own key, and every hop below — the cycle guard, the lock set,
        // the body — is handed a share of it rather than a copy.
        // A sweep walking a scan list already holds the instance the list
        // names, so it hands it over rather than paying this resolution again
        // per record per cycle. The records map stays the authority on whether
        // the record is still IN the database: `remove_record` destroys the
        // instance as it takes the key out of the bucket, so a handle that
        // outlived its key answers `is_destroyed`; the body checks that under
        // the data lock it takes anyway and reports the record missing.
        let (name, rec) = match target {
            ProcessTarget::Resolved(name, rec) => (FrameName::Borrowed(name), rec),
            ProcessTarget::Name(name) => match self.lookup_record(name) {
                Some((name, rec)) => (FrameName::Shared(name), rec),
                // Nothing is registered under the name — C `S_db_notFound`,
                // which C reaches in `dbNameToAddr` before `dbProcess` is
                // called at all. Answered BEFORE the marker goes in: this
                // frame is not going to run, and a marker left here is one
                // the caller's unwind never reaches. An alias whose target
                // has gone has always reported the TARGET as missing, so
                // resolve it for the message — off the hot path, where the
                // answer is an error anyway.
                None => {
                    let name = match self.resolve_alias(name) {
                        Some(target) => target,
                        None => name.to_string(),
                    };
                    return Err(CaError::ChannelNotFound(name));
                }
            },
        };

        if !visited.claim(&rec) {
            // The name is already on the CURRENT STACK, so this is a genuine
            // cycle. C reaches the same decision through PACT: `processTarget`
            // forces `psrc->pact = TRUE` before it calls `dbProcess(pdst)`
            // (`dbDbLink.c:457`/`:512` at R7.0.10), and a record whose own
            // cycle is on the stack has had PACT set by its record support
            // anyway, so `dbProcess` takes its already-active arm
            // (`dbAccess.c:536-556`). That arm is NOT silent: it counts the
            // refused entry in LCNT and, past `MAX_LOCK`, raises
            // SCAN_ALARM/INVALID "Async in progress". The port sets PACT only
            // for an async defer, so this marker is the synchronous half of
            // C's `precord->pact` — and it owes the same arm.
            //
            // The marker belongs to the OUTER frame — only that frame may
            // remove it, which is why this path must not.
            //
            // Re-reaching a record that has already FINISHED elsewhere in the
            // cascade is a different thing entirely and does NOT arrive here:
            // its frame took its marker back out on unwind, so the diamond
            // processes twice exactly as C's unconditional
            // `dbProcess(pdst)` (`dbDbLink.c:512`) does.
            self.count_refused_active_entry(&rec);
            return self.entry_did_not_run(Some(&rec));
        }

        Ok(Some((name, rec)))
    }

    /// C `dbProcess`'s already-active arm (`dbAccess.c:536-556` at R7.0.10)
    /// — the ONE place LCNT moves and the ONE place "Async in progress" is
    /// raised.
    ///
    /// C needs one test for "active" because its record support sets
    /// `precord->pact = TRUE` at the top of every `process()`, so PACT covers
    /// both the async wait and a cycle that is merely on the stack. The port
    /// sets PACT only for an async defer ([`RecordInstance::enter_pact`]), so
    /// "active" is two tests here: [`RecordInstance::is_processing`] for the
    /// async half, and the `visited` marker in
    /// [`Self::process_entry_prelude`] for the synchronous half. Two tests,
    /// one arm — both call this, so neither can decline more quietly than C.
    fn count_refused_active_entry(&self, rec: &Arc<RecordCell>) {
        const MAX_LOCK: i16 = 10;
        let mut instance = rec.write();

        // C `dbAccess.c:539-541` — when TPRO is set on a record whose PACT is
        // true, print the diagnostic line before the bail decision. The C path
        // emits "%s: dbProcess of Active '%s' with RPRO=%d", mirroring the
        // context format the regular trace path uses (thread/client name +
        // record name + current RPRO bit). Without this, an operator debugging
        // a stuck async record sees NO sign that the entry guard is firing —
        // they only notice the eventual SCAN_ALARM after MAX_LOCK=10 attempts.
        if instance.common.tpro != 0 {
            eprintln!(
                "[TPRO] {}: dbProcess of Active '{}' with RPRO={}",
                instance.name, instance.name, instance.common.rpro,
            );
        }

        // C `dbAccess.c:544-546`:
        //   if ((precord->stat == SCAN_ALARM) ||
        //       (precord->lcnt++ < MAX_LOCK) ||
        //       (precord->sevr >= INVALID_ALARM)) goto all_done;
        // The increment is in the test, so it happens on every refusal.
        let already_invalid = instance.common.sevr >= crate::server::record::AlarmSeverity::Invalid;
        let already_scan_alarm =
            instance.common.stat == crate::server::recgbl::alarm_status::SCAN_ALARM;
        let lcnt_before = instance.common.lcnt;
        instance.common.lcnt = lcnt_before.saturating_add(1);
        if already_scan_alarm || lcnt_before < MAX_LOCK || already_invalid {
            return;
        }

        let snapshot = scan_alarm_refusal(&mut instance, "Async in progress");
        drop(instance);
        if let Some(snapshot) = snapshot {
            // Between the write guard's drop and the read guard's take: the one
            // window where a link target's lock is reachable. The refusal posts
            // STAT/SEVR/VAL, none of which any type link-backs, but the resolve
            // is the record's own answer rather than this caller's claim about
            // it — see `RecordInstance::make_monitor_snapshot`.
            let backing = self.resolve_link_backed_metadata_for_posts(rec);
            let backing = backing.as_link_backing();
            let inst = rec.read();
            inst.notify_from_snapshot(&snapshot, backing);
        }
    }

    /// The prelude's ONE "this entry did not run its cycle" exit — C
    /// `dbProcess`'s `all_done` with `callNotifyCompletion = TRUE`.
    ///
    /// `join_put_notify` (C `dbNotifyAdd`) is called by the link dispatcher
    /// on the will-process branch, *before* the recursion enters the prelude:
    ///
    /// ```text
    /// links.rs:1561   let pact = tg.is_processing();
    /// links.rs:1562   if !pact { tg.common.putf = src_putf;
    /// links.rs:1564              tg.join_put_notify(src_notify); }   // ws.enter()
    /// links.rs:1575   self.process_record_with_links_recursive(target, visited)
    /// ```
    ///
    /// So by the time the cycle guard decides the entry will not run, the
    /// target is already counted in the wait-set — and nothing
    /// downstream will ever `leave` for it, because the only `leave`s are on
    /// paths that ran a cycle. The set never drains, the completion oneshot
    /// never fires, and the client's `CA_PROTO_WRITE_NOTIFY` gets no reply
    /// (measured on x86_64-wrs-vxworks while the port still refused entries
    /// past a 16-hop depth bound: the first put into a longer chain never
    /// replied over 90s, and `RTEMS:E8:L16` was left
    /// holding a wait-set that could never drain — after which every later put
    /// completed, because `join_put_notify`'s `notify.is_none()` guard stops a
    /// record that already holds a stale set from joining a live one).
    ///
    /// C decides this per exit path with one flag and one finalizer
    /// (`dbAccess.c:494` `callNotifyCompletion = FALSE`, `:576` disabled,
    /// `:598` no RSET, `:619-622` `all_done`), and the pact branch
    /// (`:551-555`) deliberately does NOT set it: a record whose own cycle is
    /// running owns its completion. The same split holds here — hence the
    /// `is_processing` test, which is C's `if (precord->pact)`, not a guard
    /// bolted on.
    fn entry_did_not_run<'a>(
        &self,
        rec: Option<&Arc<RecordCell>>,
    ) -> CaResult<Option<(FrameName<'a>, Arc<RecordCell>)>> {
        if let Some(rec) = rec {
            let notify = {
                let mut instance = rec.write();
                if instance.is_processing() {
                    None
                } else {
                    instance.notify.take()
                }
            };
            // `leave` fires the completion oneshot when it empties the set, so
            // it runs outside the record lock — same as the SDIS-disable bail.
            if let Some(ws) = notify {
                ws.leave();
            }
        }
        Ok(None)
    }

    /// The gate-taking entry — the ONLY `.await` in the whole H6 chain.
    ///
    /// Everything after the guard is bound lives in
    /// `process_record_with_links_body`, which is a plain `fn`: the
    /// L1 gate-held region contains zero suspension points by construction,
    /// which is what C's `dbProcess` gives for free (`dbScanLock` is a
    /// blocking mutex and the whole cycle between lock and unlock is
    /// straight-line C).
    async fn process_record_with_links_inner(
        &self,
        name: &str,
        visited: &mut ProcStack,
        is_continuation: bool,
        acquire_gate: bool,
        // This cycle is driven by a driver interrupt callback
        // (`asyn:READBACK` / SCAN="I/O Intr" output), not a put/FLNK/scan.
        // For an output record it forces the read-back-no-write contract
        // (C `devAsynInt32.c::processBo` `newOutputCallbackValue` branch).
        // Always `false` for client/FLNK/scan entries.
        device_callback: bool,
    ) -> CaResult<()> {
        self.run_process_frame(
            ProcessTarget::Name(name),
            visited,
            acquire_gate,
            is_continuation,
            device_callback,
        )
    }

    /// C `dbGetTimeStampTag` (`dbLink.c:420-432`) — the single owner of "read
    /// a link's source timestamp", dispatched to the target's lset.
    /// `dbDbGetTimeStampTag` (`dbDbLink.c`) copies the source record's `time`
    /// and `utag`; the CA lset answers from its cached monitor and the CA wire
    /// carries no userTag, so it contributes 0.
    ///
    /// The tag is always returned; C's callers differ only in whether they ask
    /// for it. `recGbl.c:317` passes `&prec->utag`, while every `std/dev` soft
    /// input dset reaches this through the `dbGetTimeStamp` macro
    /// (`dbLink.c:415-418`), which passes NULL — so those callers DROP the tag,
    /// and this port drops it at the same call sites C does.
    ///
    /// `None` is C's non-zero return (`S_db_noLSET`, or an unresolvable
    /// target). A `pvalink` is deliberately absent: pvxs gates its lset's
    /// timestamp behind the link's own `time=true` option, which reaches the
    /// record through [`Self::external_link_time`] instead.
    fn db_get_time_stamp_tag(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<(std::time::SystemTime, u64)> {
        match link {
            crate::server::record::ParsedLink::Db(l) => {
                self.record_time_stamp_tag(&l.target().record)
            }
            crate::server::record::ParsedLink::Ca(ca) => self
                .external_link_time(&format!("ca://{}", ca.pv))
                .map(ext_time_pair),
            // `lnkCalc_getTimestampTag` (`lnkCalc.c:749-762`) answers from
            // `clink->time`/`clink->utag`, and the only thing that ever fills
            // those is `lnkCalc_getValue`/`lnkCalc_putValue` reading the
            // `time:"X"` input through `dbGetTimeStampTag` on that child link
            // (`:571-576`, `:651-656`). A calc link's timestamp is therefore
            // its time-input's, resolved by the same locality rule as any
            // other link — which is why this recurses into the owner instead
            // of re-deriving it. `tinp < 0` (no `time` key) is C's `return
            // -1` at `:761`.
            //
            // C caches the pair on the link at read time and answers later
            // reads from that cache; this port holds no per-link state, so it
            // resolves the source live. The two differ only when the source
            // is restamped between the calc read and the timestamp fetch —
            // microseconds apart inside one `process_record_with_links_body`.
            crate::server::record::ParsedLink::Calc(calc) => {
                let idx = (calc.time_source? as u8 - b'A') as usize;
                let arg = calc.args.get(idx)?;
                // `args[i]` names a record only when it is a link; a numeric
                // literal has no timestamp to adopt, and C's `readLocked`
                // runs it against a zeroed child link, leaving `clink->time`
                // at its `calloc` zero (`lnkCalc.c:571-575`). The `.FIELD`
                // suffix is stripped because the timestamp belongs to the
                // RECORD either way, as `dbDbGetTimeStampTag`
                // (`dbDbLink.c:362-370`) reads `dbChannelRecord(chan)->time`
                // and not the addressed field's.
                let record = Self::calc_time_source_record(arg)?;
                self.record_time_stamp_tag(&record)
            }
            _ => None,
        }
    }

    /// The locality half of [`Self::db_get_time_stamp_tag`], shared by every
    /// link class that names a record: `dbInitLink` (`dbLink.c:115-130`)
    /// makes a DB-style link naming a record this IOC does not hold a CA
    /// link, so its timestamp comes from the CA lset's cached monitor and
    /// carries no userTag.
    fn record_time_stamp_tag(&self, record: &str) -> Option<(std::time::SystemTime, u64)> {
        match self.link_target(record) {
            super::links::LinkTarget::Local(src) => {
                let g = src.read();
                Some((g.common.time, g.common.utag))
            }
            super::links::LinkTarget::LocalNotRecord => None,
            super::links::LinkTarget::External => self
                .external_link_time(&format!("ca://{record}"))
                .map(ext_time_pair),
        }
    }

    /// C `recGblGetTimeStampSimm`'s TSEL half (`recGbl.c:315-323`): read the
    /// record's `TSEL` link as the `.TIME` form (`TIME`/`UTAG`) or as a `TSE`
    /// source (every other form).
    ///
    /// Reads only — the store and the `TSE`→`TIME` lookup that follows it in C
    /// are `TselStamp::stamp`, which cannot be reached without the value this
    /// returns. Call it at the record's stamp point, not at the head of the
    /// cycle: C reads `TSEL` inside `recGblGetTimeStamp`, so a `.TIME` TSEL
    /// sees whatever the cycle has already done to its source — `calcRecord.c`
    /// runs `fetch_values` (`:120`) before the stamp (`:127`), so an `INPn PP`
    /// that reprocessed the TSEL source moves the stamp this record adopts.
    /// The link read takes its own locks (a failed `dbGetLink` writes
    /// `LINK_ALARM` into this record), so it must not run under the caller's
    /// data guard — which is the whole reason C's single function is two here.
    fn read_tsel(&self, rec: &Arc<RecordCell>) -> super::TselStamp {
        match Self::tsel_link(&rec.read()) {
            None => super::TselStamp::None,
            Some(link) => self.read_tsel_link(rec, link),
        }
    }

    /// The TSEL link this cycle has to read — `None` for a constant TSEL,
    /// decided under whatever guard the caller already holds. C `recGbl.c:315`
    /// wraps the whole TSEL read in `if (!dbLinkIsConstant(plink))`: a constant
    /// or unset TSEL is skipped outright and TSE keeps its own value.
    fn tsel_link(instance: &RecordInstance) -> Option<crate::server::record::ParsedLink> {
        (!crate::server::recgbl::simm::is_constant(&instance.parsed_tsel))
            .then(|| instance.parsed_tsel.clone())
    }

    /// The link half of [`Self::read_tsel`]. Takes other records' locks, so
    /// the caller's data guard must be released first.
    fn read_tsel_link(
        &self,
        rec: &Arc<RecordCell>,
        tsel_link: crate::server::record::ParsedLink,
    ) -> super::TselStamp {
        // A TSEL link pointing at a `.TIME` field copies that record's
        // timestamp+utag into `time`/`utag`, and the TSE→TIME half does not
        // run at all — C returns before it, leaving TSE alone.
        // C `TSEL_modified`
        // (dbLink.c:71-87) sets `DBLINK_FLAG_TSELisTIME` for ANY
        // `PV_LINK` tsel whose pvname contains `.TIME`, set BEFORE the
        // DB-vs-CA decision (dbLink.c:118) — so a local-DB link AND a
        // CA link both qualify. `recGblGetTimeStampSimm`
        // (recGbl.c:316-321) then copies the link's time+utag via
        // `dbGetTimeStampTag` and RETURNS, never loading TSE from the
        // value (even when the read fails). A pva link is a
        // `JSON_LINK` and returns early from `dbInitLink`
        // (dbLink.c:107) before `TSEL_modified`, so C never flags it;
        // pva TSEL `.TIME` is intentionally excluded here.
        //
        // The field comes from the SPLIT, not from the link's raw halves: C
        // truncates the pvname at `.TIME` (`strstr` then `*pfieldname = 0`,
        // dbLink.c:81-85), so `TSEL="SRC.TIME[0]"` is flagged TSELisTIME and
        // the filter is discarded with the rest of the tail. The raw halves
        // leave that link as record `SRC.TIME[0]` with field `VAL`, which is
        // neither `.TIME` nor a record — the flag was never set and the
        // record stamped itself.
        let tsel_is_time = match &tsel_link {
            crate::server::record::ParsedLink::Db(link) => {
                link.target().field.eq_ignore_ascii_case("TIME")
            }
            crate::server::record::ParsedLink::Ca(ca) => ca_tsel_time_record(&ca.pv).is_some(),
            _ => false,
        };
        if tsel_is_time {
            // C `dbGetTimeStampTag(plink, &prec->time, &prec->utag)`
            // (recGbl.c:317) copies BOTH the link's time AND utag —
            // through the owner, which returns the pair as one
            // consistent snapshot of the source.
            //
            // `TSEL_modified` strips `.TIME` from the pvname BEFORE the
            // DB-vs-CA decision (dbLink.c:115-118), so the link the
            // owner reads is the one addressing the source RECORD, not
            // its `.TIME` field.
            let src_time = match &tsel_link {
                crate::server::record::ParsedLink::Db(_) => self.db_get_time_stamp_tag(&tsel_link),
                crate::server::record::ParsedLink::Ca(ca) => match ca_tsel_time_record(&ca.pv) {
                    Some(rec_name) => self.db_get_time_stamp_tag(
                        &crate::server::record::ParsedLink::Ca(crate::server::record::CaLink {
                            pv: rec_name.to_string(),
                            ..ca.clone()
                        }),
                    ),
                    None => None,
                },
                _ => None,
            };
            // C returns after the TSELisTIME branch even when the read
            // fails (recGbl.c:317-320): keep the record's current time
            // rather than falling through to load TSE from the value.
            match src_time {
                Some((src_time, src_utag)) => super::TselStamp::Time(src_time, src_utag),
                None => super::TselStamp::None,
            }
        } else if let Some(val) = self.db_get_link(rec, "TSEL", &tsel_link).value() {
            // Non-`.TIME` TSEL: C `dbGetLink(&tsel, DBR_SHORT,
            // &prec->tse)` loads TSE from the link regardless of its
            // type. The pre-fix port only read a `ParsedLink::Db`
            // TSEL, ignoring a CA/PVA TSE source — and then over-corrected
            // by handing back a CONSTANT TSEL's text every cycle, which C
            // never does: `recGblGetTimeStampSimm` (`recGbl.c:315`) is
            // wrapped in `if (!dbLinkIsConstant(plink))`, so a constant
            // TSEL is skipped outright and TSE keeps its own value. Through the
            // coercion owner: the conversion routine is C's, chosen by the
            // SOURCE type (see the DISA read above).
            super::TselStamp::Tse(val.to_dbf_i16().unwrap_or(0))
        } else {
            super::TselStamp::None
        }
    }

    /// C `recGblGetTimeStamp` (`recGbl.c:305-308`) in full — the TSEL read
    /// followed by the TSE→TIME event lookup, for a soft record.
    ///
    /// The pair is spelled out at each stamp point that has its own data guard
    /// open; this is the entry for the callers that do not — `seq`, whose C
    /// `process` calls `recGblGetTimeStamp` once per link group
    /// (`seqRecord.c:261`).
    pub(crate) fn rec_gbl_get_time_stamp(&self, rec: &Arc<RecordCell>) {
        let tsel = self.read_tsel(rec);
        let mut instance = rec.write();
        let inst = &mut *instance;
        tsel.stamp(&inst.name, &mut inst.common, /* is_soft */ true);
    }

    /// The text every link in [`Record::multi_input_links`](crate::server::record::Record::multi_input_links) held when this
    /// process cycle started.
    ///
    /// One read serves both consumers. A by-name field read is a linear search
    /// of the record type's declared names — around ninety on a calc — and the
    /// cycle asked for the same twelve `INPA`..`INPL` twice: once at the top,
    /// to resolve link-backed metadata for the monitor posters, and again in
    /// the multi-input fetch. Reading once also removes the window in which
    /// the two answers could disagree, since neither read holds the record
    /// across the cycle.
    fn read_input_link_texts(instance: &RecordInstance) -> InputLinkTexts {
        InputLinkTexts::read_own(instance)
    }

    /// The process cycle's input stage — C's `dbGetLink` calls before the
    /// record body: the soft INP, the closed-loop DOL, the multi-input and
    /// string-input arrays, `sel`'s NVL. Runs with no record lock held, since
    /// every read takes the SOURCE's lock.
    ///
    /// The one guard it takes is for the two per-cycle hooks the record owes
    /// whatever its links say — the process-context push and
    /// `pre_input_link_actions`, which `compress` and `scalcout` use to reset
    /// cycle state — and for the facts that decide whether there is anything
    /// to read at all. A stock database wires none of a `calc`'s inputs, and
    /// that cycle used to walk the whole stage to learn it: the empty INP read
    /// through three classifiers, twelve slots asked for a text that was
    /// never set. It now gets [`InputStage::none`] from inside that guard.
    fn fetch_input_stage(
        &self,
        name: &str,
        guard: &mut DataGuard<'_>,
        plan: &crate::server::record::record_instance::ProcessPlan,
        input_link_texts: &InputLinkTexts,
        visited: &mut ProcStack,
    ) -> InputStage {
        let rec = guard.rec;
        let shape = {
            let instance = guard.hold();

            let is_soft = instance.common.dtyp.is_soft();

            // C `vt.ptime = (dbLinkIsConstant(&prec->tsel) &&
            // prec->tse == epicsTimeEventDeviceTime) ? &prec->time : NULL`
            // — `devAiSoft.c:73-74`, and byte-for-byte the same in every one of
            // the 23 soft input dsets. TSE=-2 says "the device stamps this
            // record", and for a soft channel the device IS the INP link, so
            // `recGblGetTimeStampSimm` (recGbl.c:324-342) deliberately leaves
            // `time` alone and the dset is the only thing that fills it.
            //
            // The TSEL half is read here, ahead of the stamp point where
            // `read_tsel` runs, for the reason C can read it before
            // `recGblGetTimeStampSimm` does: this tests only whether the link
            // is CONSTANT, and a CONSTANT tsel is never loaded into TSE by
            // either — `recGbl.c:315` gates the `dbGetLink` on
            // `!dbLinkIsConstant` — so the two orders cannot disagree.
            let wants_source_time = instance.common.tse == -2
                && crate::server::recgbl::simm::is_constant(&instance.parsed_tsel);

            // DOL link info for the records that perform C's SCALAR
            // closed-loop DOL fetch. Which records those are is
            // `Record::fetches_dol_closed_loop`, whose doc carries the C
            // citations and names the OMSL-bearing records that answer false.
            let dol = if plan.fetches_dol_closed_loop {
                let omsl = instance
                    .record
                    .get_field("OMSL")
                    .and_then(|v| v.to_menu_index())
                    .unwrap_or(0);
                let oif = instance
                    .record
                    .get_field("OIF")
                    .and_then(|v| v.to_menu_index())
                    .unwrap_or(0);
                if omsl == 1 {
                    let dol_parsed = instance
                        .record
                        .get_field("DOL")
                        .and_then(|v| {
                            if let EpicsValue::String(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .map(|s| crate::server::record::parse_link_v2(s.as_str_lossy().as_ref()))
                        .unwrap_or(crate::server::record::ParsedLink::None);
                    // C `!dbLinkIsConstant(&prec->dol)` gates the per-cycle
                    // DOL fetch in every OMSL record (e.g.
                    // `aoRecord.c:181`, `boRecord.c:192`,
                    // `dfanoutRecord.c:117`): a *constant* DOL is applied to
                    // VAL exactly once at init via `recGblInitConstantLink`
                    // and never re-sourced at process — so a client caput to
                    // VAL is not clobbered every cycle. Only a real
                    // (DB/CA/PVA) link is fetched here. The per-record init
                    // application lives in each record's `init_record`.
                    if matches!(dol_parsed, crate::server::record::ParsedLink::Constant(_)) {
                        None
                    } else {
                        Some((dol_parsed, oif))
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // The pre-input stage's own two asks, under the same guard: C
            // hands a record its `dbCommon` context for free, and the port's
            // hook plus `pre_input_link_actions` were taking an acquisition of
            // their own immediately after this one for a list that is empty on
            // all but compress, histogram, scalcout, sseq and waveform.
            let inst = &mut *instance;
            let ctx = inst.common.process_context();
            inst.record.set_process_context(&ctx);
            let pre_input_actions = instance.record.pre_input_link_actions();

            // Everything the stage below could read is unset: the answer C's
            // `dbConstGetValue` gives twelve times over, taken once, and
            // taken before INP is cloned out of the guard — the clone is the
            // fetch's to own once the guard is released. The type-static
            // halves come off the plan; the per-instance halves were read
            // under this guard.
            let only_own_inputs = crate::server::recgbl::simm::is_constant(&instance.parsed_inp)
                && dol.is_none()
                && pre_input_actions.is_empty()
                && !plan.string_input
                && !plan.sel_nvl
                && !plan.resolves_subroutine_from_link;
            if only_own_inputs && input_link_texts.none_set() {
                instance.record.set_fetch_gate_failed(false);
                return InputStage::none(is_soft, 0);
            }
            // The cycle whose only reads are the record's own input links,
            // each at its own type — a wired `calc` — is the multi-input
            // loop alone: no INP or DOL to clone out and read, no deferred
            // delivery for the body's hold. It runs below in that shape,
            // under the guard this block holds.
            if only_own_inputs && plan.multi_inputs_read_native && !plan.narrows_input_links {
                Err(is_soft)
            } else {
                // A constant (or unset) INP is no read — C `dbConstGetValue`
                // returns 0 without touching the buffer — so nothing below
                // asks it: not the soft read, not the source alarm, not a
                // remote time. `None` says so once, instead of each reader
                // finding out.
                let inp = (!crate::server::recgbl::simm::is_constant(&instance.parsed_inp))
                    .then(|| instance.parsed_inp.clone());
                Ok((inp, is_soft, wants_source_time, dol, pre_input_actions))
            }
        };
        let (inp, is_soft, wants_source_time, dol_info, pre_input_actions) = match shape {
            Ok(general) => general,
            Err(is_soft) => {
                let resolved = self.fetch_own_native_inputs(guard, plan, input_link_texts, visited);
                return InputStage::none(is_soft, resolved);
            }
        };
        // The reads between here and the multi-input loop — pre-input
        // actions, INP, DOL, NVL — go through the by-name link readers, which
        // take the record's lock themselves; the loop takes the guard back.
        if inp.is_some() || dol_info.is_some() || plan.sel_nvl || !pre_input_actions.is_empty() {
            guard.release();
        }

        // 1.1. Pre-input-link actions: actions a record needs the
        // framework to execute BEFORE any input-link fetch this cycle.
        //
        // C `devEpidSoftCallback.c:120-151`: a DB-type readback-trigger
        // (TRIG) link is written with `dbPutLink` — which synchronously
        // processes the triggered source — and only then does
        // `dbGetLink(&pepid->inp, ...)` read CVAL. The trigger write
        // must land before the `INP -> CVAL` fetch, in the same pass.
        // `pre_process_actions` runs too late (after the input-link
        // fetch below), so `pre_input_link_actions` is a strictly
        // earlier hook. The record needs `dtyp` to decide whether the
        // callback DSET is active, so push the process context first.
        //
        // The ReadDbLink actions of this stage go through the reporting owner
        // (`execute_read_db_links`), not the fire-and-forget one: a failed read
        // here is a `dbGetLink` failure like any other, and the record must be
        // able to see it. C `aaoRecord.c::process` (167-168) aborts the whole
        // cycle when its closed-loop DOL fetch fails —
        // `if ((status = fetchValue(prec, 0))) return status;` returns BEFORE
        // `writeValue`, `monitor` and `recGblFwdLink` — which it can only do
        // because `fetchValue`'s `dbGetLink` status reaches it. Discarding the
        // outcome (as this stage did) let a dead DOL write a stale VAL to OUT,
        // post monitors and fire the forward link, every cycle, with no alarm.
        let mut pre_input_resolved: Vec<&'static str> = Vec::new();
        {
            if !pre_input_actions.is_empty() {
                let (reads, others): (Vec<_>, Vec<_>) =
                    pre_input_actions.into_iter().partition(|a| {
                        matches!(a, crate::server::record::ProcessAction::ReadDbLink { .. })
                    });
                if !reads.is_empty() {
                    pre_input_resolved = self.execute_read_db_links(name, rec, &reads, visited);
                }
                if !others.is_empty() {
                    self.execute_process_actions(name, rec, others, visited);
                }
            }
        }

        // Read INP value, converted to the record's declared `dbrType`
        // request (stringin/lsi ask for `DBR_STRING`/`dbGetLinkLS` —
        // `devSiSoft.c:53`, `devLsiSoft.c:32` — so an ENUM/MENU source
        // delivers its state label, not the index).
        let inp_value = inp.as_ref().and_then(|inp_parsed| {
            self.read_link_value_soft(inp_parsed, is_soft, visited)
                .and_then(|v| self.typed_input_value(rec, "INP", inp_parsed, v))
        });

        // C `readLocked` (`devAiSoft.c:54-63`): the same `dbLinkDoLocked` that
        // read the value reads the source's timestamp, under the source's lock
        // and gated on the read having succeeded — `if (!status && pvt->ptime)
        // dbGetTimeStamp(pinp, pvt->ptime)`. The tag half is dropped because
        // `dbGetTimeStamp` passes NULL for it (`dbLink.c:415-418`).
        //
        // A `lnkCalc` INP is the one class where the tag DOES arrive: the
        // adoption is not the dset's at all but the link's own, and
        // `lnkCalc_getValue` writes `prec->time` AND `prec->utag`
        // (`lnkCalc.c:580-581`) under the identical `dbLinkIsConstant(&prec
        // ->tsel) && prec->tse == epicsTimeEventDeviceTime` gate that
        // `wants_source_time` already carries. So the pair the owner returns
        // is adopted whole for a calc link and time-only otherwise.
        let (inp_source_time, inp_source_utag): (Option<std::time::SystemTime>, Option<u64>) =
            if let Some(inp_parsed) = inp.as_ref()
                && is_soft
                && wants_source_time
                && inp_value.is_some()
            {
                match self.db_get_time_stamp_tag(inp_parsed) {
                    Some((t, tag))
                        if matches!(inp_parsed, crate::server::record::ParsedLink::Calc(_)) =>
                    {
                        (Some(t), Some(tag))
                    }
                    Some((t, _tag)) => (Some(t), None),
                    None => (None, None),
                }
            } else {
                (None, None)
            };

        // epics-base PR #d0cf47c: single-INP MS-class link must also
        // propagate the source record's STAT/SEVR/AMSG just like the
        // multi-input fetch loop below does. Previously the INPA..L
        // path (calc/sub/aSub/sel) propagated alarms but plain single
        // INP (ai/bi/longin/mbbi/stringin) silently dropped them —
        // downstream MSS readers saw NoAlarm even when the source was
        // INVALID. Only fires for soft-channel records: hardware-driver
        // alarms travel through device-support's own last_alarm path.
        //
        // B2: a soft INP that is an external `pva://` / `ca://` link
        // also propagates the lset's alarm. The link string carries
        // no `MonitorSwitch` (the `?sevr=MS` modifier is stripped by
        // the parser before epics-base-rs sees it), so the lset has
        // already applied the MS/NMS/MSI gate — a `Some` LinkAlarm
        // here is one the lset decided to propagate. We fold it in as
        // `MaximizeStatus` so the gated severity AND message both
        // reach `LINK_ALARM`, matching `pvxs/ioc/pvalink_lset.cpp`
        // `recGblSetSevrMsg`.
        let inp_link_alarm: Option<(
            crate::server::record::MonitorSwitch,
            super::links::LinkAlarm,
        )> = if let Some(inp_parsed) = inp.as_ref()
            && is_soft
        {
            let (_v, alarm) = self.read_link_with_alarm(inp_parsed);
            self.input_link_inheritance(rec, inp_parsed, alarm)
        } else {
            None
        };

        // if the single-INP link is an external `pva://` /
        // `ca://` link configured with `time=true`, the lset returns
        // the latched upstream NT timestamp here and we adopt it
        // into the owning record's `common.time` and `common.utag`. The
        // lset gates the option internally (returns `None` unless
        // `time=true`), so a bare connected link without the flag still
        // produces local processing time. Mirrors pvxs
        // `pvxs/ioc/pvalink_lset.cpp:577-593`.
        let inp_link_remote_time: Option<(i64, i32, u64)> = inp
            .as_ref()
            .and_then(|inp_parsed| inp_parsed.external_pv_name())
            .and_then(|name| self.external_link_time(&name));

        // Read DOL value. Through the input-fetch owner, so C's
        // `dbDbGetValue` inheritance tail runs on it like every other
        // process-time read: `field(DOL,"SRC MS")` on an OMSL=closed_loop
        // ao/bo/dfanout raises the READER to the source's severity
        // (softIoc: SRC in MAJOR -> A1 SEVR MAJOR, STAT LINK). A constant DOL
        // never reaches here (`dol_info` excludes it — the constant is seeded
        // once at init), so the PP-aware fetch is the right one.
        //
        // The three outcomes stay APART here. C's DOL read is a `dbGetLink`
        // whose non-zero status has effects beyond "no value arrived":
        // `setLinkAlarm` raises LINK/INVALID (owned by `db_get_input_link`),
        // and every OMSL record then gates its own body on the status —
        // `if(!status) convert(prec, value)` (aoRecord.c:188,
        // longoutRecord.c:155, int64outRecord.c:146) or `goto CONTINUE`
        // (mbboRecord.c:206, mbboDirectRecord.c:186). Collapsing `Failed` into
        // "no value" with `LinkFetch::value()` dropped BOTH: a dead DOL left
        // the client's last `caput` sitting in VAL, ran the forward convert on
        // it, and drove it to the output with no alarm at all.
        let dol_fetch: Option<crate::server::recgbl::simm::LinkFetch> =
            dol_info.as_ref().map(|(dol_parsed, _oif)| {
                // Converted to the record's declared request: stringout reads
                // DOL with `DBR_STRING` (`stringoutRecord.c:141`), lso via
                // `dbGetLinkLS` (`lsoRecord.c:114`) — an ENUM/MENU DOL source
                // delivers its label, not the index.
                let fetch = self.db_get_input_link(rec, "DOL", dol_parsed, visited);
                self.convert_link_fetch(rec, "DOL", dol_parsed, fetch).0
            });
        // C's `if (status)` on the closed-loop DOL read, read twice below: once
        // by the record's own failure arm at the DOL-apply site, once by the
        // timestamp gate (mbbo/mbboDirect's `goto CONTINUE` jumps past
        // `recGblGetTimeStampSimm`, mbboRecord.c:221).
        let dol_read_failed = matches!(
            dol_fetch,
            Some(crate::server::recgbl::simm::LinkFetch::Failed)
        );

        // 1.45. Sel NVL link: resolve NVL -> SELN BEFORE the input fetch.
        // C `selRecord.c::fetch_values` reads NVL into SELN first, then in
        // `Specified` mode fetches ONLY INP[SELN] (lines 421-432) — the
        // non-selected inputs are never read. Resolving the selector here
        // (rather than after the fetch) lets `select_input_links` restrict
        // the fetch list, so non-selected links raise no monitors and no
        // spurious link-alarm SEVR.
        // A CONSTANT NVL is not a failed read: C `selRecord.c:99` seeds SELN
        // from it once at init (`recGblInitConstantLink(&nvl, DBF_USHORT,
        // &seln)`) and `dbGetLink` then delivers nothing every cycle, so
        // `fetch_values` succeeds and `do_sel` runs on the seeded SELN.
        let mut sel_nvl_read_failed = false;
        let sel_nvl_value: Option<EpicsValue> = if !plan.sel_nvl {
            None
        } else {
            // Extract the NVL link spec under a scoped read guard, releasing it
            // (the parking_lot guard is !Send) before the async input fetch.
            let nvl_str = {
                let instance = rec.read();
                // C reads NVL ONLY in `Specified` mode: the `dbGetLink(&nvl,
                // ...)` at `selRecord.c:423` sits inside `if (prec->selm ==
                // selSELM_Specified)` and the all-inputs loop below it never
                // touches the link. So in High/Low/Median a dead NVL processes
                // no PP source and raises no `setLinkAlarm`, and SELN keeps
                // its value.
                if instance.record.record_type() == "sel"
                    && matches!(instance.record.get_field("SELM"), Some(EpicsValue::Enum(0)))
                {
                    instance
                        .record
                        .get_field("NVL")
                        .and_then(|v| {
                            if let EpicsValue::String(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                } else {
                    Default::default()
                }
            };
            if !nvl_str.is_empty() {
                let parsed = crate::server::record::parse_link_v2(nvl_str.as_str_lossy().as_ref());
                let fetch = self.db_get_input_link(rec, "NVL", &parsed, visited);
                sel_nvl_read_failed = !fetch.is_ok();
                fetch.value()
            } else {
                None
            }
        };
        // Selector index for `select_input_links`: the freshly-resolved NVL
        // value when present, else `None` (the hook falls back to the
        // record's current SELN).
        let sel_selector: Option<u16> = sel_nvl_value
            .as_ref()
            .and_then(|v| v.get_convert_f64())
            .map(|f| f as u16);

        // 1.5. Multi-input link fetch (calc/calcout/sel/sub)
        // C's `fetch_values` runs inside `process()`, under the record lock
        // it entered with, and each `dbGetLink` writes its result straight
        // into the record (`calcRecord.c:434`). The loop below does the
        // same: it holds the guard across a read whose target is another
        // record — the lock set is shared, so the target's lock is a
        // re-entry — and gives it up only for a read that could reach this
        // record's data again: a self-link, a `PP` source to process first,
        // a link with no local target. Each result is delivered, and its
        // alarm inherited, before the next link is read, as `dbGetLink` does.
        //
        // Link fields whose fetch actually produced a value this cycle —
        // pushed to the record via `set_resolved_input_links` so its
        // `process()` can observe link-fetch success (C
        // `RTN_SUCCESS(dbGetLink(...))`). ONE list per cycle, covering every
        // framework-run input read: the pre-input stage (aao DOL, sseq SELL),
        // the `multi_input_links` fetch, and the pre-process ReadDbLink reads.
        // Built only for a type that reads it.
        let resolved_link_fields = pre_input_resolved;
        let mut fold = FetchFold::default();
        {
            // The shape questions — what a failed read means
            // (`input_fetch_policy`), whether a constant delivers at process
            // (`printf` alone), and whether the fetch is C's `dbGetLink` —
            // are settled when the type is compiled, so the cycle carries
            // them rather than asking the record.
            let input_fetch_policy = plan.input_fetch_policy;
            let instance = guard.hold();
            // Restrict to the record's active inputs this cycle (sel
            // `Specified` → only INP[SELN]); `None` = fetch every link, which
            // is every record type but `sel` / `swait` and every pass of them
            // that does not narrow. That unrestricted case is what the cycle
            // pre-read at its top — see `read_input_link_texts` — so it is
            // taken here rather than read a second time. A restriction that
            // selects nothing is still a restriction: the `Option`, not the
            // emptiness, says whether the record narrowed its inputs this
            // pass.
            let restricted: Option<InputLinkTexts> = if plan.narrows_input_links {
                instance
                    .record
                    .select_input_links(sel_selector)
                    .map(|subset| input_link_texts.read_narrowed(instance, subset))
            } else {
                None
            };
            let link_texts = restricted.as_ref().unwrap_or(input_link_texts);
            let declared = link_texts.links();
            // The record's cache is indexed by its OWN list; a narrowed list
            // maps each of its slots back by name.
            let own = link_texts.own();
            let own_list = std::ptr::eq(declared, own);
            // Over the set links only: C's loop visits every declared link,
            // but an unset one is a `dbConstGetValue` success with nothing to
            // deliver, so the passes it would make here are no-ops.
            let mut wired = link_texts.wired();
            while wired != 0 {
                let slot = wired.trailing_zeros() as usize;
                wired &= wired - 1;
                let (link_field, val_field) = declared[slot];
                let cache_slot = if own_list {
                    Some(slot)
                } else {
                    own.iter().position(|(lf, _)| *lf == link_field)
                };
                debug_assert!(
                    cache_slot.is_some(),
                    "select_input_links must narrow to a subset of multi_input_links"
                );
                let Some(cache_slot) = cache_slot else {
                    continue;
                };
                debug_assert_eq!(
                    own[cache_slot],
                    (link_field, val_field),
                    "a narrowed link is the same pair as its multi_input_links entry"
                );
                let Some(outcome) = self.fetch_multi_input(guard, plan, own, cache_slot, visited)
                else {
                    continue;
                };
                if fold.note(
                    input_fetch_policy,
                    cache_slot,
                    slot + 1 == declared.len(),
                    outcome,
                ) {
                    break;
                }
            }

            // C `selRecord.c::fetch_values` returns the status of its LAST
            // `dbGetLink` (`:434-437` assigns `status` unguarded every pass),
            // and `process` (`:114-116`) gates `do_sel` on it in EVERY mode.
            // The gate is "the last link read FAILED" — never "a link
            // delivered no value": `dbGetLink` on an unset OR constant link
            // returns success (`dbConstGetValue`), and the field it would have
            // written keeps its init-seeded value, which flows into `do_sel`.
            // `Specified` mode returns early on a failed NVL read
            // (`selRecord.c:423-425`), before any INP is touched. Only `sel`
            // reads NVL, so this needs no record-type test.
            let fetch_values_failed = fold.failed(input_fetch_policy, sel_nvl_read_failed);

            // The outcome, delivered here under the guard the loop holds: to
            // `Record::set_fetch_gate_failed` for the records that compute in
            // their own `process()` (calc/calcout/scalcout/acalcout/swait/sel)
            // — written on EVERY cycle, `false` included, so the flag cannot
            // outlive the cycle it belongs to — and, for sub/aSub, whose body
            // is the framework-dispatched subroutine, to the same one-shot
            // skip the bad-SNAM path arms (C `subRecord.c:144-147`,
            // `aSubRecord.c:216-218`: `status = fetch_values(prec); if
            // (status == 0) status = do_sub(prec);`), consumed by the single
            // owner `run_registered_subroutine`.
            let inst = guard.hold();
            inst.record.set_fetch_gate_failed(fetch_values_failed);
            if fetch_values_failed {
                inst.suppress_subroutine_run = true;
            }
        }
        let resolved = fold.resolved;
        // The two stages below read this record by name through the shared
        // lock, so they run with the guard released.
        if plan.string_input || plan.resolves_subroutine_from_link {
            guard.release();
        }

        // The multi-input fetch delivered everything it read; what is left
        // is the reads whose delivery waits for the body's own hold. A cycle
        // that made none of them — a `calc` with only its inputs wired —
        // hands the body the same nothing a cycle with no links does.
        let deferred = inp.is_some()
            || dol_info.is_some()
            || plan.sel_nvl
            || plan.string_input
            || plan.resolves_subroutine_from_link
            || !resolved_link_fields.is_empty()
            || inp_link_alarm.is_some();
        if !deferred {
            return InputStage::none(is_soft, resolved);
        }
        // PR #d0cf47c continued: the INP alarm (if any) goes into the same
        // `link_alarms` list the lock-section iterates over. Order doesn't
        // matter — `rec_gbl_set_sevr_msg` takes the maximum severity across
        // all sources.
        let mut link_alarms: Vec<(
            crate::server::record::MonitorSwitch,
            super::links::LinkAlarm,
        )> = Vec::new();
        link_alarms.extend(inp_link_alarm);
        // The two fetches only a deferring cycle has, made once the early
        // return is behind them: built before it, their empty results were
        // dropped on the path that never has them.
        // 1.6. String-input link fetch — C `sCalcoutRecord.c::fetch_values`'s
        // SECOND loop (890-942), over INAA..INLL → AA..LL. It is a separate
        // loop here for the same reason it is one in C: it does not feed the
        // fetch gate (`return(0)` at :943, so a failing string link never
        // suppresses sCalcPerform), a failed read writes a diagnostic INTO the
        // value field instead of leaving it alone, and a multi-element
        // DBF_CHAR/DBF_UCHAR source is read as escaped text. See
        // `Record::string_input_links`.
        let string_input_values: Vec<(String, EpicsValue)> = if !plan.string_input {
            Vec::new()
        } else {
            let link_info: Vec<(String, &'static str, &'static str)> = {
                let instance = rec.read();
                instance
                    .record
                    .string_input_links()
                    .iter()
                    // C (:895-911): an unset link is neither CA_LINK nor
                    // DB_LINK, so neither `dbGetLink` branch runs, `status`
                    // stays 0, and the string field keeps whatever was last
                    // put to it. Dropping it here is that same skip, taken
                    // before the text is materialised rather than after.
                    .filter_map(|(lf, vf)| Some((instance.link_text(lf)?, *lf, *vf)))
                    .collect()
            }; // read lock dropped
            let mut results = Vec::with_capacity(link_info.len());
            for (link_str, link_field, val_field) in &link_info {
                let parsed = crate::server::record::parse_link_v2(link_str);
                if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                    self.process_passive_db_source(db, visited);
                }
                // C `sCalcoutRecord.c:916` / `:934` read these with `dbGetLink`
                // like every other input, so a failed one raises `setLinkAlarm`
                // (LINK/INVALID, AMSG `field INAA`) even though `fetch_values`
                // itself returns 0 (`:941`) and never gates `sCalcPerform`.
                let request = rec.read().record.input_link_request(link_field);
                let (fetch, alarm, _raw) =
                    self.db_get_link_deferred(rec, link_field, &parsed, None, request);
                if let Some(pair) = self.input_link_inheritance(rec, &parsed, alarm) {
                    link_alarms.push(pair);
                }
                let text = match fetch {
                    crate::server::recgbl::simm::LinkFetch::Value(value) => {
                        string_link_text(&value)
                    }
                    // C (:894-911) only reads a CA_LINK or a DB_LINK; a
                    // CONSTANT string link is never read and never seeded:
                    // the `if (i < MAX_FIELDS)` gate around the seed
                    // (`sCalcoutRecord.c:257-260`, under the comment "Don't
                    // InitConstantLink the string links" at `:256`) skips
                    // every string link, so `status` stays 0 and the string
                    // field keeps what was last put to it — no diagnostic.
                    crate::server::recgbl::simm::LinkFetch::NoData => continue,
                    // C (:939-940): `epicsSnprintf(*psvalue, STRING_SIZE-1,
                    // "%s:fetch(%s) failed", pcalc->name, sFldnames[i])` — the
                    // failed fetch REPLACES the value with the diagnostic; the
                    // previous string is not kept, and the record still computes.
                    crate::server::recgbl::simm::LinkFetch::Failed => truncate_string_field(
                        PvString::from(format!("{name}:fetch({val_field}) failed")),
                    ),
                };
                results.push((val_field.to_string(), EpicsValue::String(text)));
            }
            results
        };

        // aSub LFLG=READ: re-read the subroutine name from the SUBL link and,
        // if it changed, re-resolve the function — computed here, before the
        // process write lock, so the SUBL link read cannot deadlock against
        // this record (C `aSubRecord.c::fetch_values`). `None` for everything
        // that is not an aSub in READ mode.
        let asub_dynamic = if plan.resolves_subroutine_from_link {
            self.resolve_asub_dynamic_subroutine(rec)
        } else {
            None
        };

        InputStage {
            is_soft,
            resolved,
            links: Some(LinkInputs {
                inp_value,
                inp_source_time,
                inp_source_utag,
                inp_link_remote_time,
                dol_info,
                dol_fetch,
                dol_read_failed,
                sel_nvl_value,
                string_input_values,
                asub_dynamic,
                resolved_link_fields,
                link_alarms,
            }),
        }
    }

    /// The record process cycle itself — C `dbProcess`'s body
    /// (`dbAccess.c:537-700`), entered with the record's advisory write gate
    /// already held (or deliberately not held, for the recursive /
    /// already-locked entries).
    ///
    /// **This function and everything it calls is synchronous.** That is the
    /// H6 contract: the gate-held region must contain no suspension point,
    /// because the gate is about to become a blocking priority-inheritance
    /// mutex and a suspended task holding it would deadlock the executor.
    /// Where C's `dbProcess`
    /// cannot finish inline it sets `PACT` and RETURNS, releasing
    /// `dbScanLock`, and the device callback re-takes the lock later
    /// (`dbAccess.c:611-628`, `dbNotify.c:252-264`); every deferred step here
    /// does the same — it stages work on a queue or spawns a task and returns.
    #[allow(clippy::too_many_arguments)]
    fn process_record_with_links_body(
        &self,
        name: &str,
        rec: &Arc<RecordCell>,
        visited: &mut ProcStack,
        is_continuation: bool,
        device_callback: bool,
    ) -> CaResult<()> {
        let mut cycle_end = CycleEndGuard::new(self, name, rec);
        let mut guard = DataGuard::new(rec);

        // 0a. PACT entry guard — C `dbProcess`'s PACT test (dbAccess.c:536,
        // 557-558 at R7.0.10). If the record is currently mid-async, do NOT
        // re-enter the body; hand the refusal to `count_refused_active_entry`,
        // which owns the counting and the alarm for both of the port's
        // "active" tests.
        //
        // Without this guard, FLNK / scan-loop / event scans dispatched onto
        // a record whose first cycle is still pending (async device support,
        // CA put_notify on PUTF) would re-enter `record.process()` while the
        // device's first response is still in flight — corrupting the
        // record's internal state machine and bypassing the C-parity
        // contract that callers see for `dbProcess`. This is where the port
        // decides what an ASYNC-active record does with a foreign process
        // request; `process_one_cp_target` used to pre-empt it with an
        // RPRO-and-skip of its own, which is how a starved CP target got an
        // extra device write instead of C's SCAN_ALARM.
        //
        // Both questions are asked under one guard. C reads the type's `rset`
        // and tests `pact` inside a single `dbScanLock`; the plan is settled
        // at construction and the PACT test is two field reads, so splitting
        // them across two acquisitions cost the record lock twice at the top
        // of every cycle and bought nothing.
        let (plan, active, input_link_texts, metadata) = {
            let plan = guard.rec.process_plan();
            let instance = guard.hold();
            if instance.is_destroyed() {
                return Err(CaError::ChannelNotFound(name.to_string()));
            }
            let active = !is_continuation
                && if instance.is_processing() {
                    true
                } else {
                    // Not pact: reset lcnt (C `else { precord->lcnt = 0; }`
                    // at dbAccess.c:558) so the next async cycle starts clean.
                    instance.common.lcnt = 0;
                    false
                };
            let input_link_texts = Self::read_input_link_texts(instance);
            // C reads a link-backed field's metadata live inside the rset,
            // under the TARGET record's lock (`dbDbLink.c:240-261`). A poster
            // here holds THIS record's lock and cannot reach for a second one,
            // so the cycle resolves once, below, and hands every poster the
            // borrowed result. The borrow is what makes "the metadata a
            // monitor carries was resolved during this cycle" true by
            // construction: there is nowhere to keep it.
            //
            // What the resolve asks of this record — which links, and whether
            // anyone is subscribed to read the answer — it asks here, under
            // the guard the cycle already holds. The lock set is held for the
            // whole cycle, so the subscriber list it reads is the one every
            // poster below will see. Empty for every record type that backs
            // no field's metadata with a link — all but calc, calcout, sub,
            // aSub and seq.
            let metadata = if active {
                MetadataPlan::Empty
            } else {
                PvDatabase::plan_link_backed_metadata_for_posts(instance, &input_link_texts)
            };
            (plan, active, input_link_texts, metadata)
        };
        if active {
            guard.release();
            self.count_refused_active_entry(rec);
            return Ok(());
        }

        // The walk locks each link's target, which for a self-link is this
        // record; a plan with nothing to walk keeps the guard.
        if matches!(metadata, MetadataPlan::Links(_)) {
            guard.release();
        }
        let link_backing = self.resolve_link_backed_metadata_plan(metadata);
        let link_backing = link_backing.as_link_backing();

        // 0. SDIS disable check — C parity dbAccess.c:562-592.
        //
        // When the SDIS link evaluates to a value equal to DISV, the
        // record is disabled and bails before record support runs. C
        // ALWAYS clears rpro/putf and triggers dbNotifyCompletion at
        // this point — regardless of whether the alarm transition
        // fires — because a disabled record must not leave behind
        // pending reprocess requests or stranded put_notify completion
        // callbacks. Pre-fix the Rust port only reset
        // nsta/nsev and updated the alarm state, leaking rpro/putf
        // into the next cycle and stalling CA WRITE_NOTIFY callers
        // (the put_notify_tx never fired so the CA dispatcher waited
        // until socket disconnect to release the operation).
        let no_sim_pact_exit;
        {
            // C `dbGetLink(&precord->sdis, DBR_SHORT, &precord->disa, 0, 0)`
            // (`dbAccess.c:566`) reads the SDIS link regardless of its type
            // (DB / CA / PVA / constant) via the lset — so it goes through the
            // one classifier. A CONSTANT SDIS delivers NOTHING
            // (`dbConstGetValue`), and dbCommon has no `recGblInitConstantLink`
            // for SDIS, so DISA keeps its `initial(0)`: `field(SDIS,"3")` with
            // `DISV=3` does NOT disable the record in C (softIoc-verified).
            // Handing back the constant here disabled it forever.
            //
            // Which of the two it is, is asked in the guard that reads DISV and
            // DISS: a record with no SDIS source still honours a DISA a client
            // put there, so the test below stays, but the link clone, the read
            // and the second guard that re-reads DISA after it all belong to
            // the sourced case alone.
            //
            // The same guard answers the cycle's PACT-exit question. C reads
            // DISA/DISV/DISS and the record's notify state under the one
            // `dbScanLock`; the port asked for them in two acquisitions with
            // nothing but read-only tests in between.
            let (sdis_link, disv, diss, disa) = {
                let instance = guard.hold();
                let sourced = !crate::server::recgbl::simm::is_constant(&instance.parsed_sdis);
                no_sim_pact_exit = instance.pact_exit_without_release();
                (
                    sourced.then(|| instance.parsed_sdis.clone()),
                    instance.common.disv,
                    instance.common.diss,
                    instance.common.disa,
                )
            };

            let disa = match sdis_link {
                Some(sdis_link) => {
                    guard.release();
                    if let Some(val) = self.db_get_link(rec, "SDIS", &sdis_link).value() {
                        // C `dbGetLink(&prec->sdis, DBR_SHORT, &prec->disa)` — the
                        // routine is picked by the SOURCE type, so this goes through
                        // the coercion owner, not `c_cast` direct (an integer SDIS
                        // source takes C's defined modular conversion; only a float
                        // source takes the UB cast).
                        let disa_val = val.to_dbf_i16().unwrap_or(0);
                        guard.hold().common.disa = disa_val;
                    }
                    guard.hold().common.disa
                }
                None => disa,
            };
            if disa == disv {
                let notify = {
                    let instance = guard.hold();
                    // C `dbAccess.c:575-577` — clear rpro/putf and arm
                    // notifyCompletion BEFORE the alarm check. Disabled
                    // records skip processing entirely, so any pending
                    // reprocess request is dropped (the next non-
                    // disabled cycle will pick up fresh state) and the
                    // CA put-notify caller must be released. A disabled
                    // record drives no FLNK/OUT chain, so leaving the
                    // wait-set here is its whole contribution.
                    instance.common.rpro = 0;
                    instance.common.putf = false;
                    let notify = instance.notify.take();

                    // Reset nsta/nsev so stale alarm state doesn't bleed
                    // into a subsequent (re-enabled) cycle. C resets
                    // them after the sevr/stat transition; doing it
                    // first here is observationally identical because
                    // the SDIS bail short-circuits any record-support
                    // path that could read them.
                    instance.common.nsta = 0;
                    instance.common.nsev = crate::server::record::AlarmSeverity::NoAlarm;

                    // C `dbAccess.c:580-581` — if already in
                    // DISABLE_ALARM, the alarm post is skipped entirely
                    // (the alarm cycle is debounced). The rpro/putf
                    // clear above still ran, matching C's pre-`goto
                    // all_done` ordering.
                    if instance.common.stat != crate::server::recgbl::alarm_status::DISABLE_ALARM {
                        use crate::server::recgbl::EventMask;
                        instance.common.sevr =
                            crate::server::record::AlarmSeverity::from_u16(diss as u16);
                        instance.common.stat = crate::server::recgbl::alarm_status::DISABLE_ALARM;
                        // C `dbAccess.c:586-593` posts each field with
                        // its own mask:
                        //   db_post_events(&stat, DBE_VALUE);
                        //   db_post_events(&sevr, DBE_VALUE);
                        //   db_post_events(&val,  DBE_VALUE|DBE_ALARM);
                        // STAT/SEVR get DBE_VALUE only — a DBE_ALARM-only
                        // subscriber on `.STAT`/`.SEVR` must NOT receive
                        // this disable event. Only the value field
                        // carries DBE_ALARM.
                        instance.notify_field("STAT", EventMask::VALUE);
                        instance.notify_field("SEVR", EventMask::VALUE);
                        instance.notify_field("VAL", EventMask::VALUE | EventMask::ALARM);
                    }
                    notify
                };
                guard.release();
                // Fire dbNotifyCompletion outside the record lock —
                // C `dbAccess.c:622-623` runs it at `all_done` after
                // the disable bail. Without this, a CA WRITE_NOTIFY
                // landing on a disabled record stalls until socket
                // disconnect. `leave` fires the completion oneshot when
                // this empties the wait-set.
                if let Some(ws) = notify {
                    ws.leave();
                }
                return Ok(());
            }
        }

        // 0.4. The dset gate — the FIRST statement of every C `process()`
        // that needs device support:
        //
        // ```c
        // if( (pdset==NULL) || (pdset->read_ai==NULL) ) {
        //     prec->pact=TRUE;
        //     recGblRecordError(S_dev_missingSup, prec, "read_ai");
        //     return(S_dev_missingSup);
        // }
        // ```
        // (`aiRecord.c:143-147`, and the same four lines in 19 more
        // `<rec>Record.c` files.) It sits here, after `dbProcess`'s PACT test
        // and the SDIS disable bail and before anything of the body, because
        // that is where C's is: `dbProcess` reaches `prset->process` only past
        // those two, and `process` refuses on its first line.
        //
        // This is not a message. The PACT it takes is never released — the
        // only release is a cycle tail this record never reaches — so the
        // record is inert from its first process attempt onward, exactly as it
        // is in C, and every later attempt is turned away by the PACT guard
        // above without a second report. Reporting without taking PACT would
        // have printed C's line over a record that then went on processing:
        // measured against `softIoc` R7.0.10 on `asyn`'s `testErrors` IOC, C
        // leaves `testErrors:AoInt32` at `PACT 1`, `STAT UDF`, `TIME
        // <undefined>` where this port left it `PACT 0`, `STAT NO_ALARM` and
        // stamped.
        //
        // The gate is `dev_sup_process_refusal`, which is `None` for every
        // record type whose C `process()` has no dset test — `calc`, `sub`,
        // `fanout`, and `calcout`, which refuses only at init.
        if plan.dset_can_refuse {
            let refusal = {
                let instance = guard.hold();
                if instance.common.dtyp.is_soft() || instance.device.is_some() {
                    None
                } else {
                    crate::server::recgbl::dev_sup_process_refusal(instance.record.record_type())
                }
            };
            if let Some(message) = refusal {
                let notify = {
                    let instance = guard.hold();
                    instance.enter_pact();
                    // C returns from `process()` without reaching
                    // `recGblFwdLink`, so its `dbNotifyCompletion` never fires
                    // and a put-notify parked on such a record waits for a
                    // cycle that will never come. Releasing the wait-set is
                    // the same thing the SDIS bail above does, and for the
                    // same reason: a CA WRITE_NOTIFY caller must not be held
                    // to a socket timeout by a record that has already decided
                    // not to run.
                    instance.notify.take()
                };
                guard.release();
                crate::server::recgbl::rec_gbl_record_error(
                    &crate::server::recgbl::DevSupStatus::MissingSup.text(),
                    name,
                    message,
                );
                if let Some(ws) = notify {
                    ws.leave();
                }
                return Ok(());
            }
        }

        // 0.5. Simulation mode check.
        //
        // C handles simulation inside `readValue()` / `writeValue()` — the
        // device-I/O step — then `process()` ALWAYS runs the rest of the
        // body (`convert` / OROC / the record's own state machine) plus
        // `checkAlarms` / `monitor` / `recGblFwdLink(prec)`. SIMM replaces
        // ONLY the device read/write, never the body. The substitution
        // point differs by direction: an INPUT `readValue()` precedes the
        // body, so `Simulated` does the SIOL read here and short-circuits;
        // an OUTPUT `writeValue()` follows the body, so
        // `RedirectOutputToSiol` falls through to run the uniform body and
        // redirects only the final output write to SIOL (see below). Either
        // way the forward-link / CP / RPRO tail still runs — returning early
        // without it would silently break every FLNK / CP chain downstream
        // of any record in SIMM mode.
        //
        // `sim_output` carries the OUTPUT redirect (SIOL link, SIMS, RAW
        // flag) from this point to the OUT stage / alarm epilogue below;
        // `None` for a non-simulated record or a simulated INPUT.
        // The cycle's simulation state, pushed to the record before the body —
        // the twin of `set_fetch_gate_failed`. Written on EVERY cycle of a record
        // that declares the input-stage shape (`false` included), so the flag
        // cannot outlive the cycle it belongs to.
        let mut sim_input_stage = false;
        // C `writeValue` returned before performing ANY output. `writeValue`
        // runs at the END of C `process()`, so the body has already run and
        // only the device / OUT-link / SIOL write is lost. Two C paths reach
        // it, and both mean exactly this one thing:
        //   * `switch (prec->simm)` `default:` — `recGblSetSevr(SOFT_ALARM,
        //     INVALID_ALARM); return -1;`  (`SimOutcome::IllegalMode`)
        //   * a failed SIML read — `if (status) return status;`
        //     (`SimOutcome::AbortedBeforeWrite`, busyRecord.c:399-401)
        let mut sim_write_aborted = false;
        // The PACT the SDLY defer held, released by the SIM continuation arms —
        // carried to whichever `recGblFwdLink` tail this cycle ends at, so the
        // put-notify parked on that window is replayed there (C
        // `dbNotifyCompletion`) instead of being stranded.
        let (sim_outcome, sim_pact_exit) = if plan.simulation {
            guard.release();
            self.check_simulation_mode(rec)
        } else {
            (SimOutcome::NotSimulated, no_sim_pact_exit)
        };
        // Every exit below this line owes C's `recGblFwdLink` tail. The guard
        // owns that debt so no path can leave without either paying it or
        // saying, at the site, that it is handing the cycle to someone else.
        cycle_end.merge_in(sim_pact_exit);
        let sim_output = match sim_outcome {
            SimOutcome::NotSimulated => None,
            SimOutcome::Simulated(posts) => {
                self.run_forward_link_tail(name, rec, posts, visited);
                self.end_process_cycle(name, rec, cycle_end.take());
                return Ok(());
            }
            SimOutcome::AbortedBeforeWrite => {
                // C busy `writeValue`: `status = dbGetLink(&prec->siml, ...);
                // if (status) return status;` — the SIML read failed, so the
                // routine returns before `write_busy` AND before the SIOL
                // redirect. `dbGetLink` has already raised LINK_ALARM/INVALID.
                sim_write_aborted = true;
                None
            }
            SimOutcome::IllegalMode { is_output } => {
                if is_output {
                    // `writeValue` follows the body, so only the write is lost.
                    sim_write_aborted = true;
                    None
                } else {
                    // `readValue` precedes the body and IS the body's input, so
                    // nothing of the body is left to run. SOFT_ALARM/INVALID is
                    // already pending; commit it, post the monitors and fire the
                    // forward link — C `process()` runs `checkAlarms`,
                    // `monitor()` and `recGblFwdLink()` regardless of the -1.
                    let tsel = self.read_tsel(rec);
                    let posts = {
                        let mut instance = rec.write();
                        sim_process_tail(&mut instance, tsel, false, link_backing)
                    };
                    self.run_forward_link_tail(name, rec, posts, visited);
                    self.end_process_cycle(name, rec, cycle_end.take());
                    return Ok(());
                }
            }
            SimOutcome::SimulatedInputStage => {
                sim_input_stage = true;
                None
            }
            SimOutcome::DeferRead(delay) => {
                // C `readValue`/`writeValue` async path: hold PACT and
                // schedule the SIOL round-trip `SDLY` seconds out. Post
                // nothing this cycle — C `process()` returns 0 on the
                // async-start pass (`if (!pact && prec->pact) return 0`), so
                // no value, no alarm, no monitor, no forward link. The
                // continuation re-enters via `process_record_continuation`
                // (`is_continuation = true`) and runs the synchronous branch
                // + tail. The PACT hold is gated on the scheduled re-entry
                // that releases it, the same construction-time invariant as
                // the `ReprocessAfter` ODLY defers.
                {
                    let instance = rec.write();
                    instance.enter_pact();
                }
                self.schedule_delayed_reprocess(name, delay);
                // This arm is reachable only with PACT clear on entry, so nothing
                // can be queued; run the check through the single owner anyway so
                // no path drops a token blind.
                self.apply_pact_exit(name, rec, cycle_end.take());
                return Ok(());
            }
            SimOutcome::RedirectOutputToSiol {
                siol,
                sims,
                raw_mode,
            } => Some((siol, sims, raw_mode)),
        };
        if plan.substitutes_input_stage_when_simulating {
            guard.hold().record.set_simulation_active(sim_input_stage);
        }

        // 1. The input stage: every link read this cycle performs before it
        //    takes the record's lock to apply what arrived. A record with
        //    nothing to read gets the stage's empty result without the stage
        //    running — see `fetch_input_stage`.
        let mut stage = self.fetch_input_stage(name, &mut guard, plan, &input_link_texts, visited);

        // 2. Lock record, apply INP/DOL, process, evaluate alarms, build snapshot
        let (flnk_name, process_actions, result_is_defer_output, restamps_after, posts) = 'epilogue: {
            // One data guard for Segments A–E; each boundary below releases it
            // only across work that may lock another record.
            // Segment A (guarded): apply DOL/INP/multi-input values, run the
            // device read, and collect pre-process ReadDbLink actions. The data
            // guard is released at the segment boundary below so the following
            // link-I/O awaits hold no `!Send` parking_lot guard (the record stays
            // claimed by the `processing` gate meanwhile — the signed-off
            // momentary release, uniform with the async paths that already
            // release the data lock across link I/O here).
            let (
                pre_actions,
                deferred_device_actions,
                is_soft,
                device_did_compute,
                read_produced_no_value,
                device_read_computed,
            ) = {
                let instance = guard.hold();
                // One discriminant for "this cycle sourced no value", set by
                // either source: a failed soft-INP read, and a device support
                // returning C's negative `read_ai()` status (-1, -2). Both miss
                // C's `if (status == 0)` gate identically, so the UDF re-derive
                // below tests one condition rather than a per-source exception.
                let mut read_produced_no_value = false;
                // C `return 2` specifically: the dset wrote VAL. Kept apart
                // from `device_did_compute`, which the soft-INP branch also
                // sets — there the framework IS the dset (it is the port of
                // `devBiSoft.c::readLocked`) and owns the UDF clear, so the
                // record's dset-owns-UDF rule must not fire for it.
                let mut device_read_computed = false;

                // Apply the closed-loop DOL read (OMSL=CLOSED_LOOP), keeping C's
                // three outcomes apart.
                //
                // `Failed` is C's non-zero `dbGetLink` status: the LINK/INVALID
                // alarm already rode in with the read, and the record's own
                // failure arm — `AoRecord::closed_loop_dol_read_failed` reverting
                // VAL to PVAL, every convert-bearing OMSL record suppressing this
                // cycle's convert — runs here.
                //
                // `NoData` is status 0 with the buffer untouched. A CONSTANT DOL
                // never reaches here at all (`dol_info` excludes it), so this is
                // the reader's own `default:` arm (no declared request for this
                // source class): nothing is attempted and nothing changes.
                let links = stage.links.as_mut();
                let dol_fetch = links.as_ref().and_then(|l| l.dol_fetch.as_ref());
                if let Some(crate::server::recgbl::simm::LinkFetch::Failed) = dol_fetch {
                    instance.record.closed_loop_dol_read_failed();
                }
                let mut links = links;
                if let Some(crate::server::recgbl::simm::LinkFetch::Value(dol_val)) =
                    links.as_mut().and_then(|l| l.dol_fetch.take())
                {
                    let oif = links
                        .as_ref()
                        .and_then(|l| l.dol_info.as_ref())
                        .map(|(_, oif)| *oif)
                        .unwrap_or(0);
                    if oif == 1 {
                        // Incremental: C `fetch_value` (aoRecord.c:447-455) sets
                        // `prec->val = prec->pval` first ("don't allow dbputs to
                        // val field"), then `*pvalue += prec->val`, so the
                        // increment is relative to PVAL — the last actual output —
                        // not the current VAL a client may have just caput. OIF is
                        // an ao-only field, so this branch always carries a PVAL.
                        if let (Some(pval), Some(dol_f)) = (
                            instance.record.get_field("PVAL").and_then(|v| v.to_f64()),
                            dol_val.to_f64(),
                        ) {
                            let _ = instance.record.set_val(EpicsValue::Double(pval + dol_f));
                        }
                    } else {
                        // Full: VAL = DOL value
                        let _ = instance.record.set_val(dol_val);
                    }
                    // The closed-loop DOL read DEFINES the record — C sets UDF from
                    // the value it just fetched, in the DOL branch itself:
                    // `prec->udf = isnan(value)` (aoRecord.c:147, dfanoutRecord.c:121)
                    // / `prec->udf = FALSE` (boRecord.c:162). For ao/bo this repeats
                    // what the per-cycle clear below does; for dfanout — whose
                    // `process()` touches UDF nowhere else — it is the ONLY definer,
                    // which is why dfanout can opt out of the per-cycle clear.
                    instance.common.udf = instance.record.value_is_undefined() as u8;
                }

                // Apply INP value. "Soft Channel" sets VAL directly
                // (C `read_xxx` return 2, skip RVAL→VAL conversion).
                // "Raw Soft Channel" is a DIFFERENT DSET (`devXxxSoftRaw.c`): its
                // `read_xxx` puts the value in RVAL, applies the dset's MASK and
                // returns 0, so the record's own RVAL→VAL convert runs. Whether
                // that dset exists is the record type's answer, given by
                // `Record::raw_soft_input` returning `Some` — the dset table, not a
                // separate boolean that could disagree with it.
                let inp_value = links.as_mut().and_then(|l| l.inp_value.take());
                let had_inp_value = inp_value.is_some();
                let mut soft_inp_applied = false;
                if let Some(inp_val) = inp_value {
                    let raw = if instance.common.dtyp.soft()
                        == Some(crate::server::device_support::SoftDtyp::Raw)
                    {
                        instance
                            .record
                            .raw_soft_input(RawSoftEntry::Read, inp_val.clone())
                    } else {
                        None
                    };
                    match raw {
                        // SoftRaw: value landed in RVAL; the record's RVAL->VAL
                        // convert runs in `process()`, so VAL was NOT set here.
                        Some(res) => {
                            let _ = res;
                        }
                        None => {
                            // The soft dset's `read_xxx` body. Only a
                            // soft-channel record has one: a `lnkCalc` INP is
                            // delivered above whatever the DTYP is
                            // (`read_link_value_soft`), and a device record's
                            // own dset has already run its filter.
                            let _ = if stage.is_soft {
                                instance.record.soft_input_read(Some(inp_val))
                            } else {
                                instance.record.set_val(inp_val)
                            };
                            soft_inp_applied = true;
                        }
                    }
                }
                if !had_inp_value
                    && stage.is_soft
                    && crate::server::recgbl::simm::is_constant(&instance.parsed_inp)
                {
                    // C `dbLinkIsConstant(&prec->inp)` at process. The load-once
                    // rule (a constant delivers nothing here — it was loaded at
                    // init) is the default and stays the default; the ONE soft
                    // device support that re-reads its constant INP every process
                    // is `devSASoft.c::read_sa` (subArray), which also re-subsets
                    // on an EMPTY INP. `Record::read_constant_inp` is that
                    // device-support-layer exception: every other record's default
                    // returns false and nothing happens, exactly as before.
                    let constant =
                        crate::server::recgbl::simm::constant_load_value(&instance.parsed_inp);
                    if instance.record.read_constant_inp(constant) {
                        soft_inp_applied = true;
                    }
                } else if !had_inp_value
                    && stage.is_soft
                    && matches!(
                        instance.parsed_inp,
                        crate::server::record::ParsedLink::Db(_)
                            | crate::server::record::ParsedLink::Ca(_)
                            | crate::server::record::ParsedLink::Pva(_)
                            | crate::server::record::ParsedLink::PvaJson(_)
                    )
                {
                    // A soft-channel `read_xxx` is a plain `dbGetLink` on INP
                    // (`devAiSoft.c::read_ai` -> `dbGetLink(&prec->inp, ...)`), so a
                    // failed read runs `setLinkAlarm` (dbLink.c:322) —
                    // `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "field INP")`.
                    // Route it through the `setLinkAlarm` owner so it carries C's
                    // message: raising the severity without the AMSG text left the
                    // operator with an INVALID/LINK record and a blank `.AMSG`.
                    // ParsedLink::None and Constant don't reach this branch — the
                    // former is "no link configured", the latter has its own
                    // None-as-no-value semantics.
                    crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, "INP");
                    // C's failure arm — `devAiSoft.c:92` drops the dset's
                    // "a read has completed" state so the next good reading is
                    // taken unsmoothed.
                    let _ = instance.record.soft_input_read(None);
                    // …and tell the record, so "no value was sourced" stops
                    // being indistinguishable from "no link is configured".
                    // C `devSASoft.c::read_sa` (118-120) skips `subset()` on a
                    // non-zero status and `subArrayRecord.c:148` turns that
                    // status into UDF; without the report the record could only
                    // see its own stale buffer and called itself defined.
                    instance.record.soft_input_read_failed();
                    read_produced_no_value = true;
                }

                // Apply multi-input values (INPA..INPL -> A..L).
                //
                // Uses `put_field_internal`, not `put_field`: this is the
                // framework writing a resolved input-link value into a
                // record field, exactly like the `ReadDbLink` apply
                // (`execute_read_db_links` / `execute_process_actions`),
                // which already routes through `put_field_internal`. Some
                // records map an input link to a normally read-only field
                // — e.g. the epid record's `INP -> CVAL` — and `put_field`
                // rejects those with `ReadOnlyField`, silently dropping the
                // value. `put_field_internal` defaults to `put_field`, so
                // records with writable targets (calc/sub `A..L`) are
                // unaffected.
                // An ARRAY-valued link value is offered to the target field whole:
                // C's `fetch_values` hands `dbGetLink` a pointer to the target FIELD,
                // so the field decides how much of the source it takes. An array
                // field takes `nRequest` = its own element count with the tail
                // zero-filled (aCalcoutRecord.c:1097-1102 for INAA..INLL -> AA..LL);
                // a scalar field is a one-element destination, so it takes element 0
                // (`dbGetLink(..., DBR_DOUBLE, pvalue, 0, 0)`, calcRecord.c:434).
                // The numeric view answers None for every array variant, so routing
                // every value through it dropped array-valued links outright —
                // AA..LL never populated and the record calculated on an empty
                // array. The view is `get_convert_f64`, C's DBR_DOUBLE get row,
                // not `to_f64`: the two disagree on an empty DBF_STRING source.
                let (sel_nvl_value, string_input_values) = match links {
                    Some(l) => (
                        l.sel_nvl_value.take(),
                        Some(std::mem::take(&mut l.string_input_values)),
                    ),
                    None => (None, None),
                };

                // The set_resolved_input_links report is deferred until after
                // the pre-process ReadDbLink reads below, so the record sees
                // ONE per-cycle resolution list covering both fetch paths —
                // records reset per-cycle resolution state in that hook, so
                // it must not run twice with partial lists.

                // Apply sel NVL -> SELN. SELN is DBF_USHORT (selRecord.dbd.pod:295),
                // an unsigned 0..65535 index. Carry the native unsigned value so a
                // link value in 32768..65535 is not lost to f64->i16 saturation
                // before it reaches the field's put.
                if let Some(nvl_val) = sel_nvl_value {
                    // Same one-element-destination rule as the multi-input loop
                    // above: C reads NVL with `dbGetLink(..., DBR_USHORT, &pse->seln,
                    // 0, 0)` (selRecord.c), so an array-valued source contributes its
                    // element 0 rather than being dropped by `to_f64`.
                    let scalar = if nvl_val.is_array() {
                        nvl_val.first_element()
                    } else {
                        Some(nvl_val)
                    };
                    if let Some(f) = scalar.and_then(|v| v.get_convert_f64()) {
                        let _ = instance
                            .record
                            .put_field("SELN", EpicsValue::UShort(f as u16));
                    }
                }

                // Apply the string-input values (scalcout INAA..INLL -> AA..LL),
                // fetched in step 1.6 above. `put_field_internal` is the coercion
                // owner: it converts to the target field's declared `DbFieldType`,
                // which is `String` for every one of these.
                if let Some(string_input_values) = string_input_values {
                    for (val_field, value) in string_input_values {
                        let _ = instance.record.put_field_internal(&val_field, value);
                    }
                }

                // Device support read (input records only, not output records).
                // Shadows the outer `is_soft` on purpose: that one asks "does
                // the framework own this record" (all three soft flavours),
                // this one asks "does the input dset return 2, do-not-convert"
                // — which is Plain and Async but NOT Raw. See
                // `device_support::SoftDtyp`.
                let is_soft = matches!(
                    instance.common.dtyp.soft(),
                    Some(
                        crate::server::device_support::SoftDtyp::Plain
                            | crate::server::device_support::SoftDtyp::Async
                    )
                );
                let is_output = instance.record.can_device_write();
                // The actions a device read handed back, if it handed any: a
                // soft record has no device to read and owes no empty list.
                let mut device_actions: Option<Vec<crate::server::record::ProcessAction>> = None;
                // C `devAiSoft.c:65` `read_ai` (and the other soft-channel
                // input `read_xxx`) ALWAYS returns 2 ("don't convert") for a
                // Soft-Channel input record — whether the value arrived via
                // an INP link or the INP link is constant/unset
                // (`dbLinkIsConstant` → `return 2`). Only `aiRecord.c:158`'s
                // `if (status==0) convert(prec)` runs RVAL→VAL conversion, so
                // for a plain Soft-Channel input record `convert()` must be
                // skipped unconditionally. Without this, a soft ai with no
                // INP would run `convert()` and clobber a preset VAL — e.g.
                // a preset NaN would be rewritten to 0.0, then the framework
                // UDF check (`value_is_undefined()`) would see a defined 0.0
                // and wrongly clear UDF. `SoftDtyp::Raw` is excluded above —
                // `devAiSoftRaw` returns 0 and deliberately wants the RVAL→VAL
                // convert.
                //
                // Gated on `soft_channel_skips_convert()` so this only
                // suppresses an `RVAL → VAL` convert step. Records such as
                // `epid` also override `set_device_did_compute` but treat it
                // as "skip the whole built-in compute" (the PID loop); they
                // return `false` here so a Soft-Channel `epid` still runs
                // `do_pid()` in `process()`.
                let soft_input_skips_convert =
                    is_soft && !is_output && plan.soft_channel_skips_convert;
                let mut device_did_compute =
                    (soft_inp_applied && is_soft) || soft_input_skips_convert;
                // Input records read every cycle (`!is_output`). An OUTPUT record
                // reads only on a driver-callback (`asyn:READBACK`) cycle: it pulls
                // the callback value into VAL here and the OUT stage below skips the
                // write — C `devAsynInt32.c::processBo` `getCallbackValue` readback
                // branch. A put/FLNK/scan cycle (`device_callback == false`) leaves
                // the output untouched here and writes below.
                if !is_soft && (!is_output || device_callback) {
                    if let Some(mut dev) = instance.device.take() {
                        // Push framework-owned common state (PHAS/TSE/TSEL/
                        // UDF) so device support's read() can see it — C
                        // device support reads `dbCommon` directly
                        // (`devTimeOfDay.c:122` uses `psi->phas`).
                        dev.set_process_context(&instance.common.process_context());
                        match dev.read(&mut *instance.record) {
                            Ok(read_outcome) => {
                                let status = read_outcome.status;
                                device_did_compute = status.skips_conversion();
                                if status.read_failed() {
                                    read_produced_no_value = true;
                                }
                                device_read_computed = matches!(
                                    status,
                                    crate::server::device_support::DeviceReadStatus::Computed
                                );
                                // A C dset writes `prec->udf` itself, before
                                // its `return` — `devBiSoft.c::readLocked` and
                                // `devBiDbState.c:67` clear it, `devAsynInt32.c
                                // :902` sets it. Ours cannot reach `dbCommon`
                                // through the `&mut dyn Record` it holds, so the
                                // framework — the single owner of the UDF
                                // transition — applies what the outcome states,
                                // HERE, before the record's own rule below: C's
                                // order is dset first, `process()` second, and
                                // for the record types that re-derive
                                // unconditionally the second write is what wins.
                                use crate::server::device_support::DeviceUdf;
                                match read_outcome.udf() {
                                    DeviceUdf::Untouched => {}
                                    DeviceUdf::Defined => instance.common.udf = 0,
                                    DeviceUdf::Undefined => instance.common.udf = 1,
                                }
                                if !read_outcome.actions.is_empty() {
                                    device_actions = Some(read_outcome.actions);
                                }
                            }
                            Err(e) => {
                                eprintln!("device read error on {}: {e}", instance.name);
                                use crate::server::recgbl::{alarm_status, rec_gbl_set_sevr};
                                rec_gbl_set_sevr(
                                    &mut instance.common,
                                    alarm_status::READ_ALARM,
                                    crate::server::record::AlarmSeverity::Invalid,
                                );
                            }
                        }
                        instance.device = Some(dev);
                    }
                }

                // Pre-process actions: execute ReadDbLink from device support and
                // record's pre_process_actions() BEFORE process() so the values
                // are immediately available. Matches C dbGetLink() semantics.
                let mut pre_actions = instance.record.pre_process_actions();
                // Also collect ReadDbLink from device actions; the rest wait
                // for the record body and join its own actions after it.
                let mut deferred_device_actions: Option<Vec<_>> = None;
                if let Some(device_actions) = device_actions {
                    for action in device_actions {
                        if matches!(
                            action,
                            crate::server::record::ProcessAction::ReadDbLink { .. }
                        ) {
                            pre_actions.push(action);
                        } else {
                            deferred_device_actions
                                .get_or_insert_with(Vec::new)
                                .push(action);
                        }
                    }
                }
                (
                    pre_actions,
                    deferred_device_actions,
                    is_soft,
                    device_did_compute,
                    read_produced_no_value,
                    device_read_computed,
                )
            };

            // await 1 (guard-free): pre-process ReadDbLink resolution. `name` is
            // the record's resolved canonical name (== `instance.name`).
            if !pre_actions.is_empty() {
                guard.release();
                let pre_resolved = self.execute_read_db_links(name, rec, &pre_actions, visited);
                stage
                    .links
                    .get_or_insert_with(LinkInputs::none)
                    .resolved_link_fields
                    .extend(pre_resolved);
            }

            // Segment B (guarded): apply resolved inputs, run the subroutine and
            // `process()`, and classify the outcome. The guard is released before
            // the branch-specific async work below (parking_lot guards are
            // `!Send`); each branch re-acquires the data lock as it needs it. The
            // Segment-A mutations were committed under that guard and are visible
            // through this fresh acquisition (same `Arc`).
            let (
                process_result,
                process_actions,
                post_write_fields,
                result_is_defer_output,
                result_is_alarm_only,
            ) = {
                let instance = guard.hold();

                // Tell the record which input link fields actually resolved
                // a value this cycle — the union of the multi-input fetch and
                // the pre-process ReadDbLink reads; the framework analogue of
                // C device support inspecting `RTN_SUCCESS(dbGetLink(...))`
                // (`epidRecord.c:191-193`, `motorRecord.cc:3687-3698`).
                let links = stage.links.as_ref();
                instance.record.set_resolved_input_links(
                    crate::server::record::ResolvedInputLinks::new(
                        input_link_texts.own(),
                        stage.resolved,
                        links.map_or(&[][..], |l| l.resolved_link_fields.as_slice()),
                    ),
                );

                // The cycle's single `fetch_values()` outcome reached
                // `set_fetch_gate_failed` (and sub/aSub's
                // `suppress_subroutine_run`) inside the input stage, under the
                // guard its loop held.

                // Note: C EPICS LCNT prevents reentrant processing of the same
                // record within a single processing chain. In Rust, this is handled
                // by the `visited` HashSet (cycle detection) and the `processing`
                // AtomicBool guard. LCNT is not needed as a separate mechanism
                // because async processing with visited sets already prevents
                // the runaway loops that LCNT guards against in C.

                // Tell the record whether device support already computed.
                // Records that override set_device_did_compute() use this to
                // skip their built-in computation (e.g., ai skips RVAL->VAL).
                // Note: field_io.rs may have already called set_device_did_compute(true)
                // for CA puts to VAL. We only set true here, never reset to false.
                if device_did_compute {
                    instance.record.set_device_did_compute(true);
                } else if instance.record.skips_forward_convert_when_undefined()
                    && instance.common.udf != 0
                {
                    // C output-record `else if (prec->udf) goto CONTINUE`
                    // (mbboRecord.c:210-213): an output record whose VAL is still
                    // undefined and had no value source this cycle (no VAL put —
                    // which clears UDF in `field_io` — and no closed-loop DOL fetch,
                    // which clears UDF at the DOL-apply site above) SKIPS the
                    // forward VAL->RVAL convert. Without this a `caput REC.RVAL 1`
                    // on a bare mbbo is clobbered by `convert()` recomputing
                    // `RVAL = VAL(=0)`. Same vehicle as the device-compute skip:
                    // `set_device_did_compute(true)` sets the record's own
                    // convert-skip flag, which `process()` consumes and clears. The
                    // per-cycle UDF clear below stays gated on `clears_udf()` /
                    // `device_did_compute` (both false here), so UDF stays 1 —
                    // matching C's `goto CONTINUE` leaving `prec->udf` untouched.
                    instance.record.set_device_did_compute(true);
                }

                // TPRO: trace processing (C EPICS dbProcess prints context when TPRO>0)
                if instance.common.tpro != 0 {
                    eprintln!(
                        "[TPRO] {}: process (SCAN={:?}, PACT={})",
                        instance.name,
                        instance.common.scan,
                        instance.is_processing()
                    );
                }

                // MS-class alarm propagation from input links. Mirrors C
                // `recGblInheritSevrMsg` (recGbl.c:263-281):
                //
                // * NMS  — do nothing.
                // * MS   — DEST gets `LINK_ALARM` (NOT the source stat),
                //          max-raised sevr, NO amsg propagation.
                // * MSI  — same as MS, but only when source.sevr == INVALID.
                // * MSS  — DEST gets source stat, max-raised sevr, source amsg
                //          (PR d0cf47c is the only branch that propagates msg).
                //
                // Folded BEFORE the record body, not after: C raises the link
                // severity inside `dbGetLink` (recGbl.c `recGblInheritSevr` is
                // called from the link's `getValue`), i.e. during the record's
                // input-fetch phase, so the body already sees it in `prec->nsev`.
                // `transformRecord.c:554` branches on exactly that
                // (`nsev >= INVALID_ALARM && ivla == DO_NOTHING`), and
                // `ProcessContext::nsev` below is that same `common.nsev` — one
                // owner, no second severity accumulator for records to consult.
                // Folding it here also gives C's tie-break: with equal severities
                // the link's LINK_ALARM lands first and `rec_gbl_set_sevr`'s
                // strict-greater test keeps it, exactly as in C where `dbGetLink`
                // precedes the record's own `recGblSetSevr` calls.
                for (ms, alarm) in links.map_or(&[][..], |l| l.link_alarms.as_slice()) {
                    super::links::inherit_sevr_msg(&mut instance.common, *ms, alarm);
                }

                // Push framework-owned common state (UDF/UDFS/NSEV/PHAS/TSE/TSEL) so
                // the record's process() can see it — C records read
                // `dbCommon` directly (`epidRecord.c:195` checks
                // `pepid->udf`, `timestampRecord.c:90` checks `tse`,
                // `transformRecord.c:554` checks `ptran->nsev`).
                {
                    let inst = &mut *instance;
                    let ctx = inst.common.process_context();
                    inst.record.set_process_context(&ctx);
                }
                // Tell the record whether this is its own scheduled re-entry
                // (the `ReprocessAfter` timer, a put-notify completion) or a
                // fresh cycle. Only this path can be a continuation; the
                // `process_local` and simulated-read paths always run a fresh
                // `process()`, which is the hook's default.
                instance.record.set_process_continuation(is_continuation);

                // Apply the aSub LFLG=READ resolution computed above (outside the
                // lock). The single apply owner; the bad-sub skip is carried on the
                // instance and consumed by `run_registered_subroutine`.
                if let Some(ds) = links.and_then(|l| l.asub_dynamic.as_ref()) {
                    apply_asub_dynamic_sub(instance, ds);
                }

                // C `subRecord.c:144`+`:147` / `aSubRecord.c:216-218`:
                //     status = fetch_values(prec);
                //     if (status == 0) status = do_sub(prec);
                // A failed input link means the subroutine does not run this cycle
                // — VAL (and aSub's VALA..VALU) freeze, and none of `do_sub`'s
                // alarms (BAD_SUB / SOFT at BRSV) or its `udf = isnan(val)` update
                // happen. Same one-shot flag the aSub bad-SNAM skip arms, consumed
                // Invoke the registered subroutine (sub/aSub SNAM) before the
                // record body, on the same dispatch path as process_local. The
                // framework owns the SubroutineFn registry (the record's own
                // process() is a no-op for sub/aSub), so without this the main
                // engine path — SCAN, event, CA-put-to-PP, FLNK — never ran the
                // subroutine and VAL/VALA..VALU/OUTA..OUTU never updated.
                instance.run_registered_subroutine()?;

                // Process
                let mut outcome = instance.record.process()?;
                // Merge deferred device actions into process outcome actions
                if let Some(deferred) = deferred_device_actions {
                    outcome.actions.extend(deferred);
                }
                let process_result = outcome.result;
                let process_actions = crate::server::record::ProcessActions::from(outcome.actions);
                let post_write_fields =
                    crate::server::record::CycleList::from(outcome.post_write_fields);
                // Captured before the `AsyncPendingNotify` `if let` below moves
                // `process_result`; consulted after the monitor epilogue to defer
                // the OUT/OEVT/FLNK tail (swait ODLY — see `CompleteDeferOutput`).
                let result_is_defer_output = matches!(
                    process_result,
                    crate::server::record::RecordProcessResult::CompleteDeferOutput
                );
                // Alarm-epilogue-only cycle (C `transformRecord.c:554-560`): the
                // alarm/timestamp commit below runs, the value side does not. See
                // `RecordProcessResult::CompleteAlarmOnly` and the `'epilogue`
                // break after `apply_timestamp`.
                let result_is_alarm_only = matches!(
                    process_result,
                    crate::server::record::RecordProcessResult::CompleteAlarmOnly
                );

                (
                    process_result,
                    process_actions,
                    post_write_fields,
                    result_is_defer_output,
                    result_is_alarm_only,
                )
            };

            if matches!(
                process_result,
                crate::server::record::RecordProcessResult::AsyncPending
            ) {
                // C `dbProcess` contract: when device support / record body
                // signals "async pending", `pact` MUST be true so subsequent
                // dbProcess attempts on the same record bail at the entry
                // guard. Previous Rust port assumed `process_local` had
                // already set it via the swap-true at function entry, but
                // this main path bypasses `process_local` and calls
                // `record.process()` directly — leaving `processing=false`.
                // Mirrors `aiRecord.c:122` and similar: `prec->pact = TRUE;
                // return 0;` before async work.
                guard.hold().enter_pact();
                guard.release();

                // PACT stays set; skip alarm/timestamp/snapshot/OUT/FLNK.
                // But still execute any actions (e.g., ReprocessAfter for delayed re-entry).
                self.execute_process_actions(name, rec, process_actions, visited);
                // After every action this arm runs, so the ordering rule holds
                // whatever the outcome carried. The `ReprocessAfter` a pending
                // cycle usually arms cannot overtake this: the continuation
                // enters through `process_record_continuation`, which acquires
                // the same per-record gate this body still holds.
                self.publish_post_write_fields(name, post_write_fields);
                // The SIM continuation released the SDLY PACT and the body then
                // went async again: still run the restart check, which finds the
                // record busy again and leaves the queue head where it is (the
                // deferral is closed under its own restart).
                self.apply_pact_exit(name, rec, cycle_end.take());
                return Ok(());
            }
            if matches!(
                process_result,
                crate::server::record::RecordProcessResult::CompleteNoEmit
            ) {
                guard.release();
                // C `compressRecord.c:365` `if (status != 1)`: the record
                // completed synchronously but emitted no new value this cycle
                // (a compress still accumulating toward its next compressed
                // sample). C runs none of `prec->udf = FALSE`,
                // `recGblGetTimeStamp`, `monitor`, nor `recGblFwdLink` — so the
                // entire value-publication epilogue (UDF clear / alarm commit /
                // timestamp / monitor / FLNK) is skipped. PACT is already clear
                // on this synchronous path (only the async branches set it), so
                // there is nothing to release. `complete_no_emit()` carries no
                // actions and compress is soft (no deferred device actions), so
                // there is nothing to run — return without awaiting
                // `execute_process_actions`, which would enlarge this hot
                // recursive function's async frame (the FLNK chain nests one
                // poll frame per hop, unbounded as in C; the write guard
                // `instance` is released on return).
                debug_assert!(
                    process_actions.is_empty(),
                    "CompleteNoEmit must carry no process actions"
                );
                // No actions means no link writes to order against, so the rule
                // is satisfied here; the arm still publishes, so the mechanism
                // has no arm-shaped hole.
                self.publish_post_write_fields(name, post_write_fields);
                // The record is idle (this path sets no PACT), so a notify queued
                // on a released SDLY window replays straight away.
                self.apply_pact_exit(name, rec, cycle_end.take());
                return Ok(());
            }
            if let crate::server::record::RecordProcessResult::AsyncPendingNotify(fields) =
                process_result
            {
                // Intermediate notification (e.g. DMOV=0 at move start).
                // Execute device write first so the move command reaches the
                // driver, then fire the record's link writes, then flush
                // DMOV=0 etc. to monitors. This mirrors the C ordering on an
                // async (pact=1) pass: `motorRecord.cc:1491` runs `do_work`
                // (the device move), `motorRecord.cc:1495` then fires
                // `dbPutLink(&pmr->rlnk, ...)` UNCONDITIONALLY — on every pass
                // including the move-start pass where DMOV just went 0 — and
                // only `motorRecord.cc:1507` afterwards calls `monitor()`. So
                // the requested `WriteDbLink`/`WriteDbLinkNotify` actions must
                // run on the pending cycle as well; a put processes a PP target
                // even when the value is unchanged, so dropping them changes
                // downstream process counts (motor RLNK, asyn async writes).
                // The forward link stays deferred: C runs `recGblFwdLink` only
                // when `pmr->dmov != 0` (motorRecord.cc:1509), i.e. on async
                // completion, not on this pending pass.
                // Guarded: device write, timestamp, and the changed-field
                // snapshot. The data guard is released before the link-write /
                // notify awaits below (parking_lot guards are `!Send`).
                guard.release();
                let tsel = self.read_tsel(rec);
                let snapshot = {
                    let mut instance = rec.write();
                    if !is_soft {
                        if let Some(mut dev) = instance.device.take() {
                            let _ = dev.write(&mut *instance.record);
                            instance.device = Some(dev);
                        }
                    }
                    let inst = &mut *instance;
                    tsel.stamp(&inst.name, &mut inst.common, is_soft);
                    // The pass's posts, through the owner this path shares with
                    // `RecordInstance::process_local`.
                    let changed_fields = instance.collect_notify_posts(fields);
                    // C parity (calcoutRecord.c:277-282, sCalcoutRecord.c:400-404):
                    // a record that defers its output by ODLY via a timer
                    // (`callbackRequestProcessCallbackDelayed`) keeps `pact=TRUE`
                    // across the whole delay — it `return 0`s with pact still set,
                    // so the record stays ACTIVE and a concurrent `dbProcess`
                    // bails; the delayed callback re-enters (`pact==TRUE`, `dlya`
                    // branch) and clears pact. Mirror that: when this notify
                    // schedules a `ReprocessAfter` (the continuation that clears
                    // PACT at the `is_continuation` arm below), hold PACT now.
                    //
                    // The gate is the `ReprocessAfter` itself, not a flag: holding
                    // PACT is sound ONLY because a continuation is scheduled to
                    // release it. A notify WITHOUT a `ReprocessAfter` (motor's
                    // DMOV-pulse pass, which completes via its device callback and
                    // returns Complete on later passes — no timer continuation)
                    // gets no PACT-clearing re-entry, so it must NOT hold PACT or
                    // it would stick forever (spurious SCAN_ALARM). Tying the hold
                    // to the presence of its own release keeps the invariant by
                    // construction and leaves motor's path untouched.
                    let holds_pact_until_continuation = process_actions.iter().any(|a| {
                        matches!(a, crate::server::record::ProcessAction::ReprocessAfter(_))
                    });
                    if holds_pact_until_continuation {
                        instance.enter_pact();
                    }
                    changed_fields
                };
                // Partition exactly as the synchronous Complete path: link
                // writes fire here (C `dbPutLink` precedes `monitor()`);
                // delayed-reprocess / device-command actions run after the
                // notify (the Complete path runs them after the FLNK tail,
                // which is deferred to async completion on this pending pass).
                let (link_writes, deferred_actions): (Vec<_>, Vec<_>) =
                    process_actions.into_iter().partition(|a| {
                        matches!(
                            a,
                            crate::server::record::ProcessAction::WriteDbLink { .. }
                                | crate::server::record::ProcessAction::WriteDbLinkNotify { .. }
                        )
                    });
                self.execute_process_actions(name, rec, link_writes, visited);
                self.publish_post_write_fields(name, post_write_fields);
                {
                    let inst = rec.read();
                    inst.notify_from_snapshot(&snapshot, link_backing);
                }
                self.execute_process_actions(name, rec, deferred_actions, visited);
                // Same as the `AsyncPending` arm: run the restart check through the
                // single drain owner, which is a no-op if this pass re-took PACT.
                self.apply_pact_exit(name, rec, cycle_end.take());
                return Ok(());
            }

            // Async-completion PACT clear for the `ReprocessAfter`
            // continuation path. C parity `dbAccess.c:583` —
            // `prset->process(precord)` for a record whose first cycle
            // returned async-pending is the *completion* re-entry; the
            // record support clears `pact` itself inside `process()`
            // (e.g. `aiRecord.c` second pass sets `prec->pact = FALSE`).
            //
            // A record that returns `AsyncPending` AND emits a
            // `ProcessAction::ReprocessAfter` is re-entered here via
            // `process_record_continuation` (`is_continuation == true`,
            // PACT entry guard skipped). Reaching this point means the
            // continuation's `process()` did NOT return async-pending
            // again (both async branches above return early), so the
            // async cycle is genuinely complete. The non-continuation
            // async-device path clears `processing` in
            // `complete_async_record_inner`; the continuation path has
            // no such callback, so without this clear `processing`
            // stays `true` forever — every later foreign
            // `process_record_with_links` then trips the PACT entry
            // guard, counts to MAX_LOCK, and raises a spurious
            // SCAN_ALARM. Clearing here (record still write-locked,
            // before the OUT/FLNK tail) mirrors the C ordering where
            // `pact` is already `FALSE` when `recGblFwdLink` runs.
            //
            // The release is carried to this cycle's `recGblFwdLink` tail below
            // as the `PactExit`, which is where C runs the restart check
            // (`recGbl.c:295` → `dbNotifyCompletion` → `restartCheck`).
            // Restarting at the `pact = FALSE` store instead — before the
            // OUT/FLNK tail — would let the replayed put process the record
            // concurrently with the tail it is still running.
            // dfanout's SELL read sits between `recGblGetTimeStamp` and
            // `checkAlarms` (`dfanoutRecord.c:126-127`), so it must run before
            // Segment C: a failed read is a `setLinkAlarm`, and the line after
            // it is the `nsev < INVALID_ALARM` test that decides between
            // `push_values` and the IVOA branch. Taken outside the write guard
            // below because the read takes its own locks. The owner ignores
            // every record whose C reads SELL elsewhere.
            if plan.reads_sell {
                guard.release();
                self.read_sell_into_seln(rec, super::links::SellPhase::BeforeAlarms);
            }

            // The TSEL half of this cycle's `recGblGetTimeStampSimm`, read here
            // because the store below happens under Segment C's data guard and
            // the link read cannot. It is the record's own input stage —
            // `fetch_values` / `readValue`, both already done — that C lets move
            // the TSEL source before this read, so reading it here and storing
            // it at the stamp point is C's order with the lock split out.
            let tsel = match Self::tsel_link(guard.hold()) {
                None => super::TselStamp::None,
                Some(link) => {
                    guard.release();
                    self.read_tsel_link(rec, link)
                }
            };

            // Segment C (guarded): the alarm / UDF / timestamp epilogue, the IVOA
            // output veto, and the output-time-link read list. Re-acquire the data
            // lock (Segments A/B committed their writes under their own guards).
            // On the alarm-only path this segment `break`s the whole `'epilogue`.
            let (restamps_after, skip_out, out_time_reads) = {
                let instance = guard.hold();
                // Folded into the guard the moment it is minted, and never
                // threaded onward by value: one carrier, so the exits between
                // here and the tail — the `?` on the device write, the
                // async-output `write_begin` early return, the `break 'epilogue`
                // — all release it without a site of their own.
                cycle_end.merge_in(if is_continuation {
                    instance.leave_pact()
                } else {
                    instance.pact_exit_without_release()
                });

                // NOTE: the MS-class input-link alarm propagation
                // (`inherit_sevr_msg`) already ran BEFORE the record body — see the
                // fold site above `set_process_context`. C raises it inside
                // `dbGetLink`, so the body must be able to read the resulting
                // `nsev` (transform IVLA="Do Nothing").

                // UDF update — C parity (aiRecord.c:285, calcRecord.c
                // checkAlarms, int64inRecord.c:144): clear UDF only when
                // this cycle produced a *defined* value. A NaN computed
                // value (calc divide-by-zero) or a failed link read that
                // left VAL un-updated must keep UDF true so the following
                // `recGblCheckUDF` raises UDF_ALARM at severity UDFS.
                //
                // This MUST run before `evaluate_alarms()` (which calls
                // `rec_gbl_check_udf`): C records set `prec->udf` inside
                // `process()` before `checkAlarms()` runs.
                //
                // The re-derive fires only when a value was actually SOURCED or
                // RECOMPUTED this cycle — the C invariant. Two record classes
                // reach it:
                //   * `clears_udf()` true: records whose C `process()` re-derives
                //     UDF UNCONDITIONALLY every cycle, whatever the read did
                //     (`aiRecord.c:161` `if(status==0) prec->udf = isnan(val)`,
                //     with a soft read's `status==2` folded to 0 — so a constant
                //     INP still re-derives). ai/ao/bi/longin/calc/mbbi… .
                //   * `device_did_compute`: a value was sourced this cycle — a
                //     real soft-channel INP read landed a value, or device
                //     support's `read()` computed one. This is how the
                //     sourced-only records (`clears_udf()` false: stringin, bo,
                //     longout, …) get their UDF cleared on a genuine read, exactly
                //     like C `devSiSoft.c::read_stringin` clears UDF only inside
                //     the `!dbLinkIsConstant` read branch.
                //
                // A cycle that sources nothing — e.g. a `caput UDF x` that drove
                // processing on a Passive record with a constant/empty INP — must
                // NOT re-derive UDF on a sourced-only record: the client's UDF put
                // stands (softIoc-verified: `caput REC.UDF 1` keeps UDF=1 for
                // stringin/lso/bo/longout, unlike ai/longin which re-derive to 0).
                // DOL-sourced output records clear UDF in their own DOL branch
                // above; the subroutine records (aSub) clear it in the subroutine
                // run (C `do_sub`), so neither needs `device_did_compute` here.
                //
                // …and it is gated on the READ STATUS, which is C's own shape:
                // `if (status == 0) prec->udf = <derive>` (aiRecord.c:161,
                // mbbiDirectRecord.c:155-164). A cycle whose soft INP read
                // failed sourced nothing, so it re-derives nothing and UDF
                // stands — that is what leaves `if (prec->udf) recGblSetSevr(
                // prec, UDF_ALARM, ...)` reachable. The array records and
                // compress are the documented exceptions
                // ([`Record::derives_udf_on_read_failure`]).
                //
                // A DEVICE read that produced no value is the same case and
                // takes the same arm: C's `-1`/`-2` returns miss `if(status==0)`
                // just as a failed soft read does, so the gate is one condition
                // over both sources rather than a per-record-type exception at
                // the ai site ([`DeviceReadStatus::read_failed`]).
                let derive_udf = if read_produced_no_value {
                    instance.record.derives_udf_on_read_failure()
                } else if device_read_computed {
                    // C `return 2` from a DEVICE dset. Whether the record
                    // re-derives on top of what the dset already wrote is the
                    // record's own rule and is not uniform: `aiRecord.c:158-161`
                    // folds 2 into 0 first and re-derives, `biRecord.c:136-141`
                    // and its four twins keep the assignment inside
                    // `if (status == 0)` and never reach it.
                    instance.record.rederives_udf_on_computed_read()
                } else {
                    plan.clears_udf || device_did_compute
                };
                if derive_udf {
                    instance.common.udf = instance.record.value_is_undefined() as u8;
                }

                // Per-record alarm hook — record-type-specific STATE / COS
                // / limit / SOFT alarms (C `checkAlarms()`). Records that
                // have migrated their alarm logic here raise into
                // `nsta`/`nsev`; the rest fall back to the framework's
                // centralised `evaluate_alarms` match below.
                {
                    let inst = &mut *instance;
                    inst.record.check_alarms(&mut inst.common);
                }

                // Evaluate alarms (accumulates into nsta/nsev)
                instance.evaluate_alarms();

                // Device support alarm/timestamp override
                if !is_soft {
                    let (dev_alarm, dev_ts, dev_utag) = if let Some(ref dev) = instance.device {
                        (dev.last_alarm(), dev.last_timestamp(), dev.last_utag())
                    } else {
                        (None, None, None)
                    };
                    if let Some((stat, sevr)) = dev_alarm {
                        use crate::server::recgbl::rec_gbl_set_sevr;
                        rec_gbl_set_sevr(
                            &mut instance.common,
                            stat,
                            crate::server::record::AlarmSeverity::from_u16(sevr),
                        );
                    }
                    if let Some(ts) = dev_ts {
                        instance.common.time = ts;
                    }
                    // C device support writes `prec->utag` directly during
                    // `read()` — the event-system pulse-id path, since
                    // `epicsTimeStamp` carries no tag. Adopt the device's
                    // userTag when it supplies one; read in the same `dev`
                    // borrow as the timestamp above so the time/tag pair is a
                    // single consistent device snapshot.
                    if let Some(utag) = dev_utag {
                        instance.common.utag = utag;
                    }
                }

                // The soft-channel half of the same override: for a `Soft
                // Channel` record the dset IS the device, and the timestamp it
                // supplies is the INP source's (`devAiSoft.c:59-60`). `None`
                // unless the read succeeded under C's TSE=-2 + constant-TSEL
                // gate, so a record that is not asking for device time, or
                // whose read failed, keeps whatever `apply_timestamp` gives it.
                let links = stage.links.as_ref();
                if let Some(ts) = links.and_then(|l| l.inp_source_time) {
                    instance.common.time = ts;
                }
                // The calc half of the same adoption (`lnkCalc.c:581`) — see
                // where `inp_source_utag` is built for why only that link
                // class supplies one.
                if let Some(tag) = links.and_then(|l| l.inp_source_utag) {
                    instance.common.utag = tag;
                }

                // pvalink `time=true` adopts the latched upstream timestamp
                // into the owning record. `external_link_time` returned
                // `None` unless the lset signalled the option, so a `Some`
                // here is the operator-requested remote timestamp: the remote
                // NT `timeStamp` while connected, or the disconnect-event time
                // while the subscription is down (pvxs `snap_time = e.time`,
                // adopted on the invalid read — `pvxs/ioc/pvalink_lset.cpp:268-270`).
                // Apply BEFORE `apply_timestamp` so the upstream value
                // survives the soft-channel TSE=0 default (`apply_timestamp`
                // would otherwise stamp wall-clock-now on top).
                if let Some((secs, ns, utag)) = links.and_then(|l| l.inp_link_remote_time) {
                    let secs = secs.max(0) as u64;
                    let ns = ns.max(0) as u32;
                    instance.common.time =
                        std::time::UNIX_EPOCH + std::time::Duration::new(secs, ns.min(999_999_999));
                    // adopt the upstream `timeStamp.userTag` alongside the
                    // time, mirroring pvxs PR-added `precord->utag = snap_tag`
                    // next to `precord->time = snap_time` in the `time=true`
                    // branch. The tag is already widened without sign
                    // extension by the lset; `0` when the source carries
                    // none. `apply_timestamp` never touches `utag`, so this
                    // survives regardless of the TSE branch below.
                    instance.common.utag = utag;
                    // Whether the adopted time SURVIVES is the record's
                    // declared TSE, not something to arrange here: pvxs writes
                    // `precord->time` and `precord->utag` and nothing else
                    // (`pvalink_lset.cpp:269-272`), so a `time=true` link needs
                    // `field(TSE,"-2")` for `recGblGetTimeStamp` to leave the
                    // pair alone — which is exactly what pvxs's own test
                    // database declares (`test/testpvalink.db:140,230`).
                    // Writing -2 here instead made the field report a value the
                    // database never declared.
                }

                // IVOA gate severity for a redirected SIMM output. C decides
                // `if (prec->nsev < INVALID_ALARM)` at the `writeValue` call
                // (aoRecord.c:197) using the severity `checkAlarms` produced —
                // BEFORE `writeValue` raises SIMM_ALARM. Snapshot the real
                // (pre-SIMM) pending severity here so a `SIMS=INVALID` never flips
                // the IVOA decision: with a finite, in-range VAL the IVOA veto must
                // NOT fire and C still writes OVAL to SIOL. For a non-simulated
                // record no SIMM_ALARM is raised below, so `nsev` here equals the
                // committed `sevr`, leaving the IVOA gate unchanged.
                let real_sev = instance.common.nsev;

                // SIMM simulation severity on a redirected OUTPUT record. C
                // `writeValue` raises `recGblSetSevr(prec, SIMM_ALARM, prec->sims)`
                // AFTER `checkAlarms` (aoRecord.c:196 -> :570 / boRecord.c:219 ->
                // :436), so a coincident limit/state alarm of equal severity keeps
                // its stat/amsg (set first; `rec_gbl_set_sevr` is strict-greater).
                // A simulated INPUT instead raises this inside
                // `check_simulation_mode` before its body, because `readValue`
                // precedes the body. Raised here (after the alarm hooks, before the
                // commit) it still folds into this cycle's committed SEVR.
                if let Some((_, sims, _)) = &sim_output {
                    let sev = crate::server::record::AlarmSeverity::from_u16(*sims as u16);
                    crate::server::recgbl::rec_gbl_set_sevr(
                        &mut instance.common,
                        crate::server::recgbl::alarm_status::SIMM_ALARM,
                        sev,
                    );
                }

                // Apply timestamp based on TSE. BEFORE the output stage: C
                // `aoRecord.c:190` stamps the record before `writeValue` "so it
                // will be up to date if any downstream records fetch it via TSEL".
                //
                // A `restamps_time_after_completion` record (sseq) restamps at the
                // very END of its completion instead — C `sseqRecord.c::asyncFinish`
                // posts VAL (`:474`) and runs `recGblFwdLink` (`:499`) BEFORE
                // `recGblGetTimeStamp` (`:501`). Skip the pre-output restamp here so
                // this cycle's VAL monitor carries the record's pre-update
                // timestamp; the deferred restamp after the forward-link tail
                // advances TIME for the BUSY post and the next cycle.
                //
                // mbbo/mbboDirect are a second exception: C `mbboRecord.c:210-221`
                // takes `else if (prec->udf) goto CONTINUE`, jumping PAST this
                // pre-output `recGblGetTimeStampSimm`. So a soft (sync) UDF
                // mbbo/mbboDirect never stamps here; TIME stays at the epoch until
                // VAL is defined. Only the SYNC first-pass stamp is skipped — the
                // async-completion re-entry (`complete_async_record_inner`) stamps
                // unconditionally, matching C's `if (pact)` re-stamp
                // (mbboRecord.c:256-258).
                let restamps_after = plan.restamps_time_after_completion;
                // Either way into C's `goto CONTINUE` skips the same
                // `recGblGetTimeStampSimm`: `else if (prec->udf)`
                // (mbboRecord.c:210) and the failed closed-loop DOL read
                // (mbboRecord.c:205) jump to the identical label.
                let skips_ts_undef = plan.skips_timestamp_when_undefined
                    && (instance.common.udf != 0 || links.is_some_and(|l| l.dol_read_failed));
                if !restamps_after && !skips_ts_undef {
                    let inst = &mut *instance;
                    tsel.stamp(&inst.name, &mut inst.common, is_soft);
                }
                // NOTE: UDF was already updated before `evaluate_alarms`
                // above — keyed on `value_is_undefined()` so a NaN result
                // keeps UDF true and UDF_ALARM is raised this cycle. Do
                // NOT clear UDF unconditionally here.

                // C `transformRecord.c:554-560` — the record body asked for the
                // ALARM epilogue only (IVLA="Do Nothing" on an INVALID input):
                // `recGblGetTimeStamp` + `checkAlarms` + `recGblResetAlarms` have
                // now run, and C `return`s here. Everything below is C's
                // `monitor()` + output + `recGblFwdLink()` — none of it happens on
                // that cycle. The SEVR/STAT/AMSG/ACKS posts `recGblResetAlarms`
                // itself makes are the only events the cycle emits; VAL and the
                // value fields are NOT posted and their last-posted trackers stay
                // put (C leaves `LA..LP` un-updated), so the next publishing cycle
                // re-detects the change.
                //
                // This is C's OTHER `recGblResetAlarms` call site — the record
                // body's own, not `monitor()`'s — and the cycle performs no output,
                // so the commit happens here and the path returns.
                if result_is_alarm_only {
                    // This path performs no output — it drops the cycle's
                    // actions by design — so a withheld store would have
                    // nothing to be ordered against and nothing to publish it.
                    debug_assert!(
                        post_write_fields.is_empty(),
                        "CompleteAlarmOnly runs no outputs and must carry no post-write fields"
                    );
                    let alarm_result =
                        crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);
                    let alarm_posts = alarm_field_posts(&instance.common, &alarm_result);
                    let snapshot = crate::server::record::ProcessSnapshot::new();
                    let posts = publish_cycle(instance, &snapshot, link_backing, alarm_posts);
                    break 'epilogue (
                        // No forward link of EITHER kind: the comment above is
                        // C's `return` before `recGblFwdLink`. The external
                        // half used to escape it, because the tail re-derived
                        // that half for itself out of the record instead of
                        // taking the answer this arm hands it.
                        crate::server::record::record_instance::ForwardTarget::None,
                        crate::server::record::ProcessActions::new(),
                        false,
                        restamps_after,
                        posts,
                    );
                }

                // **The IVOA owner** — the single site that decides what an INVALID
                // cycle does with its outputs, for EVERY output path of this
                // record: its own OUT, the SIOL redirect, the generic multi-output
                // pairs, and the dfanout `OUTn` push. Each of those consumes the
                // decision (`skip_out`, plus the IVOV the record has by then
                // stored in its own output field); none re-derives it.
                //
                // C makes the decision exactly once, BEFORE any output — at the
                // `writeValue` call (`if (prec->nsev < INVALID_ALARM)`,
                // aoRecord.c:197) and at dfanout's push (`dfanoutRecord.c:128`).
                // An output path that re-reads `nsev` after the writes have begun
                // reads an alarm the writes THEMSELVES raised (a failed put's
                // LINK_ALARM/INVALID, dbLink.c:444-446) and acts on a decision C
                // never made — e.g. overwriting VAL with IVOV on a cycle whose only
                // INVALID came from the failed push.
                //
                // Gate on the real (pre-SIMM) severity `real_sev` snapshotted above
                // — C decides IVOA before `writeValue` raises SIMM_ALARM, so a
                // `SIMS=INVALID` simulation severity does not trigger the veto (the
                // committed `sevr` may be INVALID from SIMM while the record's own
                // alarm is not).
                // The cycle drives no outputs when the type has no output stage
                // (`ProcessPlan::output_stage`: C `calcRecord.c::process` has no
                // OUT lines) or IVOA vetoes them on an INVALID cycle.
                let skip_out = if !plan.output_stage {
                    true
                } else if real_sev == crate::server::record::AlarmSeverity::Invalid {
                    let ivoa = instance
                        .record
                        .get_field("IVOA")
                        .and_then(|v| v.to_menu_index())
                        .unwrap_or(0);
                    match ivoa {
                        1 => true, // Don't drive outputs
                        2 => {
                            // Set output to IVOV. Each record type knows
                            // which field its OUT writeback consumes — see
                            // [`Record::apply_invalid_output_value`]. The
                            // earlier path special-cased `calcout`
                            // (OVAL) and fell back to `set_val` (VAL) for
                            // every other record. That hid a real bug:
                            // ao/lso/bo/mbbo/busy left their OVAL/RVAL
                            // staging field stale, so the OUT writeback —
                            // which reads `OVAL.or(VAL)` — sent the
                            // pre-IVOA value to the linked record. Per-type
                            // overrides now apply IVOV to the field that
                            // matches the C convention.
                            // C's IVOA=2 arm cannot fail. It is a plain store into the
                            // record's own fields — `prec->val = prec->ivov`
                            // plus the mask conversion (`boRecord.c:231-238`),
                            // `strncpy(prec->val, prec->ivov, sizv-1)` plus
                            // `len` (`lsoRecord.c:131-137`) — with C's only
                            // failure arm reserved for an ILLEGAL IVOA choice
                            // (`boRecord.c:241-244`), which this `match` has
                            // already excluded. So an `Err` here is a port bug
                            // in the record's `apply_invalid_output_value` /
                            // `put_field` pair, never a runtime condition, and
                            // discarding it silently is what let lso's arm be a
                            // complete no-op for a whole round: `put_field` had
                            // no `"OVAL"` case, so the `?` returned
                            // `FieldNotFound` before VAL was ever written and
                            // the record kept its stale value with no monitor.
                            // Loud in test/debug; release behaviour unchanged,
                            // because C has no alarm for this case to copy.
                            if let Some(ivov) = instance.record.get_field("IVOV") {
                                let applied = instance.record.apply_invalid_output_value(ivov);
                                debug_assert!(
                                    applied.is_ok(),
                                    "{}: IVOA=Set_output_to_IVOV could not apply IVOV: {:?}",
                                    instance.record.record_type(),
                                    applied.err()
                                );
                            }
                            false
                        }
                        _ => false, // Continue normally
                    }
                } else {
                    false
                };

                // Output-time input links (swait DOL). C
                // `swaitRecord.c::execOutput` (763-772) fetches DOL through
                // `recDynLinkGet` at OUTPUT time — not in the input-fetch phase —
                // and only on a cycle whose output actually fires, so DOLD carries
                // the value the link holds at the moment of the write (ODLY
                // delay-end included) and a non-firing cycle neither refreshes nor
                // posts it. Run here, after the IVOA veto and before the OUT stage
                // composes `out_info`, so the fresh value is the one written and
                // the changed field still reaches this cycle's snapshot.
                //
                // The write lock is released across the read (the link may target
                // another record) and re-taken, the same way the pre-process
                // `ReadDbLink` stage above does it; the record stays claimed by the
                // `processing` guard meanwhile.
                let out_time_reads: Option<Vec<(String, &'static str)>> = if skip_out {
                    None
                } else {
                    let out_time_links = instance.record.output_time_input_links();
                    if !out_time_links.is_empty() && instance.record.should_output() {
                        Some(
                            out_time_links
                                .iter()
                                .filter_map(|(link_field, value_field)| {
                                    Some((instance.link_text(link_field)?, *value_field))
                                })
                                .collect(),
                        )
                    } else {
                        None
                    }
                };

                (restamps_after, skip_out, out_time_reads)
            };

            // await 2 (guard-free): output-time input-link (swait DOL) reads. The
            // write lock is released across the reads (a link may target another
            // record); the record stays claimed by the `processing` gate.
            let mut out_time_fetched: Option<Vec<(&'static str, EpicsValue)>> = None;
            if out_time_reads.is_some() {
                guard.release();
            }
            if let Some(out_time_reads) = out_time_reads {
                for (link, value_field) in out_time_reads {
                    // A bare read, no `process_passive_db_source`: C's DOL is a
                    // `recDynLink` (CA-style) input, which never process-passives its
                    // source. `NoData` (constant DOL) writes nothing — the value field
                    // keeps what it holds, as in C where a swait DOL that is not a PV
                    // name never registers a recDynLink and so never delivers.
                    let parsed = crate::server::record::parse_link_v2(&link);
                    if let Some(value) = self.db_try_get_link(rec, &parsed).value() {
                        out_time_fetched
                            .get_or_insert_with(Vec::new)
                            .push((value_field, value));
                    }
                }
            }

            // Segment D (guarded): apply the output-time reads, queue OEVT, compose
            // the OUT-stage `out_info` plan, and capture the OUT-link source fields.
            // Yields those; the guard then closes so the output-write awaits below
            // hold no `!Send` guard (a self/cyclic OUT link would also dead-lock the
            // non-reentrant gate). The async device-write branch inside the
            // `out_info` match returns straight from the function.
            // The cycle's link-carried writes, split off the record's other
            // actions; `None` when it has none, which is the usual cycle.
            let (link_writes, process_actions): (
                Option<Vec<_>>,
                crate::server::record::ProcessActions,
            ) = if process_actions.is_empty() {
                (None, process_actions)
            } else {
                let (writes, rest): (Vec<_>, Vec<_>) = process_actions.into_iter().partition(|a| {
                    matches!(
                        a,
                        crate::server::record::ProcessAction::WriteDbLink { .. }
                            | crate::server::record::ProcessAction::WriteDbLinkNotify { .. }
                    )
                });
                ((!writes.is_empty()).then_some(writes), rest.into())
            };
            // Whether this cycle has an output stage at all — the one rule that
            // decides both the output segment and the guard release it needs.
            // It is the union of every output kind the segment below can
            // perform; each of its dispatchers is a no-op under the negation,
            // so a cycle without an output (a stock `calc`: no OUT stage, no
            // simulation, no write actions) runs none of them, and reads
            // nothing for a `OutLinkSrc` it would hand to no one.
            let has_output = !skip_out
                || plan.multi_output_dispatch
                || sim_output.is_some()
                || link_writes.is_some()
                || !post_write_fields.is_empty();
            let dispatched = if !has_output {
                super::links::MultiOutDispatch::default()
            } else {
                let (out_info, src_putf, src_notify, src_alarm) = {
                    let instance = guard.hold();
                    if let Some(out_time_fetched) = out_time_fetched {
                        for (field, value) in out_time_fetched {
                            let _ = instance.record.put_field(field, value);
                        }
                    }

                    // OEVT: queue the output event when the output fires — the
                    // event-subsystem twin of the OUT write, gated by the SAME IVOA
                    // Don't_drive veto (`skip_out`). C
                    // `calcout`/`sCalcout`/`aCalcout` `execOutput` posts
                    // `postEvent(epvt)` / `post_event(oevt)` right after `writeValue`
                    // in every OUT-driving branch and never on Don't_drive;
                    // `output_event()` folds in the record's own OOPT/calc-fail/ODLY
                    // output-fire decision. Spawned (not inline) like
                    // `dispatch_event_record` so the woken `SCAN="Event"` records run
                    // on the callback path, not recursively inside this cycle.
                    if !skip_out {
                        if let Some(event_name) = instance.record.output_event() {
                            let db = self.clone();
                            // Middle band, not this record's PRIO: C `postEvent`
                            // fires one `callbackRequest` per non-empty band and
                            // each carries the *scanned* record's priority
                            // (`dbScan.c:513-527`), a fan-out the port's single
                            // Event list cannot express (`scan_index.rs`
                            // `post_event_named`). The poster's own PRIO is not
                            // the answer, so this keeps `callbackRequest`'s
                            // general band (`callback.h:42`).
                            crate::runtime::task::spawn_background(
                                crate::runtime::task::CallbackPriority::Medium,
                                async move {
                                    db.post_event_named(&event_name).await;
                                },
                            );
                        }
                    }

                    // OUT stage: soft channel -> link put, non-soft -> device.write()
                    // Must run BEFORE check_deadband_ext so MLST is not prematurely
                    // updated for async writes that return early.
                    let out_info = if sim_output.is_some() {
                        // Simulated OUTPUT record: C `writeValue` redirects the output
                        // to SIOL (`dbPutLink(&prec->siol, ..., &prec->oval)`) INSTEAD
                        // of the real device write / soft OUT-link write. The redirect
                        // is applied from the OUT epilogue by `write_simulated_output_siol`
                        // (it reads the post-body OVAL/RVAL), so the normal device/OUT
                        // write is suppressed here.
                        None
                    } else if sim_write_aborted {
                        // C `writeValue` returned before writing — either the
                        // `default:` arm (`recGblSetSevr(SOFT_ALARM, INVALID_ALARM);
                        // status = -1;`) or a failed SIML read. Both return BEFORE the
                        // device write and BEFORE the SIOL redirect, so this cycle
                        // performs no output at all.
                        None
                    } else if skip_out {
                        None
                    } else {
                        let can_dev_write = instance.record.can_device_write();
                        // The soft OUT-link value THIS DTYP's dset would put — VAL/OVAL for
                        // "Soft Channel", RVAL for "Raw Soft Channel". `None` = not a soft
                        // output dset. See `RecordInstance::soft_output_value`.
                        let soft_out = instance.soft_output_value();
                        let record_should_output = instance.record.should_output();
                        if !can_dev_write {
                            // Non-output records (calcout, etc.) may still have a
                            // soft OUT link (DB or external ca://`/`pva://`).
                            // Write OVAL to OUT when the record says should_output().
                            if record_should_output && instance.parsed_out.is_writable_out_link() {
                                let out_val = instance.record.output_link_value();
                                out_val.map(|v| (instance.parsed_out.clone(), v))
                            } else {
                                None
                            }
                        } else if let Some(out_val) = soft_out {
                            if !record_should_output {
                                // epics-base 7.0.8 OOPT: gate the soft OUT-link
                                // write on the record's `should_output()`. For
                                // longout/calcout with OOPT != 0 this lets a
                                // condition-not-met cycle silently skip the link
                                // write without disturbing alarms / monitors.
                                None
                            } else if instance.parsed_out.is_writable_out_link() {
                                out_val.map(|v| (instance.parsed_out.clone(), v))
                            } else {
                                None
                            }
                        } else if device_callback
                            && instance
                                .device
                                .as_ref()
                                .is_some_and(|d| d.output_callback_readback())
                        {
                            // Driver-callback (`asyn:READBACK`) cycle on a hardware output
                            // whose device support takes the callback-readback branch: the
                            // new value was read back into VAL by the read stage above;
                            // writing it here would re-assert the setpoint to the driver and
                            // re-trigger it (the AD `Acquire` loop). C
                            // `devAsynInt32.c::processBo` takes the `newOutputCallbackValue`
                            // readback branch and never calls `processCallbackOutput`'s
                            // `write()` on a callback cycle. Devices without that contract
                            // (`output_callback_readback` false — devMotorAsyn) run their
                            // output stage on callback cycles like any other C `dbProcess`:
                            // the motor record's retry / backlash / NTM-stop commands are
                            // emitted on exactly these passes.
                            None
                        } else if !record_should_output {
                            // OOPT gating for hardware outputs (longout DTYP=...).
                            // Skip the device write when the OOPT predicate is
                            // not satisfied; the record's val/timestamp/snapshot
                            // path still runs so monitor consumers see the value
                            // change even on a non-output cycle.
                            None
                        } else {
                            if let Some(mut dev) = instance.device.take() {
                                // Try async write_begin() first
                                use crate::server::device_support::WriteStart;
                                match dev.write_begin(&mut *instance.record) {
                                    Ok(WriteStart::Pending(completion)) => {
                                        // Async write submitted -- set PACT, return early.
                                        // complete_async_record will handle deadband, snapshot,
                                        // notification, and FLNK when the write completes.
                                        instance.enter_pact();
                                        instance.device = Some(dev);
                                        let rec_name = instance.name.clone();
                                        let timeout = std::time::Duration::from_secs(5);
                                        let db = self.clone();
                                        let prio = instance.common.callback_priority();
                                        crate::runtime::task::spawn_background(prio, async move {
                                            // The write's outcome travels to the
                                            // completing pass, which raises the
                                            // WRITE alarm the synchronous branch
                                            // below raises in place — C carries
                                            // it as `pPvt->result.status` from
                                            // `processCallbackOutput` to the
                                            // record's `process()` re-entry
                                            // (devAsynFloat64.c:668). A wait the
                                            // pool never ran is an unknown
                                            // outcome, reported the same way.
                                            let outcome =
                                                crate::runtime::task::spawn_blocking_background(
                                                    prio,
                                                    move || completion.wait(timeout),
                                                )
                                                .await
                                                .unwrap_or_else(|e| {
                                                    Err(CaError::Protocol(format!(
                                                        "device write completion not awaited: {e}"
                                                    )))
                                                });
                                            let _ = db
                                                .complete_async_record_with_outcome(
                                                    &rec_name, outcome,
                                                )
                                                .await;
                                        });
                                        // Not an end: `complete_async_record_inner`
                                        // owns this cycle's tail now, and mints its own
                                        // token from the record when the write lands.
                                        cycle_end.hand_off_to_async_completion();
                                        return Ok(());
                                    }
                                    // The value is at the device; the cycle goes on
                                    // as after a synchronous write.
                                    Ok(WriteStart::Completed) => {}
                                    Ok(WriteStart::Synchronous) => {
                                        if let Err(e) = dev.write(&mut *instance.record) {
                                            eprintln!(
                                                "device write error on {}: {e}",
                                                instance.name
                                            );
                                            // C device support raises the write failure
                                            // through `recGblSetSevr` (a PENDING alarm),
                                            // and `process()`'s `monitor()` commits it in
                                            // the same cycle — the commit now follows this
                                            // output stage, so the pending raise is what
                                            // reaches SEVR/STAT (a direct `stat`/`sevr`
                                            // poke would be overwritten by the commit).
                                            crate::server::recgbl::rec_gbl_set_sevr(
                                                &mut instance.common,
                                                crate::server::recgbl::alarm_status::WRITE_ALARM,
                                                crate::server::record::AlarmSeverity::Invalid,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "device write_begin error on {}: {e}",
                                            instance.name
                                        );
                                        crate::server::recgbl::rec_gbl_set_sevr(
                                            &mut instance.common,
                                            crate::server::recgbl::alarm_status::WRITE_ALARM,
                                            crate::server::record::AlarmSeverity::Invalid,
                                        );
                                    }
                                }
                                instance.device = Some(dev);
                            }
                            None
                        }
                    };

                    // PUTF / put-notify wait-set / source alarm for every write of this
                    // cycle. C `dbDbPutValue` (dbDbLink.c:382-383) inherits the source's
                    // PENDING alarm (`psrce->nsta/nsev/namsg`) — this is the point in the
                    // cycle C reads them, before the commit. Captured under the Segment-D
                    // guard, which then closes.
                    let src_putf = instance.common.putf;
                    let src_notify = instance.notify.clone();
                    let src_alarm = super::links::LinkAlarm::pending(&instance.common);
                    (out_info, src_putf, src_notify, src_alarm)
                };

                // C `writeValue` reaches `conditional_write` — whose epilogue
                // advances PVAL — on every cycle except the three that return
                // before the switch: SIMM simulation (`longoutRecord.c:411-424`
                // redirects to SIOL), a failed SIML read or a bad SIMM
                // (`:400-403`, `:428-430`), and the IVOA Don't_drive veto, which
                // skips the `writeValue` call site altogether (`:169-171`).
                let reached_conditional_write =
                    sim_output.is_none() && !sim_write_aborted && !skip_out;

                // C `process()` runs every output of the cycle BEFORE `monitor()`,
                // and `monitor()` is where `recGblResetAlarms` commits the cycle's
                // alarm (aoRecord.c:196-232 → aoRecord.c `monitor`). A failed
                // `dbPutLink` raises LINK_ALARM/INVALID from INSIDE the put
                // (`setLinkAlarm`, dbLink.c:434-448) — so the write alarm must land
                // in THIS cycle's committed SEVR and this cycle's monitor posts,
                // not the next one. Every link-carried output of the cycle
                // therefore runs here, before the commit below:
                //
                //   * the soft OUT link (`out_info`),
                //   * the record's multi-output pairs (scalcout / acalcout OUT),
                //   * the SIMM SIOL redirect,
                //   * the record's own `WriteDbLink` actions (transform OUTn,
                //     scaler COUTP, throttle OUT — C writes them before
                //     `monitor()`/`recGblFwdLink` too).
                //
                // The record's write gate is released across the writes (a
                // self/cyclic OUT link would otherwise dead-lock on the
                // non-reentrant gate, exactly as the FLNK tail already runs
                // unlocked) and re-acquired for the commit. The put owner raises
                // the LINK_ALARM on the record itself, so nothing has to be
                // threaded back here.
                // await 3 (guard-free): the cycle's link-carried outputs run with the
                // data guard released (the put owner raises any LINK_ALARM on the
                // record itself). SEG E re-acquires for the alarm commit.
                // The boundary rule (see `DataGuard`): the guard is released only
                // when this cycle has an output to perform — every kind below may
                // lock another record, or this one through a cyclic link. An
                // un-skipped output stage counts whatever it turns out to write:
                // its dispatchers read the record to decide.
                guard.release();
                let src = super::links::OutLinkSrc {
                    putf: src_putf,
                    notify: src_notify.as_ref(),
                    alarm: &src_alarm,
                    field: "OUT",
                };
                if let Some((ref link, ref out_val)) = out_info {
                    self.write_out_link_value(rec, link, out_val.clone(), src, visited);
                }
                // C `longoutRecord.c:492-493`, OUTSIDE `if (doDevSupWrite)`:
                // the OOPT reference advances on a suppressed cycle too, which
                // is the only reason a transition can ever be detected.
                if reached_conditional_write && plan.redecides_after_output {
                    rec.write().record.after_output_decision();
                }
                self.dispatch_multi_output_values(rec, src, skip_out, plan, visited);
                // The value-putting multi-output records — dfanout `OUTn`, seq
                // `LNKn` — push HERE, with the record's other outputs, so the
                // whole output stage sits between `checkAlarms` and the alarm
                // commit exactly as C's does (`dfanoutRecord.c:128-146`
                // push_values → monitor; `seqRecord.c:264` dbPutLink →
                // asyncFinish's `recGblResetAlarms`, :227). A failed put's
                // LINK_ALARM therefore folds into THIS cycle's committed SEVR,
                // and the push reads the VAL the IVOA owner already settled.
                // The fanout dispatch stays in the forward-link tail: its
                // `LNKn` are `DBF_FWDLINK` (dbScanFwdLink), driving no value.
                let dispatched = if plan.multi_output_dispatch {
                    self.dispatch_multi_output(
                        rec,
                        super::links::MultiOutPhase::Output { skip_out },
                        visited,
                    )
                } else {
                    super::links::MultiOutDispatch::default()
                };
                self.write_simulated_output_siol(rec, &sim_output, skip_out, src, visited);
                if let Some(link_writes) = link_writes {
                    self.execute_process_actions(name, rec, link_writes, visited);
                }
                // Every link-carried output of the cycle has now run, so the
                // withheld stores become visible here — still ahead of Segment
                // E, which therefore change-detects against the published value
                // and does not post it a second time.
                self.publish_post_write_fields(name, post_write_fields);
                dispatched
            };

            // The seq record armed its delayed group chain: C `process` has
            // set `pact = TRUE` and returned through `processNextLink`
            // (`seqRecord.c:143`, `:196`), so THIS cycle commits nothing. The
            // alarm/timestamp/monitor/FLNK epilogue is `asyncFinish`'s
            // (`:219-241`), reached from the chain's last hop via
            // `complete_async_record`. Same shape as the `AsyncPending` arm
            // above; PACT was set by the dispatch before it spawned, so the
            // chain cannot complete ahead of it.
            if dispatched.went_async {
                guard.release();
                self.execute_process_actions(name, rec, process_actions, visited);
                self.apply_pact_exit(name, rec, cycle_end.take());
                return Ok(());
            }
            let push_alarm = dispatched.alarm;

            // Segment E (guarded): commit alarms, build the snapshot, resolve the
            // FLNK target, and yield the `'epilogue` tuple. Re-acquire the data lock.
            let instance = guard.hold();
            if let Some((stat, sevr)) = push_alarm {
                crate::server::recgbl::rec_gbl_set_sevr(&mut instance.common, stat, sevr);
            }

            // C `monitor()` with its opening `recGblResetAlarms` — AFTER every
            // output of the cycle, so a failed put's LINK_ALARM is committed
            // here and no async write advances MLST/ALST before it returns.
            let outcome = instance.monitor_cycle();

            let flnk_name = instance.forward_target();

            // Put-notify completion is NOT fired here. Firing before the
            // OUT/FLNK/process-action tail (below) would report the
            // WRITE_NOTIFY done while the chain it triggers — including
            // an async FLNK target — is still running (C `dbNotify.c`
            // keeps the originating record in the waitList until the
            // chain settles). The originating record instead `leave`s
            // the wait-set at the END of this function, after every PP
            // target it drives has joined. See `complete_put_notify`
            // at the tail.

            // 3. Notify subscribers, still under the segment's own guard.
            let posts = publish_cycle(
                instance,
                &outcome.snapshot,
                link_backing,
                outcome.alarm_posts,
            );

            (
                flnk_name,
                process_actions,
                result_is_defer_output,
                restamps_after,
                posts,
            )
        };

        // C `swaitRecord.c::process` (lines 425-481): `schedOutput` armed the
        // ODLY watchdog (`async=TRUE`), so `process` ran `monitor()` — the
        // value-publication epilogue above just posted VAL + the alarm fields at
        // the START of the delay — but SKIPPED the `if(!async){recGblFwdLink;
        // pact=FALSE;}` tail. The OUT write / OEVT are already gated out this
        // cycle by `should_output()==false`; `recGblFwdLink` is NOT
        // should_output-gated, so the forward-link tail below is skipped when
        // deferring (`result_is_defer_output`). The deferred `execOutput` — the
        // scheduled `ReprocessAfter` reprocess at delay-END — runs the OUT write
        // + OEVT + FLNK. Hold PACT across the wait so a foreign `dbProcess` bails
        // at the entry guard (C keeps the record ACTIVE on the watchdog,
        // swaitRecord.c:716); the hold is gated on the `ReprocessAfter` that
        // releases it (the same by-construction invariant as the
        // `AsyncPendingNotify` ODLY defer above). The `ReprocessAfter` itself is
        // dispatched by the shared deferred-actions site at the tail, NOT a
        // separate `execute_process_actions().await` here — adding one would
        // enlarge this hot recursive function's async frame (see the
        // `CompleteNoEmit` note above; it overflowed the stack in the deep-chain
        // tests).
        // Holding `processing=true` also makes the tail's putf-clear (gated on
        // `!is_processing()`) a no-op, leaving putf for the continuation.
        if result_is_defer_output {
            let holds_pact_until_continuation = process_actions
                .iter()
                .any(|a| matches!(a, crate::server::record::ProcessAction::ReprocessAfter(_)));
            if holds_pact_until_continuation {
                guard.hold().enter_pact();
            }
        }

        // 4.5 - 7. Multi-output / event / generic-multi-out / FLNK /
        // CP / RPRO tail. Shared with the simulation-mode path so a
        // simulated record runs the exact same `recGblFwdLink`
        // equivalent (C `aiRecord.c:168`).
        //
        // Skipped on a `CompleteDeferOutput` (swait ODLY) delaying cycle: the
        // multi-output / OEVT are already gated out by `should_output()==false`,
        // and `recGblFwdLink` runs only at delay-END (C `execOutput`) — the
        // continuation drives the whole tail. The deferred-actions site below
        // still runs (it dispatches this cycle's `ReprocessAfter`).
        if !result_is_defer_output {
            self.run_forward_link_tail_with_putf(
                name,
                &mut guard,
                &flnk_name,
                TailCtx { posts, plan },
                visited,
            );
        }

        // Deferred restamp for a `restamps_time_after_completion` record (sseq):
        // C `sseqRecord.c::asyncFinish` calls `recGblGetTimeStamp` (`:501`)
        // AFTER the VAL post (`:474`) and `recGblFwdLink` (`:499`). The VAL
        // monitor + forward link above therefore carried the record's
        // pre-update timestamp; restamp now so TIME advances for the following
        // BUSY post (sseq's out-of-band `post_fields`) and the next cycle. Soft
        // record (no device support), so `apply_timestamp` resolves TSE→TIME
        // the same as the pre-output site it replaces.
        if restamps_after {
            // Its own TSEL read, not the one Segment C took: C's stamp here is
            // a whole `recGblGetTimeStamp` running AFTER `recGblFwdLink`, so a
            // `.TIME` TSEL adopts whatever the forward-link chain just did to
            // its source.
            guard.release();
            self.rec_gbl_get_time_stamp(rec);
        }

        // 8. Execute the deferred ProcessActions after the FLNK tail:
        // `ReprocessAfter` schedules a later reprocess (the current
        // cycle's FLNK must proceed first) and `DeviceCommand` posts its
        // own monitors after this cycle's snapshot. The record's link writes
        // are NOT here — they ran pre-commit with the rest of the cycle's
        // output (C `transformRecord.c:605-621` / `scalerRecord.c:457-480`
        // put before `monitor()` + `recGblFwdLink()`), so a downstream FLNK
        // target still reads the freshly written value.
        if !process_actions.is_empty() {
            guard.release();
            self.execute_process_actions(name, rec, process_actions, visited);
        }

        // 9. C `recGbl.c::recGblFwdLink:302` clears `putf = FALSE` at the
        // tail of every synchronous process cycle, NOT just on the
        // foreign-entry path. When this record was driven through an
        // OUT-link propagation (write_db_link_value set our putf), the
        // target record's own process cycle must clear it before
        // returning — same lifecycle as the source record's PUTF
        // (which `put_record_field_from_ca` separately clears at the
        // foreign-entry boundary, and the async branch clears in
        // `complete_async_record_inner`). Async-pending records skip
        // this clear: their FLNK / putf-clear happens later in
        // `complete_async_record_inner` once the device round-trip
        // completes.
        // The guard holds both releases — `check_simulation_mode`'s SDLY/SIM
        // continuation and the `is_continuation` arm's — merged. At most one of
        // them can carry the parked put. Taking it here disarms the guard, so
        // the release happens once whether the cycle reaches this line or leaves
        // by one of the exits above.
        // The fetch buffers back to the chain, from exactly the cycle that
        // took them — the gates are the same facts the takes were: an input
        // stage exists only past `fetch_input_stage`'s take, and a set-link
        // list has capacity only if `read_into` took the buffer.
        finish_cycle(guard.hold());
        guard.release();
        self.apply_pact_exit(name, rec, cycle_end.take());

        Ok(())
    }

    /// The end of a synchronous process cycle — C `recGblFwdLink`'s tail
    /// (`recGbl.c:295-302`), after `dbScanFwdLink`:
    ///
    /// ```c
    /// if (pdbc->ppn) dbNotifyCompletion(pdbc);  /* leave the wait-set; queue the restart */
    /// ...
    /// pdbc->putf = FALSE;
    /// ```
    ///
    /// The single owner of both halves, so no cycle end can skip them. Open-coded
    /// at the tail of `process_record_with_links_inner` alone, it was jumped over
    /// by the two simulation early-returns: a put-notify on a SIMM record never
    /// left its wait-set (the callback never fired) and PUTF leaked into the next
    /// scan.
    fn end_process_cycle(&self, name: &str, rec: &Arc<RecordCell>, exit: PactExit) {
        finish_cycle(&mut rec.write());
        self.apply_pact_exit(name, rec, exit);
    }

    /// C `restartCheck` (`dbNotify.c:149-170`), reached from
    /// `dbNotifyCompletion` (`:445-475`) via `recGblFwdLink` (`recGbl.c:295`)
    /// at the tail of the cycle that released the record.
    ///
    /// **The single owner of the restart-list drain.** Every cycle end routes
    /// through it — including cycles that released no PACT, because a notify
    /// queued behind an in-flight wait-set on an idle record is freed by
    /// `complete_put_notify` above, not by a PACT release.
    ///
    /// Queued, not recursed — the same `scanOnce` shape as the RPRO restart.
    /// The pop itself happens inside `restart_next_notify_put`, under the
    /// record's advisory write gate, so a client put racing this spawn cannot
    /// take the record between the pop and the replay and thereby overtake a
    /// notify that has been waiting longer.
    ///
    /// `rec` is the record the restart re-enters. It is a parameter, and not a
    /// `get_record(name)` inside, because the consumer must be free to read the
    /// record: every caller must therefore already have let the record's DATA
    /// lock go, which a handle in hand makes visible at the call and a name
    /// lookup would hide. `parking_lot::RwLock` is not reentrant, so a caller
    /// still holding `rec.write()` would deadlock, not fail.
    pub(super) fn apply_pact_exit(&self, name: &str, _rec: &Arc<RecordCell>, exit: PactExit) {
        // NO record lock here, deliberately. This runs from cycle tails and
        // from a `Drop` that can fire while a `rec.write()` guard is still
        // alive in the same scope; parking_lot is not reentrant, so a read
        // here would deadlock on drop order. The bit was minted under the
        // releasing site's own lock instead — see `PactExit`.
        if !exit.restart_pending() {
            return;
        }
        let db = self.clone();
        let put_name = name.to_string();
        // C pins every put-notify callback to the low band —
        // `callbackSetPriority(priorityLow, &pnotifyPvt->callback)`
        // (`dbNotify.c:131`) — regardless of the record's PRIO.
        crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Low,
            async move {
                db.restart_next_notify_put(&put_name).await;
            },
        );
    }

    /// Forward-link / CP / RPRO tail for the simulation-mode path.
    ///
    /// C `aiRecord.c:151-168`: a record in SIMM mode handles the value
    /// inside `readValue()`, then `process()` still runs `monitor` +
    /// `recGblFwdLink(prec)`. The simulation path in
    /// `process_record_with_links_inner` does its own monitor posting,
    /// so this drives the forward-link / CP / RPRO tail that
    /// `recGblFwdLink` would. `flnk_name` (with its PUTF) is derived
    /// fresh from the record (a simulated cycle does not change FLNK,
    /// and SIOL reads/writes do not carry a foreign PUTF into the
    /// chain).
    fn run_forward_link_tail(
        &self,
        name: &str,
        rec: &Arc<RecordCell>,
        posts: CyclePosts,
        visited: &mut ProcStack,
    ) {
        let flnk_name = rec.read().forward_target();
        let plan = rec.process_plan();
        let mut guard = DataGuard::new(rec);
        self.run_forward_link_tail_with_putf(
            name,
            &mut guard,
            &flnk_name,
            TailCtx { posts, plan },
            visited,
        );
    }

    /// Steps 4.5 - 7 of the process chain: multi-output dispatch,
    /// event-record posting, generic OUTA..OUTP links, FLNK forward
    /// link, CP-target dispatch, and RPRO reprocess. Shared by the
    /// main process path and the simulation-mode path so both run the
    /// identical `recGblFwdLink` equivalent.
    fn run_forward_link_tail_with_putf(
        &self,
        name: &str,
        guard: &mut DataGuard<'_>,
        flnk: &crate::server::record::record_instance::ForwardTarget,
        src: TailCtx<'_>,
        visited: &mut ProcStack,
    ) {
        let rec = guard.rec;
        // 4.5. Multi-output dispatch, forward-link phase: fanout only. Its
        // `LNK0..LNKF` are `DBF_FWDLINK` — `dbScanFwdLink`, no value, no put
        // status, so the tail is where they belong. dfanout `OUTn` and seq
        // `LNKn` carry a value through `dbPutLink` and dispatch pre-commit in
        // `process_record_with_links_inner`, so a failed put's LINK_ALARM
        // folds into the same cycle's SEVR; the `ForwardLink` phase argument
        // skips them here (`multi_out_phase_of`).
        if src.plan.multi_output_dispatch {
            guard.release();
            let _ =
                self.dispatch_multi_output(rec, super::links::MultiOutPhase::ForwardLink, visited);
        }

        // 4.55. event record: post the named software event.
        if src.plan.posts_software_event {
            guard.release();
            self.dispatch_event_record(rec);
        }

        // The generic multi-output OUT writes (scalcout / acalcout OUT->OVAL)
        // are NOT part of this tail: C performs a record's output writes inside
        // `process()` BEFORE `monitor()` commits the cycle's alarm, so they run
        // pre-commit in `dispatch_multi_output_values` (see R14-62). This tail
        // is C's `recGblFwdLink` equivalent only.

        // 5. FLNK — C `dbScanFwdLink` → `dbScanPassive` → `processTarget`,
        // through the single owner that holds the Passive gate.
        // 5b. An external (`pva://`/`ca://`) FLNK goes out through the link
        // set's `scanForward` (pvalink `pvaScanForward`) instead — a
        // process-only trigger of the remote target. Both halves come from the
        // one resolution `RecordInstance::forward_target` made under the
        // monitor segment's guard, so the tail re-reads nothing.
        match flnk {
            crate::server::record::record_instance::ForwardTarget::Db { name, putf, notify } => {
                guard.release();
                self.process_target(
                    name,
                    super::links::ProcessTargetGate::ScanPassive,
                    *putf,
                    notify.as_ref(),
                    visited,
                );
            }
            crate::server::record::record_instance::ForwardTarget::External(pv) => {
                guard.release();
                self.scan_forward_external_flnk(rec, pv);
            }
            crate::server::record::record_instance::ForwardTarget::None => {}
        }

        // 6. CP link targets -- holders of a CP/CPP link on this record,
        // driven by what this cycle POSTED (see `CyclePosts`), not by the
        // fact that it processed.
        if src.posts.triggers_cp() && self.sources_cp_edges(name, rec) {
            guard.release();
            self.dispatch_cp_targets(name, rec, src.posts, visited);
        }

        // 7. RPRO: if reprocess requested, clear flag and queue a
        // fresh process pass.
        //
        // C `recGblFwdLink` (recGbl.c:296-300) consumes RPRO via
        // `scanOnce(pdbc)` — the record is QUEUED on the scanOnce ring
        // buffer and reprocessed in a separate pass with a fresh lock
        // cycle AFTER the current process chain fully unwinds. It does
        // NOT recurse inline within the current link chain.
        //
        // Spawning a detached task is the Rust equivalent of the
        // scanOnce queue: the reprocess runs on its own task, so it must
        // carry its own `visited` — the current
        // chain's set is a `&mut` local to that stack and cannot be
        // shared. That is now the ONLY reason for the fresh set. It used
        // to be doing double duty as an escape hatch from the cycle
        // guard, which over-blocked; the guard is frame-scoped now
        // ([`Self::run_process_frame`]), so there is nothing to escape.
        {
            let needs_rpro = {
                let instance = guard.hold();
                if instance.common.rpro != 0 {
                    instance.common.rpro = 0;
                    true
                } else {
                    false
                }
            };
            if needs_rpro {
                let db = self.clone();
                let rpro_name = name.to_string();
                // Middle band, not the record's PRIO: C `recGblFwdLink` hands
                // RPRO to `scanOnce` (`recGbl.c`), whose single "scanOnce"
                // thread runs at `epicsThreadPriorityScanLow + nPeriodic`
                // (`dbScan.c:770-779`) and is not a callback band at all.
                crate::runtime::task::spawn_background(
                    crate::runtime::task::CallbackPriority::Medium,
                    async move {
                        let mut fresh_visited = ProcStack::new();
                        let _ = db
                            .process_record_with_links(&rpro_name, &mut fresh_visited)
                            .await;
                    },
                );
            }
        }
    }

    /// Fire a non-DB (external `pva://`/`ca://`) forward link (FLNK).
    ///
    /// C `recGblFwdLink` → `dbScanFwdLink` (`dbLink.c:475-480`) dispatches
    /// every FLNK uniformly through `plink->lset->scanForward`: a DB lset
    /// runs `scanOnce(target)` — handled directly by the local FLNK §5
    /// path — while the pvalink/calink lset runs `pvaScanForward`, a
    /// process-only trigger of the remote target. The DB-only `flnk_name`
    /// filter at the three `should_fire_forward_link` sites dropped every
    /// external FLNK; this is the single owner that forwards them, so the
    /// dispatch is not open-coded per site (each FLNK tail calls only
    /// this).
    ///
    /// On a non-retry, disconnected link the lset returns `Err`; pvxs
    /// raises `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "Disconn")` on
    /// the owning record (`pvxs/ioc/pvalink_lset.cpp:677-680`). This raises
    /// the same *pending* LINK/INVALID alarm via [`rec_gbl_set_sevr_msg`](crate::server::recgbl::rec_gbl_set_sevr_msg),
    /// promoted by the next `recGblResetAlarms` — exactly as the C late-set
    /// inside `recGblFwdLink` (after the record's own alarm/monitor stage)
    /// is.
    fn scan_forward_external_flnk(&self, rec: &Arc<RecordCell>, target: &str) {
        if let Err(e) = self.scan_forward_external_pv(target) {
            let _ = e;
            let mut instance = rec.write();
            crate::server::recgbl::rec_gbl_set_sevr_msg(
                &mut instance.common,
                crate::server::recgbl::alarm_status::LINK_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
                "Disconn",
            );
        }
    }

    /// One record-declared input link read — the framework's `dbGetLink`.
    ///
    /// The value goes into `target_field`; the return is C's
    /// `RTN_SUCCESS(dbGetLink(...))` and nothing finer, because C's callers have
    /// nothing finer: `dbGetLink` hands back one `long status`, and every reader
    /// of it — `motorRecord.cc:3687`, `epidRecord.c:191`, `aaoRecord.c`'s
    /// `fetchValue` — asks only whether it was zero.
    ///
    /// `true` is that zero, and it covers the reads that delivered NO value as
    /// well as the ones that did: an empty link, a CONSTANT link
    /// (`dbConstGetValue`, `dbConstLink.c:219-225`, sets `*pnRequest = 0` and
    /// returns 0), and the source class the record has no case for (C's
    /// `default:` — `dbGetLink` is never called, so `status` keeps the 0 it was
    /// initialised with). `false` is the non-zero status: a dead DB target, a
    /// disconnected CA link, a value the target field rejects.
    ///
    /// Returning `Option<bool>` here — "nothing attempted" apart from "no
    /// value" — invited [`Self::execute_read_db_links`] to report only
    /// `Some(true)` as resolved, which made a CONSTANT link indistinguishable
    /// from a failed one to every record reading that report. A motor with a
    /// constant `RDBL` stopped its own axis (`motorRecord.cc:3690-3697`) on a
    /// read C calls successful. The multi-input fetch loop, reading the same
    /// links on the same records, always used C's rule.
    ///
    /// On the `false` side C `dbGetLink` (`dbLink.c:316-323`) runs
    /// `setLinkAlarm(plink)`, i.e. `recGblSetSevrMsg(precord, LINK_ALARM,
    /// INVALID_ALARM, "%s", dbLinkFieldName(plink))` — so the failure raises
    /// LINK/INVALID carrying the link's field name as the AMSG, right here, as
    /// an effect of the read itself. Every caller inherits it; none can forget
    /// it.
    ///
    /// A HEALTHY read is the other half of the same C function: `dbDbGetValue`
    /// ends with `recGblInheritSevrMsg` (`dbDbLink.c:228-232`), so an
    /// `field(INP,"SRC MS")` on a compress / aao-DOL / epid link raises the
    /// READER to the source's severity. That inheritance runs here too, through
    /// `input_link_inheritance` — the same owner the multi-input
    /// fetch uses.
    ///
    /// The DBR class of the read is the RECORD's
    /// ([`Record::input_link_request`](crate::server::record::Record::input_link_request), C's `dbGetLink` `dbrType` argument),
    /// resolved from the SOURCE's metadata by the same owner the OUT side uses
    /// ([`Self::resolve_out_target`]): a record that switches on the source's
    /// DBF class (sseq `DOLn`, `sseqRecord.c:640-705`) gets the value C's
    /// `dbGetLink` would deliver — an `ENUM`/`MENU` source's LABEL, a `CHAR`
    /// array's bytes — instead of a native value it would have to guess at.
    /// `None` from the record is C's `default: break`: no read, no alarm.
    fn read_db_link_into_field(
        &self,
        rec: &Arc<RecordCell>,
        link_field: &'static str,
        target_field: &'static str,
        visited: &mut ProcStack,
    ) -> bool {
        let link_str = {
            let instance = rec.read();
            instance
                .record
                .get_field(link_field)
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };
        // An empty link IS a CONSTANT link in C (`dbConstLink.c`'s lset with a
        // NULL string), and `dbConstGetValue` returns 0 for it.
        if link_str.is_empty() {
            return true;
        }
        let parsed = crate::server::record::parse_link_v2(link_str.as_str_lossy().as_ref());
        // The source's DBF class + element count (C `dbGetLinkDBFtype` /
        // `dbGetNelements` — the same lset accessors the OUT side asks of a
        // destination), resolved with NO record lock held: a self-referencing
        // link would otherwise re-enter this record's own gate.
        // C's `default:` arm — the record's switch has no case for this link,
        // so `dbGetLink` is never called: nothing is attempted, the untouched
        // `status` raises no link alarm, and it is still zero.
        let Some(read_as) = self.input_link_read_as(rec, link_field, &parsed) else {
            return true;
        };
        use crate::server::recgbl::simm::LinkFetch;
        match self.read_link_value_as(&parsed, read_as, visited) {
            // C `dbConstGetValue`: SUCCESS with nothing written. The target
            // field keeps what it holds (a client's `caput SELN 5` survives a
            // `field(SELL,"3")`), no LINK alarm is raised, and the link did NOT
            // deliver. The constant reached the record once, at init, via
            // `rec_gbl_init_constant_links`. Status 0 all the same, so the
            // record is told the read SUCCEEDED — C's `dbGetLink` on a constant
            // returns 0, and `motorRecord.cc:3690` stops the axis on non-zero.
            LinkFetch::NoData => true,
            LinkFetch::Value(value) => {
                // C `dbDbGetValue` tail (dbDbLink.c:228-232): a healthy read
                // folds the SOURCE's committed alarm into the READER per the
                // link's MS class. The source has already been processed above
                // (a PP link), so its alarm is the one this cycle sees.
                let inheritance = {
                    let alarm = self.read_link_with_alarm(&parsed).1;
                    self.input_link_inheritance(rec, &parsed, alarm)
                };
                let mut instance = rec.write();
                // A value the target field REJECTS is a failed read, not a
                // silent no-op: C `dbGetLink`'s conversion failure comes back as
                // a non-zero status and takes the `setLinkAlarm` path
                // (`dbLink.c:316-323`) exactly like a dead target. Discarding it
                // left the target field holding its previous value with no
                // alarm to say so.
                let stored = instance
                    .record
                    .put_field_internal(target_field, value)
                    .is_ok();
                if !stored {
                    crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, link_field);
                    return false;
                }
                if let Some((ms, alarm)) = inheritance {
                    super::links::inherit_sevr_msg(&mut instance.common, ms, &alarm);
                }
                true
            }
            LinkFetch::Failed => {
                let mut instance = rec.write();
                crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, link_field);
                false
            }
        }
    }

    /// Execute the ReadDbLink actions of a stage, and report which
    /// `link_field`s C would call a SUCCESSFUL `dbGetLink` — see
    /// [`Self::read_db_link_into_field`], which owns the read (and its
    /// LINK/INVALID alarm on failure).
    ///
    /// One list, one meaning: the multi-input fetch loop feeds the same
    /// `set_resolved_input_links` report on the same predicate
    /// ([`LinkFetch::is_ok`](crate::server::recgbl::simm::LinkFetch::is_ok), C's
    /// `status == 0`), so a record deriving "this link failed" from absence gets
    /// the same answer whichever path read it.
    fn execute_read_db_links(
        &self,
        _record_name: &str,
        rec: &Arc<RecordCell>,
        actions: &[crate::server::record::ProcessAction],
        visited: &mut ProcStack,
    ) -> Vec<&'static str> {
        use crate::server::record::ProcessAction;
        let mut resolved = Vec::new();
        for action in actions {
            match action {
                ProcessAction::ReadDbLink {
                    link_field,
                    target_field,
                } => {
                    if self.read_db_link_into_field(rec, link_field, target_field, visited) {
                        resolved.push(*link_field);
                    }
                }
                // The OUT-link twin: resolve the target's class and hand it to
                // the record, so its `process()` can branch on it (C's
                // `checkLinks`-cached `lnk_field_type`).
                ProcessAction::ResolveOutTarget { link_field } => {
                    self.resolve_out_target_into_record(rec, link_field);
                }
                _ => {}
            }
        }
        resolved
    }

    /// Resolve one OUT link's TARGET and hand it to the record ahead of
    /// `process()` — [`ProcessAction::ResolveOutTarget`](crate::server::record::ProcessAction::ResolveOutTarget).
    ///
    /// The record's own link string is the input, so an empty/constant `LNKn`
    /// resolves to [`OutTarget::UNRESOLVED`](crate::server::record::OutTarget::UNRESOLVED) and the record sees "no target",
    /// which is the answer C's `default:` arm acts on.
    fn resolve_out_target_into_record(&self, rec: &Arc<RecordCell>, link_field: &'static str) {
        let link_str = rec.read().link_text(link_field);
        let parsed = crate::server::record::parse_output_link_v2(link_str.as_deref().unwrap_or(""));
        let target = self.resolve_out_target(&parsed);
        rec.write()
            .record
            .set_resolved_out_target(link_field, target);
    }

    /// Execute ProcessActions returned by a record's process() call.
    ///
    /// Actions are executed in order:
    /// - ReadDbLink: reads a linked PV value and writes it into a record field
    ///   (bypasses read-only checks via put_field_internal)
    /// - WriteDbLink: writes a value to a linked PV
    /// - ReprocessAfter: schedules a delayed re-process via tokio::spawn
    pub(super) fn execute_process_actions(
        &self,
        record_name: &str,
        rec: &Arc<RecordCell>,
        actions: impl IntoIterator<Item = crate::server::record::ProcessAction>,
        visited: &mut ProcStack,
    ) {
        use crate::server::record::ProcessAction;

        for action in actions {
            match action {
                ProcessAction::ReadDbLink {
                    link_field,
                    target_field,
                } => {
                    // The read (and the LINK/INVALID alarm a failed one raises,
                    // C `dbGetLink` -> `setLinkAlarm`) belongs to ONE owner, so
                    // an input link cannot fail silently on one stage and
                    // loudly on another.
                    let _ = self.read_db_link_into_field(rec, link_field, target_field, visited);
                }
                // A pre-process action (the record asks for the target BEFORE it
                // decides), so it is a no-op if it reaches the post-process
                // stage — the resolve here would be too late to change anything.
                ProcessAction::ResolveOutTarget { .. } => {}
                ProcessAction::WriteDbLink { link_field, value } => {
                    // 1. Get the link string (record fields → common fields)
                    // and the source PUTF for processTarget propagation,
                    // plus the PENDING alarm for `recGblInheritSevrMsg`
                    // MS-class propagation into the OUT-link target — this
                    // write stage runs before the cycle's
                    // `rec_gbl_reset_alarms`, exactly where C reads
                    // `psrce->nsta/nsev/namsg` ([`LinkAlarm::pending`]).
                    let (link_str, src_putf, src_notify, src_alarm) = {
                        let instance = rec.read();
                        let link = instance
                            .resolve_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        (
                            link,
                            instance.common.putf,
                            instance.notify.clone(),
                            super::links::LinkAlarm::pending(&instance.common),
                        )
                    };
                    if link_str.is_empty() {
                        // No link to put through: C `dbPutLink` on an
                        // unresolved link is a failure, and the emitter is
                        // told so — every emitted action reports exactly once,
                        // so a record deriving a field from the result cannot
                        // be left holding a stale one.
                        rec.write()
                            .record
                            .set_out_link_write_status(link_field, &value, true);
                        continue;
                    }
                    // 2. Parse and write to the linked PV — DB *or*
                    // external `ca://`/`pva://`. A record's `process()`
                    // emits `WriteDbLink` to drive an OUT-link field
                    // (transform `OUTn`, throttle/scaler `COUTP`, epid
                    // `TRIG`/`OUTL`); that field may resolve to a CA/PVA
                    // link, which C `dbPutLink` routes through the link
                    // set's `putValue` identically to a DB link
                    // (dbLink.c:434-448). The field is a `DBF_OUTLINK`, so it
                    // carries the OUT modifier mask (`dbStaticLib.c:2382-2387`).
                    let parsed = crate::server::record::parse_output_link_v2(
                        link_str.as_str_lossy().as_ref(),
                    );
                    let failed = self.write_out_link_value(
                        rec,
                        &parsed,
                        value.clone(),
                        super::links::OutLinkSrc {
                            putf: src_putf,
                            notify: src_notify.as_ref(),
                            alarm: &src_alarm,
                            field: link_field,
                        },
                        visited,
                    );
                    // The record-owned half of the put's outcome. The alarm
                    // half was already raised by `write_out_link_value`; this
                    // is what lets a record keep a C-truthful status field
                    // (throttle STS) instead of committing its own intent.
                    rec.write()
                        .record
                        .set_out_link_write_status(link_field, &value, failed);
                }
                ProcessAction::DeviceCommand { command, ref args } => {
                    let mut instance = rec.write();
                    if let Some(mut dev) = instance.device.take() {
                        // `handle_command` runs after the process snapshot
                        // was already built/notified, so any record field
                        // it mutated needs an explicit monitor post. The
                        // returned field names are posted with DBE_VALUE,
                        // mirroring the C record's `db_post_events` calls
                        // from inside `process()` (scalerRecord.c:425-430).
                        let changed = dev
                            .handle_command(&mut *instance.record, command, args)
                            .unwrap_or_default();
                        instance.device = Some(dev);
                        for field in changed {
                            instance.notify_field(field, crate::server::recgbl::EventMask::VALUE);
                        }
                    }
                }
                ProcessAction::DelayedCallbackAfter(delay) => {
                    // C `callbackRequestDelayed` whose handler mutates the
                    // record before `dbProcess` (bo/busy HIGH one-shot). The
                    // mutation lives in `delayed_callback_fire`, not in
                    // `process()`, so only this timer can perform it.
                    self.schedule_delayed_callback(record_name, delay);
                }
                ProcessAction::ReprocessAfter(delay) => {
                    // Owner-driven delayed re-entry, mirroring C
                    // `callbackRequestDelayed` dispatching to
                    // `(*prset->process)(prec)` directly (callback.c). The
                    // mint-token + delayed-fire is the single
                    // `schedule_delayed_reprocess` owner, shared with the
                    // SDLY async-simulation defer.
                    self.schedule_delayed_reprocess(record_name, delay);
                }
                ProcessAction::ArmWatchdog => {
                    // C `wdogInit` from `special()` (histogram SDEL,
                    // histogramRecord.c:266-268). The arm owner supersedes any
                    // tick already in flight.
                    self.arm_watchdog(record_name);
                }
                ProcessAction::ScanOnce => {
                    // C `scanOnce(precord)`. The `if (precord->scan)` guard C
                    // writes at every `special()` call site (scalerRecord.c:655,
                    // :667) is owned HERE: a Passive record is already processed
                    // by the put's own `pp(TRUE)` path (dbAccess.c:1265-1268), so
                    // scanning it again would double-process; a non-Passive
                    // record gets no process from the put at all, which is the
                    // whole reason C makes the call — without it the state
                    // change waits for the next periodic scan.
                    let passive = {
                        let instance = rec.read();
                        instance.common.scan == crate::server::record::ScanType::Passive
                    };
                    if !passive {
                        // Queued, not awaited: C's `scanOnce` hands the record
                        // to the scan-once thread, which takes `dbScanLock` —
                        // the process lands after the putting thread leaves
                        // `dbPutField` and releases the record gate this call is
                        // still holding.
                        let db = self.clone();
                        let name = record_name.to_string();
                        // Middle band, not the record's PRIO: `scanOnce` is a
                        // dedicated thread in C (`dbScan.c:770-779`), not one
                        // of the three callback queues.
                        crate::runtime::task::spawn_background(
                            crate::runtime::task::CallbackPriority::Medium,
                            async move {
                                let mut visited = ProcStack::new();
                                let _ = db.process_record_with_links(&name, &mut visited).await;
                            },
                        );
                    }
                }
                ProcessAction::WriteDbLinkNotify { link_field, value } => {
                    // C `sseqRecord.c` WAITn put-callback dependency: write
                    // the OUT link as a put-WITH-completion and re-enter THIS
                    // record's process() once the downstream record (plus its
                    // FLNK/OUT chain) finishes. Same OUT-link write a plain
                    // WriteDbLink performs, wrapped in the c401e2f0 put-notify
                    // wait-set + async re-entry primitive.
                    let (link_str, src_putf, src_alarm) = {
                        let instance = rec.read();
                        let link = instance
                            .resolve_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        (
                            link,
                            instance.common.putf,
                            super::links::LinkAlarm::pending(&instance.common),
                        )
                    };
                    // Mint the re-entry token BEFORE issuing the put so a
                    // synchronous downstream completion cannot fire the
                    // oneshot before the waiter is wired. The mint supersedes
                    // any prior pending re-entry for this record (newer
                    // token), exactly like ReprocessAfter.
                    let token = match self.mint_async_token(record_name) {
                        Some(t) => t,
                        None => continue,
                    };
                    let (waitset, completion) = Self::new_put_notify();
                    if !link_str.is_empty() {
                        // `DBF_OUTLINK` field — OUT modifier mask applies
                        // (`dbStaticLib.c:2382-2387`).
                        let parsed = crate::server::record::parse_output_link_v2(
                            link_str.as_str_lossy().as_ref(),
                        );
                        self.write_out_link_value(
                            rec,
                            &parsed,
                            value,
                            super::links::OutLinkSrc {
                                putf: src_putf,
                                notify: Some(&waitset),
                                alarm: &src_alarm,
                                field: link_field,
                            },
                            visited,
                        );
                    }
                    // Release the initiator's own wait-set count (C
                    // `dbProcessNotify` holds one count for the requester and
                    // drops it after issuing the put). The set then drains —
                    // and fires the completion — when the downstream
                    // target(s) that joined via `join_put_notify` finish, or
                    // immediately when the link was empty / the target
                    // completed synchronously.
                    waitset.leave();
                    self.reprocess_on_notify(token, completion);
                }
                ProcessAction::CancelReprocess => {
                    // C `callbackCancelDelayed` for `sseq` ABORT: advance the
                    // record's re-entry generation so any pending DLYn timer
                    // or WAITn notify re-entry becomes a structural no-op (the
                    // AsyncToken gate), with no runtime is-aborted check on
                    // the re-entry path.
                    self.cancel_async_reentry(record_name);
                }
            }
        }
    }

    /// Complete an asynchronous record's post-process steps.
    /// Call after device support signals completion (clears PACT, runs alarms, snapshot, OUT, FLNK).
    ///
    /// # The completion RE-TAKES the gate
    ///
    /// This is the other half of C's async-device shape. `dbProcess` released
    /// `dbScanLock` when it set `pact` and returned; the completion runs on the
    /// callback task, which takes the record's lock again for the epilogue —
    /// C `callback.c:379-388` `ProcessCallback`:
    ///
    /// ```c
    /// dbScanLock(pRec);
    /// (*pRec->rset->process)(pRec);
    /// dbScanUnlock(pRec);
    /// ```
    ///
    /// So the epilogue below — alarm commit, snapshot, OUT writes, FLNK — runs
    /// under the SAME exclusion as the cycle that started it, and a put that
    /// arrived during the async window has either already been serialised
    /// ahead of it or waits behind it. Every caller reaches this from a
    /// completion task holding no gate (the device-write completion spawn
    /// above, the seq DLYn chain, the tests); nothing calls it with the gate
    /// held, which would dead-lock on the non-reentrant gate.
    pub fn complete_async_record<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        self.complete_async_record_with_outcome(name, Ok(()))
    }

    /// [`Self::complete_async_record`] for an async device write, carrying
    /// the write's outcome: an `Err` raises `WRITE_ALARM`/`INVALID` on the
    /// completing pass, as the synchronous `write()` branch raises it in
    /// place and as C's `processCallbackOutput` carries `result.status` to
    /// the record's re-entry (devAsynFloat64.c:668). The only way to end an
    /// async write cycle is through here, so a failed write cannot complete
    /// `NO_ALARM`.
    pub fn complete_async_record_with_outcome<'a>(
        &'a self,
        name: &'a str,
        outcome: CaResult<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            // Alias-aware entry — same pattern as
            // `process_record_with_links_inner`. `name` may arrive as an alias
            // from an async device-support callback that captured the original
            // record name; normalise to canonical so the gate below, the
            // `visited` cycle set, and downstream FLNK/OUT dispatches all see
            // the same canonical name.
            //
            // Resolved HERE and not in the body, because the gate wants the
            // record: `lock_instance` reaches the lock set through the
            // record's own cell, where `lock_record(name)` would repeat this
            // very lookup by hand.
            let (canonical, rec) = match self.lookup_record(name) {
                Some(found) => found,
                // Nothing registered under the name. An alias whose target has
                // gone has always reported the TARGET as missing.
                None => {
                    let missing = self.resolve_alias(name).unwrap_or_else(|| name.to_string());
                    return Err(CaError::ChannelNotFound(missing));
                }
            };
            let _record_gate = self.lock_instance(&rec);
            let mut visited = ProcStack::new();
            self.complete_async_record_inner(canonical, rec, outcome.err(), &mut visited)
        })
    }

    fn complete_async_record_inner(
        &self,
        canonical: Arc<str>,
        rec: Arc<RecordCell>,
        write_error: Option<CaError>,
        visited: &mut ProcStack,
    ) -> CaResult<()> {
        // Seed the cycle guard with this record's own name — mirrors
        // the synchronous main path ([`Self::run_process_frame`] does
        // `visited.insert(name)` before the body). Without this
        // the async-completion FLNK / OUT / CP dispatch can re-enter
        // the just-completed record: an async FLNK chain that loops
        // back (A async -> completes -> FLNK -> B -> FLNK -> A) would
        // re-process A unbounded, because PACT is cleared below before
        // the FLNK dispatch and nothing else blocks the re-entry.
        //
        // This is a frame like any other, so it owes the same unwind at the
        // tail — see the invariant on [`Self::run_process_frame`].
        if !visited.claim(&rec) {
            return Ok(()); // Already on this stack, skip
        }
        let name: &str = &canonical;

        // The async completion is the tail of a cycle, and it posts; it owes
        // the same one resolve, at the same no-lock-held point, as the
        // synchronous body — see `process_record_with_links_body`.
        let link_backing = self.resolve_link_backed_metadata_for_posts(&rec);
        let link_backing = link_backing.as_link_backing();

        // This pass IS C's `process()` re-entry, so it runs the whole
        // `recGblGetTimeStampSimm` again — TSEL read included, before the guard.
        let tsel = self.read_tsel(&rec);

        let (flnk_name, pact_exit, posts) = {
            // Phase 1 — first write guard, confined to this scope so the
            // (!Send) parking_lot guard is released before the async OUT
            // writes below. Yields the output work plus the put-notify
            // source fields those writes consume.
            let (out_info, skip_out, src_putf, src_notify, src_alarm, plan) = {
                let mut instance = rec.write();

                // UDF update before alarm evaluation (C parity — see the
                // sync process path). A NaN/undefined value keeps UDF true
                // so `recGblCheckUDF` raises UDF_ALARM this cycle.
                if instance.record.clears_udf() {
                    instance.common.udf = instance.record.value_is_undefined() as u8;
                }
                // A failed async device write — the same pending raise the
                // synchronous branch makes, ahead of `checkAlarms` as C's
                // device support raises it ahead of the record's own
                // (devAsynFloat64.c:668-671), so on an INVALID tie the WRITE
                // status is the one that reaches STAT.
                if let Some(e) = &write_error {
                    eprintln!("device write error on {name}: {e}");
                    crate::server::recgbl::rec_gbl_set_sevr(
                        &mut instance.common,
                        crate::server::recgbl::alarm_status::WRITE_ALARM,
                        crate::server::record::AlarmSeverity::Invalid,
                    );
                }
                // Per-record alarm hook (C `checkAlarms()`).
                {
                    let inst = &mut *instance;
                    inst.record.check_alarms(&mut inst.common);
                }

                // Evaluate alarms
                instance.evaluate_alarms();

                // Any soft flavour: the framework owns the transfer, so there
                // is no device to take an alarm, time stamp or user tag from.
                let is_soft = instance.common.dtyp.is_soft();

                // Device support alarm/timestamp override
                if !is_soft {
                    let (dev_alarm, dev_ts, dev_utag) = if let Some(ref dev) = instance.device {
                        (dev.last_alarm(), dev.last_timestamp(), dev.last_utag())
                    } else {
                        (None, None, None)
                    };
                    if let Some((stat, sevr)) = dev_alarm {
                        crate::server::recgbl::rec_gbl_set_sevr(
                            &mut instance.common,
                            stat,
                            crate::server::record::AlarmSeverity::from_u16(sevr),
                        );
                    }
                    if let Some(ts) = dev_ts {
                        instance.common.time = ts;
                    }
                    // C device support writes `prec->utag` directly during
                    // `read()` — the event-system pulse-id path, since
                    // `epicsTimeStamp` carries no tag. Adopt the device's
                    // userTag when it supplies one; read in the same `dev`
                    // borrow as the timestamp above so the time/tag pair is a
                    // single consistent device snapshot.
                    if let Some(utag) = dev_utag {
                        instance.common.utag = utag;
                    }
                }

                // BEFORE the output stage — C `aoRecord.c:190` stamps the record
                // ahead of `writeValue` so a downstream TSEL fetch sees this
                // cycle's time.
                let inst = &mut *instance;
                tsel.stamp(&inst.name, &mut inst.common, is_soft);
                // UDF was already updated before `evaluate_alarms` above.

                // ---- Output stage. C `process()` performs the record's output
                // BEFORE `monitor()`, and `monitor()` is where `recGblResetAlarms`
                // commits the cycle's alarm — the async-completion re-entry runs
                // that same `process()` body. A failed `dbPutLink` raises
                // LINK_ALARM/INVALID inside the put (`setLinkAlarm`,
                // dbLink.c:434-448), so the commit MUST follow the writes for the
                // alarm to land in this cycle's SEVR and monitor posts.

                // IVOA check — on the PENDING severity, which is what C's
                // `writeValue` call site tests (`if (prec->nsev < INVALID_ALARM)`,
                // aoRecord.c:196).
                let skip_out =
                    if instance.common.nsev == crate::server::record::AlarmSeverity::Invalid {
                        let ivoa = instance
                            .record
                            .get_field("IVOA")
                            .and_then(|v| v.to_menu_index())
                            .unwrap_or(0);
                        match ivoa {
                            1 => true,
                            2 => {
                                // See the IVOA=2 comment in
                                // `process_record_with_links_inner` — IVOA=2
                                // delegates to the per-record
                                // `apply_invalid_output_value` so OVAL/RVAL/VAL
                                // get the C-convention values.
                                // The same "cannot fail in C" contract as the
                                // sync arm above; see its note.
                                if let Some(ivov) = instance.record.get_field("IVOV") {
                                    let applied = instance.record.apply_invalid_output_value(ivov);
                                    debug_assert!(
                                        applied.is_ok(),
                                        "{}: IVOA=Set_output_to_IVOV could not apply IVOV: {:?}",
                                        instance.record.record_type(),
                                        applied.err()
                                    );
                                }
                                false
                            }
                            _ => false,
                        }
                    } else {
                        false
                    };

                // OEVT: queue the output event when the output fires — same
                // IVOA-gated event-twin of the OUT write as
                // `process_record_with_links_inner`.
                if !skip_out {
                    if let Some(event_name) = instance.record.output_event() {
                        let db = self.clone();
                        // Middle band, not this record's PRIO: C `postEvent`
                        // fires one `callbackRequest` per non-empty band and
                        // each carries the *scanned* record's priority
                        // (`dbScan.c:513-527`), a fan-out the port's single
                        // Event list cannot express (`scan_index.rs`
                        // `post_event_named`). The poster's own PRIO is not
                        // the answer, so this keeps `callbackRequest`'s
                        // general band (`callback.h:42`).
                        crate::runtime::task::spawn_background(
                            crate::runtime::task::CallbackPriority::Medium,
                            async move {
                                db.post_event_named(&event_name).await;
                            },
                        );
                    }
                }

                let can_dev_write = instance.record.can_device_write();
                // Same single owner of the DTYP -> soft dset mapping as the
                // synchronous OUT stage (`RecordInstance::soft_output_value`).
                let soft_out = instance.soft_output_value();
                let record_should_output = instance.record.should_output();
                let out_info = if skip_out {
                    None
                } else if !can_dev_write {
                    // Non-output records (calcout, etc.) with soft OUT link
                    // (DB or external `ca://`/`pva://`).
                    if record_should_output && instance.parsed_out.is_writable_out_link() {
                        let out_val = instance.record.output_link_value();
                        out_val.map(|v| (instance.parsed_out.clone(), v))
                    } else {
                        None
                    }
                } else if let Some(out_val) = soft_out {
                    if instance.parsed_out.is_writable_out_link() {
                        out_val.map(|v| (instance.parsed_out.clone(), v))
                    } else {
                        None
                    }
                } else {
                    // Non-soft output: the async device write already completed
                    // (that's why we're in complete_async_record). Don't re-do
                    // write_begin -- it would start another async cycle.
                    None
                };

                // PUTF / put-notify wait-set / source PENDING alarm — the
                // values C `dbDbPutValue` reads at the put (dbDbLink.c:382-383
                // takes `psrce->nsta/nsev/namsg`). Captured here and returned
                // so the OUT writes run with NO record guard held (a self /
                // cyclic OUT link would dead-lock on the non-reentrant gate);
                // a fresh guard is re-taken below for the commit.
                let src_putf = instance.common.putf;
                let src_notify = instance.notify.clone();
                let src_alarm = super::links::LinkAlarm::pending(&instance.common);
                (
                    out_info,
                    skip_out,
                    src_putf,
                    src_notify,
                    src_alarm,
                    rec.process_plan(),
                )
            };

            // Phase 2 — async OUT writes, no record guard held.
            let src = super::links::OutLinkSrc {
                putf: src_putf,
                notify: src_notify.as_ref(),
                alarm: &src_alarm,
                field: "OUT",
            };
            if let Some((ref link, ref out_val)) = out_info {
                self.write_out_link_value(&rec, link, out_val.clone(), src, visited);
            }
            // Same `conditional_write` epilogue as the synchronous stage. C
            // runs it on the async device's first pass as well (the record
            // returns at `longoutRecord.c:187` only AFTER `writeValue`), and
            // the port's first pass returns from `write_begin` before this
            // point — so an async longout latched on no pass at all.
            if !skip_out {
                rec.write().record.after_output_decision();
            }
            self.dispatch_multi_output_values(&rec, src, skip_out, plan, visited);

            // Phase 3 — fresh write guard for the alarm commit + monitor tail.
            let mut instance = rec.write();

            // C `monitor()` with its opening `recGblResetAlarms` — after every
            // output.
            let outcome = instance.monitor_cycle();

            // Clear PACT. The release hands back the put-notify parked on this
            // window; it is carried to the tail below (C `recGblFwdLink` →
            // `dbNotifyCompletion`), never replayed here — the OUT/FLNK chain
            // this cycle still owes has not run yet.
            let pact_exit = instance.leave_pact();

            // Put-notify completion is NOT fired here. The async device
            // round-trip has finished, but the OUT/FLNK/process-action
            // tail it drives (below) may itself reach an async target;
            // firing now would report WRITE_NOTIFY done while that chain
            // still runs. The originating record `leave`s the wait-set at
            // the END of this function, after every PP target it drives
            // has joined. See `complete_put_notify` at the tail.

            let flnk_name = instance.forward_target();

            // Notify subscribers, still under this segment's own guard.
            let posts = publish_cycle(
                &mut instance,
                &outcome.snapshot,
                link_backing,
                outcome.alarm_posts,
            );

            // The FLNK's PUTF + put-notify wait-set ride in `flnk_name`
            // (see `ForwardTarget::Db`). On the async-completion path PUTF
            // was set when the put landed on the record; it (and wait-set
            // membership) must propagate through the (now-completing) FLNK
            // chain so an async target reached here also defers
            // WRITE_NOTIFY completion.
            (flnk_name, pact_exit, posts)
        };

        // The record's own OUT link and its generic multi-output pairs were
        // written in the pre-commit output stage above — C `process()` runs
        // `writeValue` before `monitor()`, and a failed `dbPutLink` must be
        // able to raise LINK_ALARM into the alarm this cycle commits
        // (dbLink.c:434-448). Only the fanout/seq dispatch and the FLNK tail
        // remain here.

        // Multi-output dispatch, forward-link phase (fanout). The
        // `ForwardLink` phase skips dfanout and seq here, which is correct:
        // their value-carrying `OUTn`/`LNKn` are driven pre-commit on the
        // processing path. seq DOES reach this function as an async
        // completion — it is C's `asyncFinish` for the DLYn group chain
        // (`seqRecord.c:219-241`) — and its groups have already run, so
        // re-dispatching them here would drive every LNKn twice.
        let plan = rec.process_plan();
        if plan.multi_output_dispatch {
            let _ =
                self.dispatch_multi_output(&rec, super::links::MultiOutPhase::ForwardLink, visited);
        }

        // event record: post the named software event.
        if plan.posts_software_event {
            self.dispatch_event_record(&rec);
        }

        // FLNK — the async-completion tail's copy of the same C path, through
        // the same single owner (C `dbScanFwdLink` → `dbScanPassive` →
        // `processTarget`).
        // Both halves of the FLNK come from the one resolution
        // `RecordInstance::forward_target` made under the monitor segment's
        // guard, exactly as on the synchronous tail (C `dbScanFwdLink` →
        // `dbScanPassive` for a DB target, → lset `scanForward` for an
        // external one).
        match &flnk_name {
            crate::server::record::record_instance::ForwardTarget::Db { name, putf, notify } => {
                self.process_target(
                    name,
                    super::links::ProcessTargetGate::ScanPassive,
                    *putf,
                    notify.as_ref(),
                    visited,
                );
            }
            crate::server::record::record_instance::ForwardTarget::External(pv) => {
                self.scan_forward_external_flnk(&rec, pv);
            }
            crate::server::record::record_instance::ForwardTarget::None => {}
        }

        // CP link targets — gated on what this cycle posted, as on the
        // synchronous tail.
        self.dispatch_cp_targets(name, &rec, posts, visited);

        // RPRO: C `recGblFwdLink` consumes a pending reprocess via
        // `scanOnce` — queued, not recursed. Mirror the synchronous
        // path: spawn a fresh process pass (clean `visited`).
        {
            let needs_rpro = {
                let mut guard = rec.write();
                if guard.common.rpro != 0 {
                    guard.common.rpro = 0;
                    true
                } else {
                    false
                }
            };
            if needs_rpro {
                let db = self.clone();
                let rpro_name = name.to_string();
                // Middle band, not the record's PRIO: C `recGblFwdLink` hands
                // RPRO to `scanOnce` (`recGbl.c`), whose single "scanOnce"
                // thread runs at `epicsThreadPriorityScanLow + nPeriodic`
                // (`dbScan.c:770-779`) and is not a callback band at all.
                crate::runtime::task::spawn_background(
                    crate::runtime::task::CallbackPriority::Medium,
                    async move {
                        let mut fresh_visited = ProcStack::new();
                        let _ = db
                            .process_record_with_links(&rpro_name, &mut fresh_visited)
                            .await;
                    },
                );
            }
        }

        // C `recGbl.c::recGblFwdLink:302` clears `putf = FALSE` after
        // the forward-link dispatch. The same clearing must happen
        // at the tail of the async-completion path (this is the moral
        // equivalent of the synchronous completion path in
        // `put_record_field_from_ca` which clears after
        // `process_record_with_links` returns). Without this, a
        // record that completed an async write triggered by a
        // CA put would keep `putf=1` forever, leaking into every
        // subsequent scan-driven process cycle.
        {
            let mut guard = rec.write();
            guard.common.putf = false;
        }

        // Put-notify completion: the async device round-trip is done and
        // the full OUT/FLNK/process-action tail above has run, so every PP
        // target it drove has joined the wait-set. The originating record
        // now `leave`s; the completion oneshot fires on the `leave` that
        // empties the set (i.e. once every joined async target has also
        // completed). `complete_put_notify` `take`s the membership, so a
        // motor re-entering `complete_async_record_inner` over several
        // device cycles leaves exactly once — matching the old fire site,
        // which `take`d its oneshot.
        {
            let mut guard = rec.write();
            complete_put_notify(&mut guard);
        }

        // C `dbNotifyCompletion` (dbNotify.c:459-473) → `restartCheck`: the
        // put-notifies that arrived while this record was PACT wrote nothing and
        // queued. PACT is clear and this cycle's wait-set has drained, so the
        // record is now the idle record the queue head was meant to see — replay
        // it whole (value + process + callback), through the single drain owner.
        self.apply_pact_exit(name, &rec, pact_exit);

        // The unwind for the seed above: this frame is leaving the stack, so
        // its marker goes with it (C `dbDbLink.c:521-526`).
        visited.release(&rec);
        Ok(())
    }

    /// Dispatch CP-link targets that take a CP/CPP input link from `name`,
    /// when this cycle published a class the CP subscription selects.
    ///
    /// **The trigger is a monitor post, never a process.** C serves every
    /// CP/CPP link as a CA link — `dbInitLink` tests the modifier BEFORE
    /// locality and short-circuits `dbDbInitLink` entirely, so a CP link to a
    /// record in this very IOC is still a CA link (`dbLink.c:118-122`; the
    /// `isLocal` at `:128` is computed only to pick the init-callback hint).
    /// That subscription is taken with `DBE_VALUE | DBE_ALARM`
    /// (`dbCa.c:1225-1229` → `cadef.h:2010-2011`), and only its
    /// `eventCallback` adds `CA_DBPROCESS` (`dbCa.c:955-963`), which the
    /// worker runs as a bare `db_process` (`:1249-1257`). A source cycle that
    /// posts nothing — an unchanged value inside `MDEL`, no alarm movement —
    /// therefore leaves every CP holder unprocessed.
    ///
    /// The port keeps a local CP target as a `Db` link rather than routing it
    /// through the CA client (the `ca` link set lives in another crate and is
    /// optional, so C's literal structure would silently disable local CP
    /// links in a bare `epics-base-rs` IOC). `posts` is what restores the C
    /// rule on top of that shape: the same `DBE_VALUE|DBE_ALARM` gate the
    /// cross-IOC path gets from its remote monitor
    /// ([`Self::dispatch_external_cp_targets`]), so "CP dispatch" means one
    /// thing on both paths.
    ///
    /// The dispatch itself is the moral equivalent of dbCaTask's
    /// `CA_DBPROCESS` handler invoking `db_process(prec)` and nothing else —
    /// no PUTF, no RPRO. Already-visited targets (current process chain) are
    /// skipped via the `visited` cycle guard.
    fn dispatch_cp_targets(
        &self,
        name: &str,
        rec: &Arc<RecordCell>,
        posts: CyclePosts,
        visited: &mut ProcStack,
    ) {
        if !posts.triggers_cp() {
            return;
        }
        // Whether this record has a CP holder at all is the record's own
        // state, not something to re-derive from a name-keyed map on every
        // cycle — see `PvDatabase::sources_cp_edges`.
        if !self.sources_cp_edges(name, rec) {
            return;
        }
        let cp_targets = self.get_cp_targets(name);
        for target in cp_targets {
            self.process_one_cp_target(&target, visited);
        }
    }

    /// Process a single CP/CPP target edge, applying the CPP passive gate.
    /// This is the single owner of the scan-time CP-dispatch decision, shared
    /// by the local-source path ([`Self::dispatch_cp_targets`]) and the
    /// cross-IOC path ([`Self::dispatch_external_cp_targets`]) so both honour
    /// the same `dbCa.c` semantics.
    ///
    /// The passive gate is the ONLY thing decided here. C's `CA_DBPROCESS`
    /// worker (`dbCa.c:1249-1257`) is bare `dbScanLock` / `db_process` /
    /// `dbScanUnlock`, so an active target is handled by `dbProcess` itself —
    /// which the port models once, in the PACT entry guard of
    /// [`Self::process_record_with_links_body`]. Deciding PACT a second time
    /// here is what let this path diverge from that owner.
    fn process_one_cp_target(&self, target: &super::CpTarget, visited: &mut ProcStack) {
        let target_rec = {
            let records = self.inner.records.read();
            records.get(target.record.as_str()).cloned()
        };
        let skip = match target_rec {
            // CPP gate (`dbCa.c:823-828`, `:958-962`, `:1032-1037`): a CPP link adds
            // `CA_DBPROCESS` only when the link-holder's SCAN is Passive. A
            // non-Passive target is reached by its own periodic/event scan, so
            // it is not dispatched here. A CP link (`passive_only == false`)
            // never takes this branch and always dispatches.
            //
            // epics-base PR #3fb10b6: PUTF must remain false on CP-driven
            // targets — only the record directly receiving the dbPut reports
            // PUTF=1 to dbNotify/onChange observers, so we deliberately do NOT
            // set PUTF here.
            Some(t) => {
                if visited.holds(&t) {
                    return;
                }
                let tg = t.read();
                target.passive_only && tg.common.scan != crate::server::record::ScanType::Passive
            }
            None => false,
        };
        if skip {
            return;
        }
        // recursive CP-target fan-out within one chain —
        // gate already held by the foreign entry record.
        let _ = self.process_record_with_links_recursive(&target.record, visited);
    }

    /// Process every holder of an EXTERNAL CP/CPP link to `external_pv` —
    /// the cross-IOC twin of `Self::dispatch_cp_targets`. Called by the
    /// calink/pvalink CA monitor callback on every remote change, this is
    /// the Rust equivalent of C `dbCa.c eventCallback` adding
    /// `CA_DBPROCESS` for a CP (or Passive CPP) link (`dbCa.c:958-962`)
    /// and the worker thread running `db_process(prec)` (`dbCa.c:1255`).
    /// A cross-IOC source never processes locally, so this callback is the
    /// only trigger; without it a `CP`/`CPP` link's holder never processes
    /// on a remote change.
    ///
    /// A fresh `visited` set starts a new process chain —
    /// the monitor event is an independent external trigger, like a scan,
    /// not a continuation of an in-flight local chain.
    pub fn dispatch_external_cp_targets(&self, external_pv: &str) {
        let targets = self.get_external_cp_targets(external_pv);
        if targets.is_empty() {
            return;
        }
        let mut visited = ProcStack::new();
        for target in targets {
            self.process_one_cp_target(&target, &mut visited);
        }
    }

    /// Apply the SIMM-mode OUTPUT redirect (the `writeValue` half of
    /// simulation). C `writeValue` substitutes the device write with
    /// `dbPutLink(&prec->siol, DBR_DOUBLE, &prec->oval, 1)` (aoRecord.c:574,
    /// `DBR_LONG`/`&prec->rval` in SIMM=RAW at :577), so this runs from the OUT
    /// epilogue after the body computed OVAL/RVAL.
    ///
    /// SIOL is a `DBF_OUTLINK` (aoRecord.dbd) driven by the SAME `dbPutLink`
    /// as the record's OUT: it is not a bare field poke. Routing it through
    /// [`Self::write_out_link_value`] — the put owner — is what gives the
    /// simulated write everything C's `dbDbPutValue` (dbDbLink.c:372-393) does
    /// and the old open-coded `put_pv_already_locked` did not: MS-class alarm
    /// inheritance into the SIOL target, `PP`/`.PROC` `processTarget`, PUTF and
    /// put-notify propagation — and the failed-put `LINK_ALARM`/`INVALID`
    /// raised BY the owner rather than by this caller (which violated
    /// `write_out_link_value`'s own single-raise invariant).
    ///
    /// `sim_output` is `None` for a non-simulated record or a simulated INPUT
    /// (whose `readValue` ran up-front); `skip_out` carries the IVOA
    /// Don't_drive veto so the SIOL write is suppressed exactly as the real
    /// device write would be.
    ///
    /// Kept as its own `async fn` so the `EpicsValue` it reads out of the
    /// record never enters `process_record_with_links_inner`'s async state —
    /// that future is polled one frame deeper per FLNK hop, unbounded as in C,
    /// and bloating it overflows the stack sooner (the deep-chain tests).
    fn write_simulated_output_siol(
        &self,
        rec: &Arc<RecordCell>,
        sim_output: &Option<(crate::server::record::ParsedLink, i16, bool)>,
        skip_out: bool,
        src: super::links::OutLinkSrc<'_>,
        visited: &mut ProcStack,
    ) {
        let Some((siol, _sims, raw_mode)) = sim_output else {
            return;
        };
        // IVOA Don't_drive veto (C skips `writeValue` entirely) and a
        // non-writable SIOL (empty / constant — C `dbPutLink` no-op) both
        // suppress the write.
        if skip_out || !siol.is_writable_out_link() {
            return;
        }
        // The record's own OUT value (RAW: RVAL) — matching C `writeValue`
        // (`dbPutLink(&prec->siol, ..., &prec->oval)`), so the SIOL redirect
        // sends exactly what the real OUT link would have.
        let value = {
            let instance = rec.read();
            if *raw_mode {
                instance
                    .record
                    .get_field("RVAL")
                    .or_else(|| instance.record.val())
            } else {
                instance.record.output_link_value()
            }
        };
        if let Some(value) = value {
            self.write_out_link_value(
                rec,
                siol,
                value,
                super::links::OutLinkSrc {
                    field: "SIOL",
                    ..src
                },
                visited,
            );
        }
    }

    /// **C `dbTryGetLink`** (`dbLink.c:307-315`) — the bare `lset->getValue`
    /// dispatch, classified into the three outcomes C's `(status, buffer)` pair
    /// can carry (see [`crate::server::recgbl::simm::LinkFetch`]) and carrying
    /// the source-alarm tail, but WITHOUT `setLinkAlarm`.
    ///
    /// Only the two readers whose C really is `dbTryGetLink`-shaped call this
    /// directly ([`Self::rec_gbl_get_simm`] and swait's `recDynLinkGet` DOL);
    /// every other process-time read is a C `dbGetLink` and goes through
    /// [`Self::db_get_link`], which owns the failure alarm.
    ///
    /// The raw [`Self::read_link_value_no_process`] collapses two of them: it
    /// hands back the CONSTANT link's parsed text as if the link had delivered
    /// it this cycle, and `None` both for "constant with nothing to give" and
    /// for "the read failed". C keeps them apart — `dbConstGetValue`
    /// (`dbConstLink.c:219-225`) returns SUCCESS and writes nothing, because a
    /// constant's value was already loaded into the record's buffer at
    /// `init_record`. Every gate downstream (simulation mode, DISA, TSE, SELN)
    /// hangs off that distinction, so every one of them reads through here and
    /// the constant reaches the record only through the init-seed owner
    /// ([`Self::rec_gbl_init_constant_links`] / [`Self::rec_gbl_init_simm`]).
    /// The read CARRIES the source alarm: C's `dbGetLink` on a DB link ends in
    /// `dbDbGetValue`'s inheritance tail (`dbDbLink.c:228-232`), so every link a
    /// record reads at process time — INP, DOL, SDIS, TSEL, SELL, SIML, SIOL —
    /// folds an `MS` source's severity into the reader. That tail runs HERE, in
    /// the read primitive itself, through the single inheritance owner
    /// ([`Self::input_link_inheritance`]): a caller cannot drop it, because a
    /// caller never sees the alarm. Dropping it is exactly how DOL, SIML and
    /// SIOL came to lose MS while INP kept it.
    ///
    /// softIoc (`SRC0` in MAJOR): `SDIS="SRC0 MS"`, `TSEL="SRC0 MS"`,
    /// `SIML="SRC0 MS"`, `SIOL="SRC0 MS"` and `DOL="SRC0 MS"` (closed-loop) all
    /// leave the reader MAJOR/LINK; without `MS`, all leave it NO_ALARM. The
    /// one read C does NOT run the tail on is the `TSEL="SRC.TIME"` form
    /// (`recGbl.c:316-321` calls `dbGetTimeStampTag`, not `dbGetLink`) — and
    /// that branch does not come through here. EVERY other TSEL form falls
    /// through to `dbGetLink` at `recGbl.c:322`, so it does.
    pub(crate) fn db_try_get_link(
        &self,
        reader: &Arc<RecordCell>,
        link: &crate::server::record::ParsedLink,
    ) -> crate::server::recgbl::simm::LinkFetch {
        // A constant or unset link has no source, so this whole read is C's
        // `dbConstGetValue`: status 0, nothing stored, nothing to inherit.
        // Every record carries several — SDIS and TSEL at minimum — and each
        // one otherwise spent the read, a reader-name resolution and two lock
        // acquisitions per cycle to arrive back at `NoData`.
        if crate::server::recgbl::simm::is_constant(link) {
            return crate::server::recgbl::simm::LinkFetch::NoData;
        }
        let (fetch, alarm) = self.read_link_with_alarm(link);
        self.inherit_link_severity(reader, link, alarm);
        fetch
    }

    /// **C `dbGetLink`** (`dbLink.c:324-340`) — [`Self::db_try_get_link`] plus the
    /// failure effect C attaches to it, because in C the two are ONE function:
    ///
    /// ```c
    /// status = dbTryGetLink(plink, dbrType, pbuffer, pnRequest);
    /// if (status == S_db_noLSET) return -1;
    /// if (status) setLinkAlarm(plink);
    /// ```
    ///
    /// `setLinkAlarm` is `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "field %s",
    /// dbLinkFieldName(plink))` — unconditional on failure, independent of the
    /// link's `MS` class, and carrying the LINK FIELD's own name as the AMSG. It
    /// is NOT the severity-inheritance tail [`Self::inherit_link_severity`] runs:
    /// that propagates the SOURCE's severity on a SUCCESSFUL read, and a link
    /// with no `MS` inherits nothing at all.
    ///
    /// The alarm lives HERE, in the read, and not in the caller, because that is
    /// where C puts it. Leaving it to each caller is what let SDIS, TSEL, DOL,
    /// NVL, SELL and SUBL go silent on a dead link while SIML, SIOL, INP and the
    /// `ReadDbLink` executor — the callers that happened to remember — did not.
    /// One uniform rule replaces six chances to forget.
    ///
    /// `link_field` is C's `dbLinkFieldName(plink)`: a `struct link` knows its own
    /// field name, a [`ParsedLink`](crate::server::record::ParsedLink) does not, so
    /// the caller spells it.
    ///
    /// Use [`Self::db_try_get_link`] for the reads whose C is NOT `dbGetLink` —
    /// `recGblGetSimm`'s SIML read (`dbTryGetLink`, which bypasses `setLinkAlarm`
    /// and writes `nsta` itself, `recGbl.c:453-454`) and swait's output-time DOL
    /// (`recDynLinkGet`, `swaitRecord.c:767`).
    pub(crate) fn db_get_link(
        &self,
        reader: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
    ) -> crate::server::recgbl::simm::LinkFetch {
        let fetch = self.db_try_get_link(reader, link);
        if matches!(fetch, crate::server::recgbl::simm::LinkFetch::Failed) {
            let mut instance = reader.write();
            crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, link_field);
        }
        fetch
    }

    /// [`Self::db_get_link`] for an INPUT link — same classification, same
    /// `setLinkAlarm`, but the PP rule applies first: C `dbGetLink` on a
    /// `ProcessPassive` DB link processes the passive source before reading it.
    /// Used by sel's NVL→SELN read and the closed-loop DOL read.
    pub(crate) fn db_get_input_link(
        &self,
        reader: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        visited: &mut ProcStack,
    ) -> crate::server::recgbl::simm::LinkFetch {
        if let crate::server::record::ParsedLink::Db(db) = link {
            self.process_passive_db_source(db, visited);
        }
        self.db_get_link(reader, link_field, link)
    }

    /// Apply the reader's declared `dbrType` request
    /// ([`Record::input_link_request`](crate::server::record::Record::input_link_request))
    /// to one delivered link value — C's `dbGetLink(plink, dbrType, ...)`
    /// second argument, which the generic fetch paths never passed: they
    /// delivered the source's native value and let the target field coerce
    /// blind, turning a `DBR_STRING` request at an ENUM/MENU source into
    /// index digits (epics-base#183).
    ///
    /// The source is resolved with NO record lock held (the
    /// [`Self::read_db_link_into_field`] rule: a self-referencing link
    /// would otherwise re-enter this record's own gate), and only when the
    /// fetch actually delivered a value. `None` from the record is C's
    /// `default: break` — no read — mapped to `NoData`; a conversion the
    /// source cannot satisfy is a FAILED read (C's non-zero status).
    ///
    /// The second return is whether the reader asked for a STRING class: such a
    /// value bypasses the store's `to_f64` funnel, because that funnel IS the
    /// `DBR_DOUBLE` request of the calc-class records (`calcRecord.c:434`), not
    /// a rule of the store.
    fn convert_link_fetch(
        &self,
        rec: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        fetch: crate::server::recgbl::simm::LinkFetch,
    ) -> (crate::server::recgbl::simm::LinkFetch, bool) {
        let instance = rec.read();
        let request = instance.record.input_link_request(link_field);
        let mut fetch = fetch;
        let store_raw =
            self.convert_link_fetch_as(&*instance.record, link_field, link, request, &mut fetch);
        (fetch, store_raw)
    }

    /// [`Self::convert_link_fetch`] for a caller that already holds the
    /// record's request for the link — the multi-input fetch reads it for
    /// every set link under the cycle's entry guard, where C's
    /// `fetch_values` has it for free, instead of taking the record's lock
    /// once per link to ask.
    #[inline]
    fn convert_link_fetch_as(
        &self,
        record: &dyn crate::server::record::Record,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        request: crate::server::record::InputLinkRequest,
        fetch: &mut crate::server::recgbl::simm::LinkFetch,
    ) -> bool {
        use crate::server::recgbl::simm::LinkFetch;
        use crate::server::record::{InputLinkRequest, LinkReadAs};
        // A native request converts nothing — C's `dbGet` with the field's
        // own `dbrType` is a copy — so the fetch is left where it is, and
        // the two tests that settle that are the whole of what the common
        // path pays: the conversion itself is a frame of its own.
        if let InputLinkRequest::As(LinkReadAs::Native) = request {
            return false;
        }
        if !matches!(fetch, LinkFetch::Value(_)) {
            return false;
        }
        self.convert_link_value(record, link_field, link, request, fetch)
    }

    /// [`Self::convert_link_fetch_as`] past its gates: `fetch` holds a value
    /// and the request is not native.
    fn convert_link_value(
        &self,
        record: &dyn crate::server::record::Record,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        request: crate::server::record::InputLinkRequest,
        fetch: &mut crate::server::recgbl::simm::LinkFetch,
    ) -> bool {
        use crate::server::recgbl::simm::LinkFetch;
        use crate::server::record::LinkReadAs;
        let LinkFetch::Value(value) = std::mem::replace(fetch, LinkFetch::NoData) else {
            unreachable!("gated by convert_link_fetch_as");
        };
        match self.link_read_as(record, link_field, link, request) {
            None => false,
            Some(read_as) => {
                let raw = matches!(
                    read_as,
                    LinkReadAs::String | LinkReadAs::CharArrayAsString { .. }
                );
                match self.apply_link_read_as(link, read_as, value) {
                    Some(v) => {
                        *fetch = LinkFetch::Value(v);
                        raw
                    }
                    None => {
                        *fetch = LinkFetch::Failed;
                        false
                    }
                }
            }
        }
    }

    /// **C `dbGetLink` for a caller that folds its MS tail in later** —
    /// [`Self::db_get_link`] read, converted and alarmed, but with the
    /// source alarm handed back instead of applied.
    ///
    /// The multi-input fetch loops (INPA..INPL and sCalcout's INAA..INLL) read
    /// many links with the record's write lock released and apply their MS
    /// inheritance together at the end, so they cannot use the inline owner.
    /// They can still not be the place the `setLinkAlarm` decision lives: that
    /// is what left `record(calc,"C"){field(INPA,"NOSUCH")}` publishing
    /// NO_ALARM where C publishes INVALID/LINK with AMSG `field INPA`.
    ///
    /// Returns `(fetch, source alarm, reader-asked-for-a-string-class)`.
    fn db_get_link_deferred(
        &self,
        rec: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        target: Option<&crate::server::record::record_instance::ResolvedTarget>,
        request: crate::server::record::InputLinkRequest,
    ) -> (
        crate::server::recgbl::simm::LinkFetch,
        Option<super::links::SourceAlarm>,
        bool,
    ) {
        let (fetch, alarm, store_raw) =
            self.db_try_get_link_deferred(rec, link_field, link, target, request);
        if matches!(fetch, crate::server::recgbl::simm::LinkFetch::Failed) {
            let mut instance = rec.write();
            crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, link_field);
        }
        (fetch, alarm, store_raw)
    }

    /// The `dbTryGetLink` twin of [`Self::db_get_link_deferred`] — same read
    /// and conversion, no `setLinkAlarm`. swait's `fetch_values`
    /// (`swaitRecord.c:702`) reads INAA..INPL with `recDynLinkGet`, which has
    /// no such effect; its failure is answered by `recGblSetSevr(READ_ALARM,
    /// INVALID_ALARM)` at `swaitRecord.c:413`.
    fn db_try_get_link_deferred(
        &self,
        rec: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        target: Option<&crate::server::record::record_instance::ResolvedTarget>,
        request: crate::server::record::InputLinkRequest,
    ) -> (
        crate::server::recgbl::simm::LinkFetch,
        Option<super::links::SourceAlarm>,
        bool,
    ) {
        let (mut fetch, alarm) = self.read_link_with_alarm_at(link, target);
        let store_raw = {
            let instance = rec.read();
            self.convert_link_fetch_as(&*instance.record, link_field, link, request, &mut fetch)
        };
        (fetch, alarm, store_raw)
    }

    /// The `Option`-shaped twin of [`Self::convert_link_fetch`] for the
    /// single-INP soft path, whose reader deals in `Option<EpicsValue>`:
    /// a conversion (or declaration) miss is `None`, which that path
    /// already classifies as a failed read of a real link (LINK alarm,
    /// VAL untouched — C `read_si` returning `dbGetLink`'s status).
    /// **The one owner of C's `dbGetLink` `dbrType` argument** — the record's
    /// per-link request, with the SOURCE resolved only for the record types
    /// that let the source decide it.
    ///
    /// The source walk (`dbGetLinkDBFtype` / `dbGetNelements`) is a records-map
    /// lookup plus the TARGET record's read lock, and it must run with no
    /// reader lock held — a self-referencing link would otherwise re-enter this
    /// record's own gate — so it cannot be deferred inside the record's answer.
    /// Asking [`Record::input_link_request`](crate::server::record::Record::input_link_request) first is what keeps it off the
    /// cycle of every record type whose C switch is on the link FIELD alone,
    /// which is all of them but `sseq`, `aSub`, `lsi` and `lso`.
    fn input_link_read_as(
        &self,
        rec: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
    ) -> Option<crate::server::record::LinkReadAs> {
        let instance = rec.read();
        let request = instance.record.input_link_request(link_field);
        self.link_read_as(&*instance.record, link_field, link, request)
    }

    /// [`Self::input_link_read_as`] with the record's request already in
    /// hand. The `FromSource` arm still resolves the source and asks the
    /// record for its answer, as before.
    fn link_read_as(
        &self,
        record: &dyn crate::server::record::Record,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        request: crate::server::record::InputLinkRequest,
    ) -> Option<crate::server::record::LinkReadAs> {
        use crate::server::record::InputLinkRequest;
        match request {
            InputLinkRequest::As(read_as) => Some(read_as),
            // C's `default:` arm — the record's switch has no case for this
            // link, so `dbGetLink` is never called.
            InputLinkRequest::NotRead => None,
            InputLinkRequest::FromSource => {
                let source = self.resolve_out_target(link);
                record.input_link_read_as_from_source(link_field, &source)
            }
        }
    }

    fn typed_input_value(
        &self,
        rec: &Arc<RecordCell>,
        link_field: &str,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
    ) -> Option<EpicsValue> {
        let read_as = self.input_link_read_as(rec, link_field, link)?;
        self.apply_link_read_as(link, read_as, value)
    }

    /// C `dbDbGetValue`'s tail, applied to the reader: the ONE place a
    /// process-time link read folds its source's alarm in. Computes the
    /// `(MS class, source alarm)` pair through the inheritance owner with no
    /// record lock held, then applies it under a brief write lock.
    fn inherit_link_severity(
        &self,
        reader: &Arc<RecordCell>,
        link: &crate::server::record::ParsedLink,
        alarm: Option<super::links::SourceAlarm>,
    ) {
        if let Some(alarm) = alarm {
            let mut instance = reader.write();
            self.fold_input_link_alarm(&mut instance.common, reader, link, alarm);
        }
    }

    /// C `recGblGetSimm` (`recGbl.c:448-457`) — **the single owner of the
    /// SIMM transition at process time**, and the only site allowed to write
    /// SIMM from SIML.
    ///
    /// ```c
    /// recGblSaveSimm(*psscn, poldsimm, *psimm);
    /// status = dbTryGetLink(psiml, DBR_USHORT, psimm, 0);
    /// if (status && !pcommon->nsev) pcommon->nsta = LINK_ALARM;
    /// recGblCheckSimm(pcommon, psscn, *poldsimm, *psimm);
    /// ```
    ///
    /// Called from `check_simulation_mode` on every `pact == FALSE` entry —
    /// C's `if (!prec->pact)` guard around it (aiRecord.c:475).
    ///
    /// Returns the SIML-read status the record's `readValue`/`writeValue` sees:
    /// `true` when the read FAILED. Only a record that declares
    /// [`Record::aborts_on_failed_siml_read`](crate::server::record::Record::aborts_on_failed_siml_read) (busy) acts on it — see that hook
    /// for why the other two families do not.
    pub(crate) fn rec_gbl_get_simm(
        &self,
        rec: &Arc<RecordCell>,
        siml: &crate::server::record::ParsedLink,
    ) -> bool {
        use crate::server::recgbl::simm::LinkFetch;
        // `recGblSaveSimm(*psscn, poldsimm, *psimm)` — latch the outgoing mode
        // BEFORE the SIML read can move SIMM.
        {
            let mut instance = rec.write();
            instance.rec_gbl_save_simm();
        }
        // `dbTryGetLink`: a CONSTANT (or unset) SIML delivers NOTHING here —
        // its value was loaded into SIMM once, at init (`rec_gbl_init_simm`).
        // So a `caput REC.SIMM YES` on a record with a constant SIML STAYS
        // YES; re-reading the constant every cycle (the pre-fix behaviour of
        // `read_link_value_no_process`) would stomp the operator's put back to
        // the constant on the very next process.
        let fetch = self.db_try_get_link(rec, siml);
        let failed = matches!(fetch, LinkFetch::Failed);
        match fetch {
            LinkFetch::Value(v) => {
                // `dbGetLink(&prec->siml, DBR_USHORT, &prec->simm)` — through the
                // coercion owner, source-type-chosen (see the DISA read above);
                // SIMM's storage here is the i16 carrier.
                let simm = v.to_dbf_i16().unwrap_or(0);
                let mut instance = rec.write();
                let _ = instance
                    .record
                    .put_field_internal("SIMM", EpicsValue::Short(simm));
            }
            // status 0, nothing written — SIMM keeps what init loaded.
            LinkFetch::NoData => {}
            // The read FAILED. Two C shapes, keyed on which SIML reader the
            // record's support uses (`Record::uses_recgbl_simm_helpers`):
            LinkFetch::Failed => {
                let mut instance = rec.write();
                if instance.record.uses_recgbl_simm_helpers() {
                    // `recGblGetSimm` (recGbl.c:453-454):
                    //     if (status && !pcommon->nsev) pcommon->nsta = LINK_ALARM;
                    // `dbTryGetLink` does NOT call `setLinkAlarm`, and this is a
                    // DIRECT write of `nsta` — NOT `recGblSetSevr`. So the record
                    // publishes STAT=LINK_ALARM with SEVR still NO_ALARM. That
                    // asymmetry is C's, quirk and all; reproduce it exactly.
                    if instance.common.nsev == crate::server::record::AlarmSeverity::NoAlarm {
                        instance.common.nsta = crate::server::recgbl::alarm_status::LINK_ALARM;
                    }
                } else {
                    // `busyRecord.c:399` / `swaitRecord.c:402` read SIML with a
                    // plain `dbGetLink`, whose failure path calls `setLinkAlarm`
                    // (dbLink.c:318-323) — a full
                    // `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "field %s")`.
                    crate::server::recgbl::rec_gbl_set_link_alarm(&mut instance.common, "SIML");
                }
            }
        }
        // `recGblCheckSimm(pcommon, psscn, *poldsimm, *psimm)` — a SIML-driven
        // SIMM transition swaps SCAN with SSCN exactly like a `caput REC.SIMM`
        // does. C runs it even on a FAILED read (recGbl.c:455 is past the
        // LINK_ALARM line), so the swap is not conditional on the status.
        self.apply_simm_scan_swap(rec);
        failed
    }

    /// Run C `recGblCheckSimm` on a record and hand the resulting scan move to
    /// the scan-index owner (`update_scan_index`) — the `scanDelete`/`scanAdd`
    /// pair inside it. The record lock is taken and released here: the
    /// scan-index update re-enters the database.
    pub(crate) fn apply_simm_scan_swap(&self, rec: &Arc<RecordCell>) {
        use crate::server::record::CommonFieldPutResult;
        let (name, result) = {
            let mut instance = rec.write();
            let name = instance.name.clone();
            let result = instance.rec_gbl_check_simm();
            (name, result)
        };
        if let CommonFieldPutResult::ScanChanged {
            old_scan,
            new_scan,
            phas,
        } = result
        {
            self.update_scan_index(&name, old_scan, new_scan, phas, phas);
        }
    }

    /// C `recGblInitSimm` (`recGbl.c:439-446`) plus the
    /// `recGblInitConstantLink(&prec->siol, …, &prec->sval)` that every
    /// SIML/SIOL-bearing `init_record` pairs with it (longinRecord.c:99-100,
    /// aiRecord.c:103-104, busyRecord.c:138, swaitRecord.c:663-670).
    ///
    /// A CONSTANT link hands its value to the record exactly ONCE, here, via
    /// `dbLoadLink` — at process time `dbGetLink` on a constant delivers
    /// nothing. This is the other half of the rule
    /// `Self::fetch_link` enforces; without it a `field(SIOL, "42")`
    /// would never reach SVAL at all.
    ///
    /// Must be called once per record, after its fields are applied — the
    /// `init_record(1)` sites (`ioc_builder`, `dbLoadRecords`).
    /// C `recGblInitConstantLink(&prec->inp, …, &prec->val)` /
    /// `dbLoadLinkArray(&prec->inp, prec->ftvl, prec->bptr, &nRequest)` — the
    /// ONE place a constant INP reaches a record.
    ///
    /// Every soft-channel INPUT device support runs this in its
    /// `init_record`: `devAiSoft.c:44`, `devLiSoft.c`, `devBiSoft.c`,
    /// `devI64inSoft.c`, `devMbbiSoft.c`, `devSiSoft.c`, `devEventSoft.c`
    /// (scalars, via `recGblInitConstantLink`), and `devAaiSoft.c:57`,
    /// `devWfSoft.c:42`, `devSASoft.c` (arrays, via `dbLoadLinkArray`). The
    /// raw variants (`devAiSoftRaw.c`, `devBiSoftRaw.c`, `devMbbiSoftRaw.c`)
    /// load into RVAL instead and let the record's own RVAL→VAL conversion
    /// run — hence the [`Record::raw_soft_input`](crate::server::record::Record::raw_soft_input) arm, the same sink the
    /// process-time path uses for `Raw Soft Channel`.
    ///
    /// This is the other half of the rule
    /// [`PvDatabase::read_link_value_soft`](super::PvDatabase::read_link_value_soft) enforces (a constant
    /// delivers NOTHING at process): without the init load a `field(INP, "5")`
    /// ai would never see 5 at all; without the process-time skip the constant
    /// would clobber the record's VAL on every scan.
    ///
    /// Gated on soft DTYP because a hardware record's INP is a device ADDRESS,
    /// not a value — C only ever loads it in soft dev support.
    ///
    /// **This is THE init-seed owner.** Beyond the device-support INP above it
    /// applies the record's own `recGblInitConstantLink` table,
    /// [`Record::constant_init_links`](crate::server::record::Record::constant_init_links) — calc/calcout/sub/sel/aSub/scalcout/
    /// acalcout/transform `INPA..L → A..L`, sel `NVL → SELN`, fanout/dfanout/
    /// seq `SELL → SELN`, seq `DOLn → DOn`, aSub `SUBL → SNAM`, and the
    /// `DOL → VAL` seeds that also clear UDF. Every one of those links is
    /// dead at process time (the link layer returns `LinkFetch::NoData` for a
    /// constant), so this is the only place their values can arrive.
    ///
    /// Must be called once per record, after its fields are applied and both
    /// `init_record` passes have run (the record needs its final NELM/FTVL
    /// buffer before an array constant can land in it) — the `init_record(1)`
    /// sites (`ioc_builder`, `dbLoadRecords`). It also runs from
    /// `PvDatabase::add_record`, the creation sink every other path funnels
    /// through, so a record built programmatically (no `IocBuilder`) still has
    /// its constants seeded: in C there is no record in the database that
    /// `init_record` did not touch. Seeding twice is a no-op — both calls
    /// happen before any client can put.
    pub(crate) fn rec_gbl_init_constant_links(&self, rec: &Arc<RecordCell>) {
        let mut instance = rec.write();
        seed_constant_links(&mut instance);
    }
}

/// The body of the init-seed owner, over a locked record — shared by
/// [`PvDatabase::rec_gbl_init_constant_links`] and `PvDatabase::add_record`.
pub(crate) fn seed_constant_links(instance: &mut RecordInstance) {
    // The SECOND seat of C's `init_record` body, and so it takes the same
    // opening test: every step below sits BELOW `if (!pdset) { … return
    // S_dev_noDSET; }` in the C source it ports — the soft dset's constant
    // load, the record's own `recGblInitConstantLink` table (`aoRecord.c:112`),
    // and the tail plus tracker seed at `aoRecord.c:156-161`. A record whose
    // dset is NULL reaches none of them, which is why softIoc reads `MLST: 0`
    // on an `ai` whose DTYP nobody registered where the port read its VAL.
    if !instance.init_record_reaches_body() {
        return;
    }

    // 0. The long-string load, C `dbLoadLinkLS` — a lset entry of its own, NOT
    //    `recGblInitConstantLink`, and the only one that can write a
    //    long-string VAL: `lso` runs it on DOL (lsoRecord.c:82), `lsi`'s soft
    //    device support on INP (devLsiSoft.c:24). It replaces the scalar seeds
    //    below for those records — a long-string VAL takes no scalar put.
    if let Some(link_field) = instance.record.constant_ls_link() {
        // C binds `loadLS` to the INP link through the SOFT device support, so
        // a hardware DTYP loads nothing; DOL is in the record itself and is
        // never gated.
        let gated = link_field != "INP" || instance.common.dtyp.is_soft();
        let text = if link_field == "INP" {
            instance.common.inp.clone()
        } else {
            match instance.record.get_field(link_field) {
                Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
                _ => String::new(),
            }
        };
        if gated {
            if let Some(load) = crate::server::record::load_link_ls(&text) {
                // C's lso/lsi init tail: `if (prec->len) { … prec->udf = FALSE; }`
                // — a link that loaded (even the number case, whose LEN is 1
                // with an empty VAL) DEFINES the record.
                if instance.record.apply_ls_load(load) != 0 {
                    instance.common.udf = 0;
                }
            }
        }
        instance.record.init_record_tail();
        instance.record.seed_deadband_tracking();
        return;
    }

    // 1. The soft-channel device support's INP → VAL/RVAL load. It is DEVICE
    //    SUPPORT's `init_record` (`devAiSoft.c` &c), so it runs only on records
    //    that HAVE a DSET — `Record::input_read_by_device_support`. A record
    //    that reads its own INP (compress) gets no init load in C, and its
    //    constant therefore never reaches the record at all.
    if instance.common.dtyp.is_soft() && instance.record.input_read_by_device_support() {
        let inp = crate::server::record::parse_link_v2(&instance.common.inp);
        let mut loaded = false;
        if let Some(value) = crate::server::recgbl::simm::constant_load_value(&inp) {
            // Same sink the per-cycle soft-input apply uses, so the constant
            // lands in the field the link would have written: RVAL for `Raw
            // Soft Channel` (the record converts RVAL→VAL), VAL otherwise.
            // `RawSoftEntry::InitConstant` — the SoftRaw dsets do NOT mask the
            // init load (`devBiSoftRaw.c:57` calls `recGblInitConstantLink`
            // straight into RVAL; only `read_bi` applies MASK).
            let raw = if instance.common.dtyp.soft()
                == Some(crate::server::device_support::SoftDtyp::Raw)
            {
                instance
                    .record
                    .raw_soft_input(RawSoftEntry::InitConstant, value.clone())
            } else {
                None
            };
            loaded = match raw {
                Some(res) => res.is_ok(),
                None => instance.record.set_val(value).is_ok(),
            };
            // C: `if (recGblInitConstantLink(...)) prec->udf = FALSE;` — a
            // record whose value came from a constant link is DEFINED.
            if loaded {
                instance.common.udf = 0;
            }
        }
        // The FAILURE arm of the same dset `init_record`. `devWfSoft.c:39-51`
        // does not just skip a link it could not load — it ZEROES the element
        // count:
        //
        // ```c
        //     status = dbLoadLinkArray(&prec->inp, prec->ftvl, prec->bptr, &nelm);
        //     if (!status) { prec->nord = nelm; prec->udf = FALSE; }
        //     else          prec->nord = 0;
        // ```
        //
        // so the record's own `nord = (nelm == 1)` seed does not survive a
        // waveform whose INP is a real link or unset. Defaulted no-op.
        instance.record.soft_input_dset_init(loaded);
    }

    // 2. The record's own `recGblInitConstantLink` table, through the shared
    //    owner of "a CONSTANT link's text becomes the target field's value"
    //    (`record::rec_gbl_init_constant_link`) — the SAME load a runtime put to
    //    the link field re-runs from `special()`, so the two cannot drift.
    for seed in instance.record.constant_init_links() {
        let Some(value) =
            crate::server::record::rec_gbl_init_constant_link(&mut *instance.record, &seed)
        else {
            continue;
        };
        // C's UDF rule for a successful constant load is per record, and the two
        // shapes differ only in the NaN case:
        //   aoRecord.c:112-113 / dfanoutRecord.c:105-106 — `udf = isnan(val)`
        //   longoutRecord.c:113 / mbboRecord.c:133 / int64outRecord.c:110 —
        //                                            `udf = FALSE`
        // A NaN cannot survive the conversion into an integer target, so the
        // isnan test covers both: the value that reached the field is defined
        // unless it is NaN.
        let is_nan = value.to_f64().is_some_and(f64::is_nan);
        if seed.clears_udf && !is_nan {
            instance.common.udf = 0;
        }
    }

    // 3. C's `init_record` TAIL, which every record runs immediately AFTER its
    //    `recGblInitConstantLink` calls (`aoRecord.c:156-161`: `oval = pval =
    //    val; mlst = alst = lalm = val; oraw = rval; orbv = rbv`). It re-derives
    //    the record's init-time tracking state from the value the seed just
    //    loaded — a constant DOL of 5 leaves C's ao at OVAL=5, not 0
    //    (softIoc-verified) — so it belongs to the seed owner, not to a caller
    //    that may or may not remember it (the iocsh `dbLoadRecords` path did
    //    not).
    instance.record.init_record_tail();
    instance.record.seed_deadband_tracking();

    // C's init-time `db_post_events` run during iocInit, before any client can
    // subscribe, so they are observable by nobody. A seed put that made the
    // record MARK a field (sseq: seeding `STRn` re-derives `DOn`) must not leave
    // that mark standing for the first process cycle to emit — that would turn a
    // no-op C post into a real, late event. Drop the init-time marks.
    let _ = instance.record.take_cycle_posted_fields();
}

impl PvDatabase {
    pub(crate) fn rec_gbl_init_simm(&self, rec: &Arc<RecordCell>) {
        // The data guard is released (block close) before the scan-swap await
        // below (parking_lot guards are `!Send`).
        let siml_is_constant = {
            let mut instance = rec.write();
            // No SIMM field -> no simulation block -> nothing to init.
            if instance.resolve_field("SIMM").is_none() {
                return;
            }
            let link_of = |instance: &RecordInstance, field: &str| {
                instance.resolve_field(field).and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(crate::server::record::parse_link_v2(
                            s.as_str_lossy().as_ref(),
                        ))
                    } else {
                        None
                    }
                })
            };
            // C `recGblInitSimm` (`recGbl.c:441-445`) is one `if
            // (dbLinkIsConstant(psiml))` around ALL THREE steps — the
            // `recGblSaveSimm` latch, the `dbLoadLink`, and the
            // `recGblCheckSimm` scan swap. A record whose SIML names a PV gets
            // none of them: OLDSIMM keeps its dbd initial and SCAN is left
            // alone until the first `recGblGetSimm`. Guarding only the load
            // would be worse than guarding nothing — with the latch still
            // taken, `field(SIMM,"YES")` in the `.db` would then read
            // `simm != oldsimm` at the tail and swap a scan C never swaps.
            let siml = link_of(&instance, "SIML");
            // An unset SIML is a CONSTANT link (`dbConstLink.c`'s lset with a
            // NULL string), which is what a missing field means here.
            let siml_is_constant = siml
                .as_ref()
                .is_none_or(crate::server::recgbl::simm::is_constant);
            if siml_is_constant {
                instance.rec_gbl_save_simm();
                if let Some(v) = siml
                    .as_ref()
                    .and_then(crate::server::recgbl::simm::constant_load_value)
                {
                    let _ = instance.record.put_field_internal("SIMM", v);
                }
            }
            // `recGblInitConstantLink(&prec->siol, DBF_<sval>, &prec->sval)` — the
            // records with no SVAL (waveform/aai read into `bptr`, lsi into `val`)
            // load nothing here, exactly as their C `init_record` does.
            if instance.record.get_field("SVAL").is_some() {
                if let Some(siol) = link_of(&instance, "SIOL") {
                    if let Some(v) = crate::server::recgbl::simm::constant_load_value(&siol) {
                        let _ = instance.record.put_field_internal("SVAL", v);
                    }
                }
            }
            // `recGblCheckSimm(pcommon, psscn, *poldsimm, *psimm)`: a record loaded
            // with `field(SIML,"1")` starts in simulation, so its SCAN and SSCN are
            // already swapped by the time the IOC reaches runtime.
            siml_is_constant
        };
        if siml_is_constant {
            self.apply_simm_scan_swap(rec);
        }
    }

    /// Check simulation mode for a record. Returns
    /// `SimOutcome::Simulated` when a simulated INPUT handled the value (the
    /// caller still runs the forward-link tail),
    /// `SimOutcome::RedirectOutputToSiol` when a simulated OUTPUT needs the
    /// uniform body to run first, or `SimOutcome::NotSimulated` when normal
    /// processing should proceed.
    ///
    /// The SIM/SDLY continuation arms release the PACT the SDLY defer held (C
    /// `readValue`/`writeValue` continue with `pact = FALSE`), so the call also
    /// hands back the [`PactExit`] for that release — the put-notify parked on
    /// the SDLY window. The caller carries it to the cycle's `recGblFwdLink`
    /// tail; the release cannot silently drop it (`#[must_use]`), which is what
    /// stranded it here before.
    fn check_simulation_mode(
        &self,
        rec: &Arc<RecordCell>,
    ) -> (SimOutcome, crate::server::record::PactExit) {
        // Read SIML, SIMM, SIOL, SIMS, SDLY from the record
        let (siml_link, siol_link, sims, sdly, _rtype, is_input, input_stage, pact_held) = {
            let instance = rec.read();
            // The entry gate is the SIM BLOCK's own marker — the SIMM field.
            // C's `readValue`/`writeValue` exists only on a record whose dbd
            // declares SIMM, and it dispatches on SIMM alone; the SIML/SIOL
            // links are read INSIDE that dispatch, never as a precondition for
            // it. Gating on "SIML and SIOL are both empty" (the pre-fix gate)
            // made `caput REC.SIMM 1` + `caput REC.SVAL 42` — simulate against
            // a constant, the standard idiom — a complete no-op on every
            // record, because an unset SIOL is exactly the case C serves from
            // SVAL (R12-61).
            //
            // It is asked FIRST, and of the record's DECLARATION. It used to be
            // asked fifth, by `resolve_field("SIMM")`, after SIML, SIOL, SIMS
            // and SDLY had each been resolved by name — so every calc, sub,
            // aSub, sel, seq and fanout in a database paid four full field
            // scans per process cycle to reach a gate that was always going to
            // turn it away.
            if !instance.declares_simulation() {
                return (
                    SimOutcome::NotSimulated,
                    instance.pact_exit_without_release(),
                );
            }
            let rtype = instance.record.record_type().to_string();
            // swait: the simulation replaces the record's input STAGE, not its
            // whole cycle. Declared by the record, not by a type-name list —
            // the classification is a property of where C put the SIOL read.
            let input_stage = instance.record.simulation_substitutes_input_stage();
            // C `prec->pact` at process entry — the value every readValue/
            // writeValue simulation guard keys on. The framework holds the
            // `processing` flag across an async wait owned by PACT (the SDLY
            // defer, the ODLY/swait ReprocessAfter), and the entry guard in
            // `process_record_with_links_inner` lets only such a held
            // continuation reach this point with the flag set. A fresh cycle
            // reads `false`; so does a `pact=FALSE` delayed re-trigger that does
            // NOT own PACT (e.g. the bo HIGH one-shot, which re-enters via the
            // same token mechanism but returned `Complete`). So `is_processing()`
            // is the faithful analog of `prec->pact` — finer than "re-entered via
            // a token" (`is_continuation`), which conflates the PACT-owning
            // continuation with the pact=FALSE re-trigger.
            let pact_held = instance.is_processing();
            // Every input record whose DBD declares SIML/SIOL/SIMM/SIMS.
            // `mbbi`/`mbbiDirect` are input records: `mbbiRecord.c:125-126`
            // (and mbbiDirectRecord.c) declare SIML+SIOL, and
            // `mbbiRecord.c:388-394` reads `dbGetLink(&prec->siol,
            // DBR_ULONG, &prec->sval)` then `rval = sval` — input
            // semantics. Omitting them sent a simulated mbbi down the
            // OUTPUT branch, which writes VAL out to SIOL instead of
            // reading the value in from it.
            //
            // `waveform`/`histogram` are also `readValue` inputs: both call
            // `readValue` at the START of `process()` and read SIOL in
            // (`waveformRecord.c:139`->`:351` `dbGetLink(&siol, ftvl, bptr)`;
            // `histogramRecord.c:209`->`:384` `dbGetLink(&siol, DBR_DOUBLE,
            // &sval)`). They are classified as inputs so a simulated cycle
            // reads SIOL rather than running the real device read and writing
            // VAL back out. Each lands the value where its own C `readValue`
            // lands it, through `Record::land_simulated_value`: `waveform` puts
            // the SIOL array in VAL (the default `set_val`), `histogram` puts
            // the scalar in SGNL and bins it (`histogramRecord.c:385` +
            // `:219` `add_count`), because its VAL is the bin-count array.
            //
            // `aai` is also a SIOL-reading input, but the SIOL read lives in
            // its soft DEVICE support, not the record support. `aaiRecord.c::
            // readValue` (:342) raises SIMM_ALARM then calls `read_aai`, and
            // `devAaiSoft.c::read_aai` (:89) reads
            // `simm == YES ? &prec->siol : &prec->inp` — i.e. SIMM=YES reads
            // the SIOL array into VAL, observably identical to `waveform`. (The
            // record-support `readValue` alone looks device-only, which is
            // misleading: the soft device is what redirects to SIOL, exactly as
            // `devAaoSoft.c::write_aao` (:56) writes `simm == YES ? &siol :
            // &out` for the `aao` OUTPUT twin.) So `aai` is classified as an
            // input alongside `waveform`; its SIOL array lands in VAL via the
            // same `set_val` path. `aao` is correctly EXCLUDED: its soft device
            // writes VAL out to SIOL, which the OUTPUT redirect (`!is_input` ->
            // `RedirectOutputToSiol` -> `write_simulated_output_siol`, VAL array
            // -> SIOL) already reproduces.
            let is_input = input_stage
                || matches!(
                    rtype.as_str(),
                    "ai" | "bi"
                        | "mbbi"
                        | "mbbiDirect"
                        | "longin"
                        | "int64in"
                        | "stringin"
                        | "lsi"
                        | "event"
                        | "waveform"
                        | "histogram"
                        | "aai"
                        // synApps `mca`: `mcaRecord.c:1097` `readValue` reads
                        // SIOL IN (`dbGetLink(&siol, ftvl, bptr, NULL,
                        // &nRequest)` with `nRequest = nmax`), exactly as
                        // `waveform` does. Omitting it sent a simulated mca
                        // down the OUTPUT branch, which writes VAL out to SIOL.
                        | "mca"
                );

            // Resolve the SIM-block fields through the INSTANCE, not through
            // `Record::get_field`. A record need not model every field its
            // `.dbd` declares, and `mca` deliberately does not model
            // SIML/SIOL — it leaves them to the framework
            // (`mca-rs/src/record/mod.rs:896-902`) — so their link text lives
            // in the instance's declared-override store and `record.get_field`
            // answers `None`. That read an empty SIOL on every simulated mca.
            // `resolve_field` is the single owner of "what does this field read
            // as": record state, dbCommon, virtual, override, `.dbd` initial.
            let siml = instance
                .resolve_field("SIML")
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let siol = instance
                .resolve_field("SIOL")
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            // SIMS is `DBF_MENU` (`mcaRecord.dbd:391`, `aiRecord.dbd.pod:511`
            // and every other), so read the INDEX and not one chosen carrier:
            // base record types answer `EpicsValue::Short` (`records/ai.rs:310`)
            // while `mca` answers `EpicsValue::Enum`
            // (`mca-rs/src/record/mod.rs:679`). Narrowing on `Short` here read
            // `mca`'s SIMS as the `unwrap_or(0)` default, so a simulated mca
            // raised `SIMM_ALARM` at NO_ALARM whatever the database asked for
            // — silently, since a menu index of 0 is a legal value.
            let sims = instance
                .resolve_field("SIMS")
                .and_then(|v| v.to_menu_index())
                .unwrap_or(0);
            // SDLY ("Sim. Mode Async Delay", DBF_DOUBLE, dbd initial
            // "-1.0"). Absent on record types whose SIMM group Rust does not
            // yet fully model — default to -1.0 (synchronous) so the async
            // branch is a no-op there, exactly as a record with the C default
            // behaves.
            let sdly = instance
                .resolve_field("SDLY")
                .and_then(|v| v.to_f64())
                .unwrap_or(-1.0);

            let siml_parsed = crate::server::record::parse_link_v2(siml.as_str_lossy().as_ref());
            // SIOL is `DBF_INLINK` on an input record (`aiRecord.dbd.pod:492`)
            // and `DBF_OUTLINK` on an output one (`aoRecord.dbd.pod:551`), so
            // its modifier mask (`dbStaticLib.c:2380-2391`) follows the same
            // direction split — CP/CPP is discarded on the output side.
            let siol_parsed = crate::server::record::parse_link_field(
                siol.as_str_lossy().as_ref(),
                if is_input {
                    crate::server::record::LinkFieldType::In
                } else {
                    crate::server::record::LinkFieldType::Out
                },
            );

            (
                siml_parsed,
                siol_parsed,
                sims,
                sdly,
                rtype,
                is_input,
                input_stage,
                pact_held,
            )
        };

        // Read SIML -> update SIMM, but only when PACT is not held. C resolves
        // the simulation mode in `recGblGetSimm` (`dbGetLink(&prec->siml,
        // DBR_USHORT, &prec->simm, 0, 0)`, reads the SIML link for any type)
        // guarded by `if (!prec->pact)` (aiRecord.c:475 / aoRecord.c:558): SIMM
        // is latched whenever the record re-enters with PACT held and is
        // re-resolved on every `pact=FALSE` entry. Gate the re-read on
        // `!pact_held` to match exactly: on the SDLY async continuation (PACT
        // held) the latch holds, so a SIML source that flips during the delay
        // cannot switch the deferred SIOL round-trip into a real device read;
        // on a `pact=FALSE` delayed re-trigger (the bo HIGH one-shot) the
        // re-resolve runs, matching C's fresh `recGblGetSimm`. The non-held
        // entry persists SIMM via `put_field` below, so a later held
        // continuation reads it back latched. (The pre-fix port only read a
        // `ParsedLink::Db` SIML, ignoring a CA/PVA/constant source.)
        //
        // The read itself goes through the SIMM transition owner
        // (`rec_gbl_get_simm`, C `recGblGetSimm`), which is the ONLY site that
        // writes SIMM.
        if !pact_held {
            let siml_read_failed = self.rec_gbl_get_simm(rec, &siml_link);
            // W10-E5. `busyRecord.c:399-401` returns from `writeValue` on a
            // failed SIML read — BEFORE `write_busy` and before the SIOL
            // `dbPutLink`. So C never reaches the `switch (prec->simm)` below:
            // no device write, no SIOL redirect, no SIMM_ALARM. The LINK_ALARM
            // that `dbGetLink`'s `setLinkAlarm` raised inside `rec_gbl_get_simm`
            // is the cycle's only simulation alarm.
            //
            // Only a record that declares it aborts takes this path — busy. The
            // recGblGetSimm records' equivalent `if (status) return status;` is
            // dead code (recGbl.c:456 always returns 0) and swait never tests
            // the status (swaitRecord.c:402), so both fall through to the switch
            // with SIMM at whatever value it already held.
            if siml_read_failed {
                // Reachable only under `!pact_held`, so no PACT to release —
                // and the exit is read under the same guard as the question,
                // since nothing sits between them.
                let (aborts, exit) = {
                    let instance = rec.read();
                    (
                        instance.record.aborts_on_failed_siml_read(),
                        instance.pact_exit_without_release(),
                    )
                };
                if aborts {
                    return (SimOutcome::AbortedBeforeWrite, exit);
                }
            }
        }

        // Check SIMM. The dispatch is the record's own C `switch (prec->simm)`,
        // whose legal arms are the choices of ITS SIMM menu — `resolve_sim_mode`
        // is the single owner of that fact.
        // PACT, if held, belongs to the continuation arm of the uniform body —
        // released there, with its park. Read beside the mode, under one guard.
        let (mode, no_sim_exit) = {
            let instance = rec.read();
            (
                crate::server::recgbl::simm::resolve_sim_mode(&*instance.record),
                instance.pact_exit_without_release(),
            )
        };

        if !mode.is_simulated() {
            return (SimOutcome::NotSimulated, no_sim_exit); // menuSimmNO
        }

        // C `default:` arm — `recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM)`
        // and NOTHING else: the device is not substituted, SIOL is never read or
        // written, SIMM_ALARM is not raised and VAL/UDF are untouched. Raise the
        // alarm here (into the PENDING pair, so the body/tail maximizes against
        // it exactly as C does) and tell the caller to suppress the record's I/O
        // stage. This is the arm a `SIMM = 2` (RAW) reaches on the 13 records
        // whose SIMM is `menu(menuYesNo)` — R11-C12 — and the arm ANY
        // out-of-menu SIMM reaches on all of them, since `recGblGetSimm`'s
        // `dbTryGetLink` writes SIMM with no menu validation at all.
        if mode == crate::server::recgbl::simm::SimMode::Illegal {
            let mut instance = rec.write();
            crate::server::recgbl::rec_gbl_set_sevr(
                &mut instance.common,
                crate::server::recgbl::alarm_status::SOFT_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
            );
            // Reachable with PACT held only on an SDLY continuation whose SIMM
            // was made illegal (by a `caput`) during the delay: C's `readValue`
            // re-reads SIMM only when `!pact`, so the continuation's switch sees
            // the new value and takes `default:` — which does NOT clear `pact`,
            // but the record's `process()` ends with `prec->pact = FALSE` on the
            // way out. Release it here for the same reason the YES/RAW branches
            // do (below and at the `Simulated` tail): the cycle ends, so the
            // record must be left idle. The release carries the put-notify
            // parked on the SDLY window out to the caller's tail.
            let exit = if pact_held {
                instance.leave_pact()
            } else {
                instance.pact_exit_without_release()
            };
            let is_output = !is_input;
            drop(instance);
            return (SimOutcome::IllegalMode { is_output }, exit);
        }

        // epics-base 7.0.7 (SIMM menu):
        //   1 = YES — read/write via SIOL using the cooked VAL
        //   2 = RAW — read/write via SIOL using the raw RVAL when the
        //             record carries one (ai/ao only); falls back to
        //             VAL when no RVAL is present. Mirrors the C
        //             implementation, which treats records lacking
        //             a raw value as "YES" since there's nothing
        //             else to copy.
        let raw_mode = mode == crate::server::recgbl::simm::SimMode::Raw;

        // SDLY async simulation — C `aiRecord.c::readValue` (488) /
        // `aoRecord.c::writeValue` (571): `if (prec->pact || prec->sdly < 0)`
        // takes the synchronous SIOL branch; otherwise (`!pact && sdly >= 0`)
        // it schedules `callbackRequestProcessCallbackDelayed(..., sdly)` and
        // sets `pact = TRUE`. Key the defer on the same `!pact_held && sdly >= 0`
        // as C: a non-held entry (fresh cycle, or a `pact=FALSE` re-trigger)
        // with a non-negative SDLY defers the whole SIOL round-trip (input read
        // OR output write — both C paths share this branch) by `SDLY` seconds
        // and holds PACT; the resulting PACT-held continuation falls through to
        // the synchronous branch below.
        if !pact_held && sdly >= 0.0 {
            // Reachable only under `!pact_held`: this is the arm that TAKES PACT.
            let exit = rec.read().pact_exit_without_release();
            return (
                SimOutcome::DeferRead(crate::runtime::time::duration_from_secs(sdly)),
                exit,
            );
        }

        // INPUT-STAGE record (swait). C `swaitRecord.c:415-422`:
        //
        // ```c
        // } else {      /* SIMULATION MODE */
        //     status = dbGetLink(&(pwait->siol),DBR_DOUBLE,&(pwait->sval),0,0);
        //     if (status==0) {
        //         pwait->val=pwait->sval;
        //         pwait->udf=FALSE;
        //     }
        //     recGblSetSevr(pwait,SIMM_ALARM,pwait->sims);
        // }
        // ```
        //
        // The read substitutes `fetch_values()` + `calcPerform()` and nothing
        // else, so this performs exactly those four lines and hands the cycle
        // back: the OOPT switch, `execOutput`, the monitors and the forward link
        // all still come from the record's own `process()`. SIMM_ALARM goes into
        // the PENDING alarm (`rec_gbl_set_sevr` is C's MAXIMIZE) before the body
        // runs, so a body-raised alarm maximizes against it exactly as in C.
        if input_stage {
            // C `swaitRecord.c:416` reads SIOL with a plain `dbGetLink`, so a
            // FAILED read runs `setLinkAlarm` (dbLink.c:322) inside the read —
            // LINK_ALARM/INVALID with AMSG "field SIOL", raised BEFORE the
            // SIMM_ALARM below because that is swait's order (`dbGetLink` at
            // `swaitRecord.c:416`, then `recGblSetSevr(SIMM_ALARM, sims)` at
            // `:421`) — the opposite of the base records. `rec_gbl_set_sevr*` is
            // strict-greater, so with `SIMS = INVALID` the LINK_ALARM raised
            // first WINS the tie here and swait publishes
            // STAT=LINK/AMSG="field SIOL", where a longin publishes STAT=SIMM.
            // Compiled C confirms both.
            let fetch = self.db_get_link(rec, "SIOL", &siol_link);
            let mut instance = rec.write();
            // C `:417-420` — `if (status == 0) { val = sval; udf = FALSE; }`.
            // A CONSTANT (or unset) SIOL is `status == 0` with SVAL untouched
            // (`dbConstGetValue`), so it still copies SVAL into VAL; only a
            // FAILED read changes neither VAL nor UDF. The SIMM_ALARM below is
            // unconditional either way.
            if fetch.is_ok() {
                if let crate::server::recgbl::simm::LinkFetch::Value(v) = fetch {
                    let sval = EpicsValue::Double(v.to_f64().unwrap_or(0.0));
                    let _ = instance.record.put_field_internal("SVAL", sval);
                }
                if let Some(sval) = instance.record.get_field("SVAL") {
                    let _ = instance.record.land_simulated_value(sval);
                }
                instance.common.udf = 0;
            }
            let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
            crate::server::recgbl::rec_gbl_set_sevr(
                &mut instance.common,
                crate::server::recgbl::alarm_status::SIMM_ALARM,
                sev,
            );
            // swait keeps the cycle going through the uniform body; a held PACT
            // is released at its continuation arm, with its park. Mint the
            // token from the write guard already held — parking_lot is not
            // reentrant, so a fresh `rec.read()` here deadlocks.
            let exit = instance.pact_exit_without_release();
            return (SimOutcome::SimulatedInputStage, exit);
        }

        // OUTPUT record: C `writeValue` substitutes the device write with the
        // SIOL write, but it runs at the END of `process()` — after the body
        // has computed OVAL (OROC) and armed any record state machine (bo HIGH
        // momentary reset). The output write therefore CANNOT be done here, up
        // front, the way the input read can: doing so would write the stale
        // pre-body VAL and skip the body entirely (the divergence this path
        // closes). Hand the redirect back so the uniform flow runs the body and
        // the OUT-stage epilogue writes the fresh OVAL/RVAL to SIOL. Clear the
        // SDLY-held PACT first (C `writeValue` sets `pact = FALSE` on the sync
        // continuation) so the body runs on an idle record.
        if !is_input {
            let exit = if pact_held {
                let mut instance = rec.write();
                instance.leave_pact()
            } else {
                rec.read().pact_exit_without_release()
            };
            return (
                SimOutcome::RedirectOutputToSiol {
                    siol: siol_link,
                    sims,
                    raw_mode,
                },
                exit,
            );
        }

        // SIMM=YES(1) / SIMM=RAW(2): read the SIOL link into VAL/RVAL. C
        // `readValue` for a SIMM-mode INPUT record goes through `dbGetLink`,
        // which dispatches by link type — a local DB target, a CA target (a
        // bare non-local name or an explicit `CA`/`ca://` link), or a
        // constant. The pre-fix port special-cased a local `ParsedLink::Db`
        // SIOL only, so a non-local or external SIOL never read yet still
        // returned `Simulated` — the record froze with no value and no alarm.
        // Dispatch uniformly through the same link read owner as every other
        // link; the alarm/timestamp/notify tail below now runs for every SIOL
        // link type.
        //
        // Output records returned `RedirectOutputToSiol` above (the output
        // write follows the body), so only an INPUT record reaches here — its
        // `readValue` precedes the body, so the SIOL read + convert are done
        // in place and the caller short-circuits.
        let sim_posts = {
            // C `readValue` raises the SIMM severity at the TOP of the
            // `case menuYesNoYES:` arm — BEFORE the SIOL read
            // (`longinRecord.c:414` `recGblSetSevr(prec, SIMM_ALARM, prec->sims)`,
            // then `:416` `dbGetLink(&prec->siol, ...)`); likewise ai, mbbi,
            // histogram, waveform. That ORDER is load-bearing, not cosmetic:
            // `recGblSetSevr` is strict-greater, so when the SIOL read fails and
            // raises LINK_ALARM/INVALID (below), an already-pending
            // SIMM_ALARM/INVALID (`SIMS = INVALID`) WINS the tie and the record
            // publishes STAT=SIMM_ALARM — while with the default
            // `SIMS = NO_ALARM` nothing is pending, so LINK_ALARM/INVALID lands
            // and the broken SIOL is reported.
            //
            // Not every record raises it first, so the ORDER is the record's to
            // declare, not this site's: `mca` reads SIOL and only then raises
            // (`mcaRecord.c:1118` then `:1129`), so with `SIMS = INVALID` the
            // LINK_ALARM wins there and C publishes STAT=LINK_ALARM. That is
            // [`Record::raises_simm_after_read`]; the default is C's base-record
            // order and lands here, before the read.
            let raise_simm = |common: &mut crate::server::record::CommonFields| {
                let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
                crate::server::recgbl::rec_gbl_set_sevr(
                    common,
                    crate::server::recgbl::alarm_status::SIMM_ALARM,
                    sev,
                );
            };
            let simm_after_read = {
                let mut instance = rec.write();
                let after = instance.record.raises_simm_after_read();
                if !after {
                    raise_simm(&mut instance.common);
                }
                after
            };

            // Read from SIOL -> SVAL -> VAL/RVAL. Uniform across Db (with
            // locality fallback) / Ca / Pva / constant via `fetch_link`
            // (C `dbGetLink`), which keeps C's three outcomes apart: a value,
            // a CONSTANT link's "status 0 with the buffer untouched", and a
            // failure. Converted to the record's declared request: stringin
            // reads SIOL with `DBR_STRING` (`stringinRecord.c:208`), lsi via
            // `dbGetLinkLS` (`lsiRecord.c:244`).
            let fetch = self.db_get_link(rec, "SIOL", &siol_link);
            let (fetch, _raw) = self.convert_link_fetch(rec, "SIOL", &siol_link, fetch);
            // Resolved before the write guard below, which reaches
            // `sim_process_tail`'s posts — see the same resolve at the head of
            // `process_record_with_links_body`.
            let link_backing = self.resolve_link_backed_metadata_for_posts(rec);
            let link_backing = link_backing.as_link_backing();
            // The read itself raised C's `setLinkAlarm` (dbLink.c:321 ->
            // `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "field SIOL")`) on a
            // FAILED fetch. For a base record that is AFTER the SIMM_ALARM
            // above (`longinRecord.c:414` then `:416`), so with
            // `SIMS = INVALID` the equal-severity LINK_ALARM loses the tie and
            // STAT stays SIMM.
            //
            // The tail's `recGblGetTimeStampSimm` owes its TSEL read here, after
            // the SIOL read that stands in for the device read and before the
            // guard the store runs under.
            let tsel = self.read_tsel(rec);
            let mut instance = rec.write();

            // The other order (`mcaRecord.c:1118` then `:1129`): the read has
            // happened, so its LINK_ALARM is already pending and the
            // equal-severity SIMM_ALARM now loses the tie instead.
            if simm_after_read {
                raise_simm(&mut instance.common);
            }

            // C's SIOL read buffer is `&prec->sval` on every scalar SIML/SIOL
            // record (`longinRecord.c:416` `dbGetLink(&prec->siol, DBR_LONG,
            // &prec->sval)`, then `prec->val = prec->sval`). The records with
            // no SVAL field read straight into the value —
            // `waveform`/`aai` into `bptr` (waveformRecord.c:351), `lsi` into
            // `val` (lsiRecord.c:244) — so for them the fetched value IS the
            // landed value and a constant SIOL lands nothing.
            //
            // Routing the read through SVAL is what makes `caput REC.SIMM 1;
            // caput REC.SVAL 42` work (R12-61): the unset SIOL delivers no
            // data (status 0), and C's `val = sval` then publishes the SVAL
            // the operator wrote.
            let has_sval = instance.record.get_field("SVAL").is_some();
            let landed: Option<EpicsValue> = match &fetch {
                crate::server::recgbl::simm::LinkFetch::Value(v) => {
                    if has_sval {
                        // `put_field_internal` is the DBR-coercion owner
                        // (C `dbGetLink(DBF_<sval>)`).
                        let _ = instance.record.put_field_internal("SVAL", v.clone());
                        instance.record.get_field("SVAL")
                    } else {
                        Some(v.clone())
                    }
                }
                crate::server::recgbl::simm::LinkFetch::NoData => {
                    if has_sval {
                        instance.record.get_field("SVAL")
                    } else {
                        None
                    }
                }
                crate::server::recgbl::simm::LinkFetch::Failed => None,
            };

            if let Some(siol_val) = landed {
                let target_supports_raw = raw_mode && instance.record.get_field("RVAL").is_some();
                if target_supports_raw {
                    // PR #ac92e3e follow-up: SIMM=RAW on records
                    // with RVAL (ai/ao/etc.) writes the raw value
                    // into RVAL and runs the record's own
                    // process() so the LINR / ESLO / EOFF / ASLO
                    // / AOFF conversion chain computes VAL. The
                    // pre-fix path additionally called set_val
                    // here, which overwrote VAL with the raw
                    // count and silently bypassed conversion —
                    // the visible failure mode was "SIMM=RAW
                    // simulation returns counts instead of EGU".
                    //
                    // Coerce to RVAL's native DBR type before
                    // put_field — ai.RVAL is Long, but SIOL on a
                    // soft channel typically yields Double. Without
                    // the coerce step the put_field rejects with
                    // TypeMismatch and leaves RVAL at 0, so
                    // process() computes VAL = 0*ESLO + EOFF
                    // (the offset only), not the intended
                    // RAW*ESLO + EOFF.
                    let rval_type = crate::server::record::record_instance::declared_field_type_of(
                        instance.record.as_ref(),
                        "RVAL",
                    )
                    .unwrap_or(crate::types::DbFieldType::Long);
                    // C parity (aiRecord.c:495): `rval = (long)floor(sval)`.
                    // Rust `convert_to(Long)` truncates toward zero,
                    // diverging for negative bipolar-ADC raw values
                    // (sval=-1.5 → C: -2, Rust as-cast: -1).
                    // Floor explicitly when narrowing a float to
                    // an integer RVAL.
                    let coerced = match (&siol_val, rval_type) {
                        (EpicsValue::Double(d), crate::types::DbFieldType::Long) => {
                            EpicsValue::Long(d.floor() as i32)
                        }
                        (EpicsValue::Double(d), crate::types::DbFieldType::Int64) => {
                            EpicsValue::Int64(d.floor() as i64)
                        }
                        (EpicsValue::Float(d), crate::types::DbFieldType::Long) => {
                            EpicsValue::Long((*d as f64).floor() as i32)
                        }
                        (EpicsValue::Float(d), crate::types::DbFieldType::Int64) => {
                            EpicsValue::Int64((*d as f64).floor() as i64)
                        }
                        _ if siol_val.db_field_type() != rval_type => {
                            siol_val.convert_to(rval_type)
                        }
                        _ => siol_val,
                    };
                    let _ = instance.record.put_field("RVAL", coerced);
                    {
                        let inst = &mut *instance;
                        let ctx = inst.common.process_context();
                        inst.record.set_process_context(&ctx);
                    }
                    let _ = instance.record.process();
                } else {
                    // Records without RVAL fall back to SIMM=YES semantics: the
                    // SIOL value lands where C's `readValue` lands it — VAL for
                    // the base records (`longinRecord.c:417` `val = sval`), SGNL
                    // plus the bin increment for `histogram`
                    // (`histogramRecord.c:385` + `:219`). `land_simulated_value`
                    // is the single owner of that assignment; no conversion to
                    // run either way.
                    let _ = instance.record.land_simulated_value(siol_val);
                }
            }

            // Simulation alarm + per-field monitor tail — see
            // `sim_process_tail`. C raises `recGblSetSevr(prec, SIMM_ALARM,
            // prec->sims)` at the TOP of the SIMM branch, BEFORE the SIOL read
            // (longinRecord.c:413-414), and `process()` runs its
            // timestamp/alarm/monitor/forward-link tail whatever the read
            // returned — so the tail is unconditional, not gated on a value
            // having landed (R12-61). UDF is the one part C does gate on the
            // read's status (`if (status == 0) prec->udf = FALSE`), and a
            // constant SIOL is status 0.
            sim_process_tail(&mut instance, tsel, fetch.is_ok(), link_backing)
        };

        // C `readValue`/`writeValue` clears `pact` on the synchronous branch
        // (`prec->pact = FALSE`, aiRecord.c:496 / aoRecord.c:578). On the
        // SDLY continuation this releases the PACT held across the delay so the
        // forward-link tail and any subsequent foreign process see the record
        // idle (C posts `monitor()` + `recGblFwdLink` with pact already
        // FALSE). An entry that never held PACT (a fresh `sdly < 0` cycle, or a
        // `pact=FALSE` re-trigger) has nothing to release, so the clear is gated
        // on `pact_held` to avoid a needless write-lock there.
        let exit = if pact_held {
            let mut instance = rec.write();
            instance.leave_pact()
        } else {
            rec.read().pact_exit_without_release()
        };

        (SimOutcome::Simulated(sim_posts), exit)
    }
}

/// Shared tail of a simulated (`SIMM` != NO) process cycle — the part of
/// C `process()` that still runs when `readValue`/`writeValue` divert to
/// the SIOL (`aiRecord.c` and every SIML/SIMM-bearing record):
/// `checkAlarms`, `recGblResetAlarms` and `monitor()`, so the simulated value
/// still trips its own limit/state alarms and the alarms the SIMM branch
/// already raised maximize against them.
///
/// The tail raises NO alarm of its own. Every alarm a simulated cycle can
/// raise — SIMM_ALARM at SIMS on the YES/RAW arms, LINK_ALARM on a failed SIOL
/// `dbGetLink`, SOFT_ALARM/INVALID on the `default:` arm — is raised by
/// `check_simulation_mode` at the point C raises it, because
/// `recGblSetSevr` is a strict-greater MAXIMIZE and the ORDER of those calls
/// decides equal-severity ties (W10-E4). Folding the SIMM raise in here instead
/// silently reordered it after the SIOL read.
///
/// The posting masks are per-field, identical to the async-completion
/// path (`complete_async_record`) and `process_local`:
///
/// * the deadband-tracked field (default `VAL`) posts the classes that
///   actually fired — MDEL → `DBE_VALUE`, ADEL → `DBE_LOG`, alarm
///   movement → `DBE_ALARM` (C `recGblResetAlarms` `val_mask`); the
///   lsi/lso explicit change gate, MPST/APST always-post override, and
///   binary always-post route through the same hooks as those paths;
/// * `SEVR` posts `DBE_VALUE` only on a sevr change; `STAT`/`AMSG`
///   share a mask carrying `DBE_ALARM` (sevr/amsg moved) and/or
///   `DBE_VALUE` (stat moved); `ACKS` posts `DBE_VALUE` when the reset
///   raised it (recGbl.c:202-222);
/// * subscribed auxiliary fields post on value change with
///   `DBE_VALUE|DBE_LOG` plus the cycle's alarm bits (C change-detected
///   posts in each record's `monitor()`, e.g. ai `oraw != rval`), and
///   `UDF` rides along with the union of the cycle's posted classes.
///
/// The pre-fix tails (duplicated across the input and output SIMM
/// branches) pushed `VAL`/`SEVR`/`STAT` unconditionally with one shared
/// `DBE_VALUE|DBE_ALARM` mask and discarded the `rec_gbl_reset_alarms`
/// result — every simulated cycle re-sent unchanged alarm fields,
/// stamped `DBE_ALARM` on cycles whose alarm state never moved, and
/// bypassed the MDEL/ADEL deadband entirely.
fn sim_process_tail(
    instance: &mut RecordInstance,
    tsel: super::TselStamp,
    clear_udf: bool,
    backing: crate::server::database::LinkBacking<'_>,
) -> CyclePosts {
    let inst = &mut *instance;
    tsel.stamp(&inst.name, &mut inst.common, true);
    // C clears UDF only on a `status == 0` SIOL read (`longinRecord.c:418`) —
    // for most records a failed read leaves the record undefined. The array
    // records are the exception: their `process()` clears UDF itself, after
    // `readValue` returns and whatever its status (waveformRecord.c:144,
    // aaiRecord.c:174, aaoRecord.c:165). They declare that with
    // `clears_udf_unconditionally`, which is the record's own C, not a
    // framework choice.
    if clear_udf || instance.record.clears_udf_unconditionally() {
        instance.common.udf = 0;
    }

    {
        let inst = &mut *instance;
        inst.record.check_alarms(&mut inst.common);
    }
    instance.evaluate_alarms();
    let outcome = instance.monitor_cycle();
    publish_cycle(instance, &outcome.snapshot, backing, outcome.alarm_posts)
}

/// The single finalizer for a process cycle, for every path that can end one.
///
/// **Invariant:** a cycle that ENDS runs [`PvDatabase::end_process_cycle`]
/// exactly once — C reaches `recGblFwdLink` (`recGbl.c:295-302`) on every path
/// that ends a cycle, and only there does `putf` clear, the wait-set `leave`,
/// and the next queued `processNotify` restart. A non-zero record status does
/// not exempt a cycle: `subRecord.c:145-167` runs the whole tail on any status
/// but the documented async `1`.
///
/// A guard and not a call because the tail sits BELOW fallible exits —
/// `run_registered_subroutine()?` and `record.process()?` — that no explicit
/// site covers. `#[must_use]` on [`PactExit`] cannot stand in for it: that
/// lint fires on an unused *expression*, and each of those paths drops a
/// `let`-bound token, which warns about nothing.
///
/// Declared BEFORE any `rec.write()` in the cycle body, so Rust's
/// reverse-declaration drop order puts the record's DATA lock down first and
/// this second; `end_process_cycle` takes that lock itself, and
/// `parking_lot::RwLock` is not reentrant.
///
/// Two ways to leave without the `Drop` firing, both explicit at the site:
/// [`Self::take`] for a site that ends the cycle its own way, and
/// [`Self::hand_off_to_async_completion`] for the async-output early return,
/// which does not end the cycle at all — `complete_async_record_inner` does,
/// later, from its own token.
struct CycleEndGuard<'a> {
    db: &'a PvDatabase,
    name: &'a str,
    rec: &'a Arc<RecordCell>,
    exit: Option<crate::server::record::PactExit>,
}

impl<'a> CycleEndGuard<'a> {
    fn new(db: &'a PvDatabase, name: &'a str, rec: &'a Arc<RecordCell>) -> Self {
        Self {
            db,
            name,
            rec,
            exit: None,
        }
    }

    /// Fold a release into the cycle's token at the moment it is minted, so the
    /// exits between here and the tail carry it without a site of their own.
    fn merge_in(&mut self, other: crate::server::record::PactExit) {
        self.exit = Some(match self.exit.take() {
            Some(held) => held.merge(other),
            None => other,
        });
    }

    /// Disarm and hand the token to a site that ends the cycle itself.
    fn take(&mut self) -> crate::server::record::PactExit {
        self.exit
            .take()
            .unwrap_or_else(|| crate::server::record::PactExit::new(false))
    }

    /// Disarm because this cycle is NOT ending: the async-output `write_begin`
    /// re-entered PACT and spawned the completion, so
    /// `complete_async_record_inner` owns the tail and mints its own token from
    /// the record when the device write lands.
    fn hand_off_to_async_completion(&mut self) {
        self.exit = None;
    }
}

impl Drop for CycleEndGuard<'_> {
    fn drop(&mut self) {
        if let Some(exit) = self.exit.take() {
            self.db.end_process_cycle(self.name, self.rec, exit);
        }
    }
}

#[cfg(test)]
mod input_link_texts_tests {
    use super::InputLinkTexts;
    use crate::server::record::RecordInstance;
    use crate::server::record::record_instance::ParsedInputLink;
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;

    fn calc() -> RecordInstance {
        RecordInstance::new_boxed("C:ONE".to_string(), Box::new(CalcRecord::default()))
    }

    /// The boundaries are READ / NOT READ and SET / UNSET, one case each way.
    /// The pair matters because a sparse list answers both with "absent", and
    /// only the read flag separates them: a reader that confuses them either
    /// re-reads every link on a put path or reports a wired link as unset.
    #[test]
    fn an_unread_list_sends_every_reader_to_the_record() {
        let mut instance = calc();
        instance
            .record
            .put_field("INPB", EpicsValue::String("SRC:ONE.VAL".into()))
            .expect("INPB takes a link string");
        let texts = InputLinkTexts::none();
        assert_eq!(
            texts
                .link_at(Some(1), &instance, "INPB")
                .map(|l| pvname(&l)),
            Some("SRC:ONE".to_string()),
            "slot 1 is INPB, and an unread list must not answer it itself"
        );
        assert!(texts.link_at(Some(0), &instance, "INPA").is_none());
    }

    #[test]
    fn a_read_list_holds_the_set_links_and_only_those() {
        let mut instance = calc();
        instance
            .record
            .put_field("INPB", EpicsValue::String("SRC:ONE.VAL".into()))
            .expect("INPB takes a link string");
        let links = instance.record.multi_input_links();
        assert_eq!(links[1].0, "INPB", "slot 1 is INPB");
        let texts = InputLinkTexts::read_own(&instance);

        assert!(texts.is_set(1));
        assert_eq!(
            texts
                .link_at(Some(1), &instance, "INPB")
                .map(|l| pvname(&l)),
            Some("SRC:ONE".to_string())
        );
        assert!(!texts.is_set(0), "INPA is unwired");
        assert!(!texts.is_set(links.len() - 1), "the last is too");
        assert!(!texts.none_set());
        assert!(InputLinkTexts::read_own(&calc()).none_set());
    }

    #[test]
    fn the_parse_follows_the_text_the_record_holds() {
        let mut instance = calc();
        let put = |instance: &mut RecordInstance, text: &str| {
            instance
                .record
                .put_field("INPB", EpicsValue::String(text.into()))
                .expect("INPB takes a link string");
        };
        let parse = |instance: &mut RecordInstance| {
            let generation = instance.record.input_links_generation();
            ParsedInputLink::validated(
                &mut instance.parsed_inputs,
                &*instance.record,
                1,
                instance.record.multi_input_links(),
                generation,
            )
            .map(|entry| entry.parsed().clone())
        };
        put(&mut instance, "SRC:ONE.VAL");
        let first = parse(&mut instance).expect("INPB is set");
        let again = parse(&mut instance).expect("INPB is set");
        assert!(
            std::sync::Arc::ptr_eq(&again, &first),
            "the same text reuses the same parse"
        );
        let texts = InputLinkTexts::read_own(&instance);
        let shared = texts
            .link_at(Some(1), &instance, "INPB")
            .expect("INPB is set");
        assert!(
            std::sync::Arc::ptr_eq(&shared, &first),
            "a shared reader hands out the cached parse"
        );
        put(&mut instance, "SRC:TWO.VAL");
        assert_eq!(
            texts
                .link_at(Some(1), &instance, "INPB")
                .map(|l| pvname(&l)),
            Some("SRC:TWO".to_string()),
            "a stale cache entry is not handed out"
        );
        assert_eq!(
            parse(&mut instance).map(|l| pvname(&l)),
            Some("SRC:TWO".to_string())
        );
        put(&mut instance, "");
        assert!(
            parse(&mut instance).is_none(),
            "an emptied link is unset again"
        );
        assert!(
            InputLinkTexts::read_own(&instance)
                .link_at(Some(1), &instance, "INPB")
                .is_none()
        );
    }

    fn pvname(link: &crate::server::record::ParsedLink) -> String {
        match link {
            crate::server::record::ParsedLink::Db(db) => db.pvname(),
            other => panic!("expected a DB link, got {other:?}"),
        }
    }
}
