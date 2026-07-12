use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{CaError, CaResult};
use crate::runtime::sync::RwLock;
use crate::server::record::{AuxPostMask, InputFetchPolicy, NotifyWaitSet, RecordInstance};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::{PvDatabase, apply_timestamp};

/// C `sCalcoutRecord.c` `STRING_SIZE` (:198) — the 40-byte buffer behind every
/// string field a string-input link writes into. The text therefore carries at
/// most 39 bytes plus the NUL, which is what `epicsSnprintf(..., STRING_SIZE-1,
/// ...)` and `epicsStrSnPrintEscaped(..., STRING_SIZE-1, ...)` enforce in C.
const STRING_FIELD_MAX_LEN: usize = 39;

/// Cut a string-link value to the C field width (see [`STRING_FIELD_MAX_LEN`]).
fn truncate_string_field(s: PvString) -> PvString {
    let bytes = s.as_bytes();
    if bytes.len() <= STRING_FIELD_MAX_LEN {
        return s;
    }
    PvString::from_bytes(&bytes[..STRING_FIELD_MAX_LEN])
}

/// The DBR_STRING view of a [`Record::string_input_links`] source, C
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
        let mut visited = HashSet::new();
        db.process_record_continuation(&self.name, &mut visited, 0)
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
    pub async fn post_fields(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
    ) -> CaResult<Vec<String>> {
        match self.db() {
            Some(db) => db.post_fields(name, fields).await,
            None => Ok(Vec::new()),
        }
    }

    /// Resolve a link's target field type for the sseq link-status
    /// diagnostics — see [`PvDatabase::link_target_field_type`]. `None` if
    /// the link is constant / external / unresolvable, or the database is
    /// gone. (Distinct from the free `server::record::link_field_type`,
    /// which returns the link *class* `LinkType`, not the target's type.)
    pub async fn link_target_field_type(&self, link: &str) -> Option<crate::types::DbFieldType> {
        match self.db() {
            Some(db) => db.link_target_field_type(link).await,
            None => None,
        }
    }

    /// Read a link's value WITHOUT processing its source record — the C
    /// `dbGetLink` semantics. Parses `link` and reads it via
    /// [`PvDatabase::read_link_value_no_process`]; `None` if the link is
    /// constant-less / external-unresolvable or the database has been
    /// dropped. Used by module-crate records (e.g. std `throttle` SYNC →
    /// `SINP`→`VAL`) that must pull an input link from `special()` without
    /// triggering a process cycle.
    pub async fn read_link_value(&self, link: &str) -> Option<EpicsValue> {
        let db = self.db()?;
        let parsed = crate::server::record::parse_link_v2(link);
        db.read_link_value_no_process(&parsed).await
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
    pub async fn mint_async_token(&self, name: &str) -> Option<AsyncToken> {
        match self.db() {
            Some(db) => db.mint_async_token(name).await,
            None => None,
        }
    }

    /// Cancel an outstanding async re-entry — see
    /// [`PvDatabase::cancel_async_reentry`]. No-op if the database is gone.
    pub async fn cancel_async_reentry(&self, name: &str) {
        if let Some(db) = self.db() {
            db.cancel_async_reentry(name).await;
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
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.db()
            .map(|db| db.reprocess_on_notify(token, completion))
    }

    /// Issue a non-blocking put-with-completion to an OUT link — see
    /// [`PvDatabase::put_link_notify`]. `None` if the database is gone or
    /// the source record is missing.
    pub async fn put_link_notify(
        &self,
        record_name: &str,
        link_str: &str,
        value: EpicsValue,
    ) -> Option<crate::runtime::sync::oneshot::Receiver<()>> {
        match self.db() {
            Some(db) => db.put_link_notify(record_name, link_str, value).await,
            None => None,
        }
    }
}

/// C `dbNotifyAdd`: a will-process PP target (FLNK / OUT) joins the active
/// put-notify wait-set exactly once, so the completion waits for it. Called
/// only on the `!pact` (will-process) branch — a busy target sets RPRO and
/// does not join (matching the pre-fix drop behaviour), and the
/// `notify.is_none()` guard prevents a double-join when a record is reached
/// again within the same chain.
pub(super) fn join_put_notify(
    target: &mut RecordInstance,
    src_notify: Option<&Arc<NotifyWaitSet>>,
) {
    if target.notify.is_none() {
        if let Some(ws) = src_notify {
            target.notify = Some(ws.clone());
            ws.enter();
        }
    }
}

/// C `dbNotifyCompletion`: this record finished its contribution to the
/// put-notify (sync completion, async completion, or SDIS-disable bail).
/// Take its wait-set membership and leave — the completion oneshot fires on
/// the `leave` that empties the set. Idempotent: a record not in any
/// put-notify is a no-op.
fn complete_put_notify(inst: &mut RecordInstance) {
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
    instance.suppress_subroutine_run = ds.skip_run;
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

/// The alarm-field events `recGblResetAlarms` posts (recGbl.c:201-220), each
/// with its own per-field mask:
///
/// * `SEVR` — `DBE_VALUE`, ONLY when `prev_sevr != new_sevr`.
/// * `STAT`/`AMSG` — `stat_mask` = `DBE_ALARM` (on sevr- or amsg-change) |
///   `DBE_VALUE` (on stat-change).
/// * `ACKS` — `DBE_VALUE`, only when `stat_mask != 0` and `recGblResetAlarms`
///   raised it.
///
/// The single owner of these masks: every process cycle that commits alarms —
/// the full value-publication epilogue and the `CompleteAlarmOnly` cycle that
/// skips it (transform IVLA="Do Nothing") — posts through this, so the two
/// cannot drift.
fn alarm_field_posts(
    common: &crate::server::record::CommonFields,
    alarm_result: &crate::server::recgbl::AlarmResetResult,
) -> Vec<(&'static str, crate::server::recgbl::EventMask)> {
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
    let mut posts: Vec<(&'static str, EventMask)> = Vec::new();
    if sevr_changed {
        posts.push(("SEVR", EventMask::VALUE));
    }
    if !stat_mask.is_empty() {
        posts.push(("STAT", stat_mask));
        posts.push(("AMSG", stat_mask));
    }
    if alarm_result.acks_changed && !stat_mask.is_empty() {
        posts.push(("ACKS", EventMask::VALUE));
    }
    posts
}

/// The source record's put-propagation context for the forward-link tail.
/// C `processTarget` (dbDbLink.c:460-474) carries `psrc->putf` and
/// `psrc->ppn` to each target as a unit — the PUTF bit and the put-notify
/// wait-set always travel together. Bundled so the tail threads one
/// snapshot instead of a `(putf, notify)` pair.
#[derive(Clone, Copy)]
struct PutNotifyCtx<'a> {
    putf: bool,
    notify: Option<&'a Arc<NotifyWaitSet>>,
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
    Simulated,
    /// Simulated record whose simulation replaces only the INPUT STAGE of its
    /// body ([`Record::simulation_substitutes_input_stage`]) — swait. The SIOL
    /// read, the `VAL = SVAL` / `UDF = FALSE` write and the SIMM_ALARM raise
    /// have already happened here (C `swaitRecord.c:415-421`, which precedes the
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
    /// branch. The wrapped [`Duration`] is the `SDLY` delay.
    DeferRead(std::time::Duration),
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
        let mut visited = HashSet::new();
        self.process_record_with_links(name, &mut visited, 0).await
    }

    /// `process_record` variant for a caller that already
    /// owns the record's advisory write gate — the QSRV atomic group
    /// PUT applying a `+proc` member. The gate `Mutex` is not
    /// reentrant; the atomic group path MUST use this entry. See
    /// [`crate::server::database::PvDatabase::lock_records`].
    pub async fn process_record_already_locked(&self, name: &str) -> CaResult<()> {
        // Same delegation as [`Self::process_record`], but to the gate-held
        // engine entry since the caller already owns the advisory write gate.
        let mut visited = HashSet::new();
        self.process_record_with_links_already_locked(name, &mut visited, 0)
            .await
    }

    /// Process a record with full link handling (INP -> process -> alarms -> OUT -> FLNK).
    /// Uses visited set for cycle detection and depth limit.
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
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.process_record_with_links_inner(name, visited, depth, false, true, false)
                .await
        })
    }

    /// Driver-callback (`asyn:READBACK`) full-processing entry.
    ///
    /// The single owner of this entry is the I/O Intr wiring
    /// ([`crate::server::ioc_app::setup_io_intr`] and its `ioc_builder`
    /// twin): the spawned task processes a record because the driver
    /// fired an interrupt callback, not because of a client put / FLNK /
    /// scan. `device_callback = true` tells
    /// [`Self::process_record_with_links_inner`] that, for an *output*
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
        visited: &'a mut HashSet<String>,
        depth: usize,
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
            self.arm_readback_callback(name).await;
            let result = self
                .process_record_with_links_inner(name, visited, depth, false, true, true)
                .await;
            self.reconcile_readback_callback(name).await;
            result
        })
    }

    /// Arm the entry record's output driver-callback cycle before a readback
    /// process pass — see [`crate::server::device_support::DeviceSupport::arm_readback_callback`].
    async fn arm_readback_callback(&self, name: &str) {
        let canonical = self.resolve_alias(name).await;
        let key: &str = canonical.as_deref().unwrap_or(name);
        // Collect-then-act: clone the instance handle under a brief map read,
        // then drop the map lock before taking the per-record write. Never
        // hold `records.read()` across `rec.write()` — same lock discipline
        // as `add_breaktables` / `all_record_names`.
        let rec = {
            let records = self.inner.records.read().await;
            records.get(key).cloned()
        };
        if let Some(rec) = rec {
            if let Some(dev) = rec.write().await.device.as_mut() {
                dev.arm_readback_callback();
            }
        }
    }

    /// Reconcile the entry record's output driver-callback cycle after a
    /// readback process pass — see
    /// [`crate::server::device_support::DeviceSupport::reconcile_readback_callback`].
    async fn reconcile_readback_callback(&self, name: &str) {
        let canonical = self.resolve_alias(name).await;
        let key: &str = canonical.as_deref().unwrap_or(name);
        // Collect-then-act: clone the handle under a brief map read, drop the
        // map lock, then take the per-record write — see `arm_readback_callback`.
        let rec = {
            let records = self.inner.records.read().await;
            records.get(key).cloned()
        };
        if let Some(rec) = rec {
            if let Some(dev) = rec.write().await.device.as_mut() {
                dev.reconcile_readback_callback();
            }
        }
    }

    /// full-processing entry for a caller that already owns the
    /// record's advisory write gate via [`PvDatabase::lock_records`] —
    /// the QSRV atomic group GET/PUT and the pvalink atomic
    /// scan-on-update epoch. The advisory gate `Mutex` is not
    /// reentrant; a transaction owner holding `lock_records` over the
    /// member set MUST use this entry to scan a member record, or it
    /// would deadlock against its own epoch guard. Foreign (non-owner)
    /// callers must use [`Self::process_record_with_links`] so the gate
    /// is taken.
    pub fn process_record_with_links_already_locked<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.process_record_with_links_inner(name, visited, depth, false, false, false)
                .await
        })
    }

    /// recursive FLNK / OUT / CP fan-out entry within a single
    /// processing chain. Does NOT re-acquire the advisory write gate:
    /// the chain is one transaction whose entry record's gate is
    /// already held by the foreign entry, and C `processTarget`
    /// (`dbDbLink.c:436`) processes a link target under the lock set
    /// already owned by the calling thread. Re-acquiring per chain
    /// member would also create a lock-ordering deadlock between
    /// reverse FLNK chains.
    pub(crate) fn process_record_with_links_recursive<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.process_record_with_links_inner(name, visited, depth, false, false, false)
                .await
        })
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
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.process_record_with_links_inner(name, visited, depth, true, true, false)
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
    pub async fn mint_async_token(&self, name: &str) -> Option<AsyncToken> {
        let records = self.inner.records.read().await;
        let rec = records.get(name)?;
        let generation = rec.read().await.reprocess_generation.clone();
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
    pub async fn cancel_async_reentry(&self, name: &str) {
        let records = self.inner.records.read().await;
        if let Some(rec) = records.get(name) {
            rec.read()
                .await
                .reprocess_generation
                .fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Schedule a delayed re-process of `name` — the single owner of the
    /// "mint a fresh [`AsyncToken`], sleep, then fire" pattern. Used by both
    /// [`ProcessAction::ReprocessAfter`] (record-driven owner re-entry: ODLY
    /// output delay, swait, sequence DLYn) and the `SDLY` async-simulation
    /// defer ([`SimOutcome::DeferRead`]). Minting advances the record's
    /// generation so a newer schedule supersedes any pending one; a stale
    /// token's `fire` is a structural no-op. No-op if the record is absent.
    async fn schedule_delayed_reprocess(&self, name: &str, delay: std::time::Duration) {
        let token = match self.mint_async_token(name).await {
            Some(t) => t,
            None => return,
        };
        let db = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = token.fire(&db).await;
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
    pub async fn post_fields(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
    ) -> CaResult<Vec<String>> {
        self.post_fields_with_mask(
            name,
            fields,
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
        )
        .await
    }

    /// Out-of-band PROPERTY-class field post — the C
    /// `db_post_events(precord, &precord->val, DBE_PROPERTY)` analogue used
    /// for enum-string table re-propagation (asyn `callbackEnum`,
    /// devAsynInt32.c:711-762). Writes each `(field, value)` through the
    /// internal put, invalidates the metadata cache, and posts a
    /// `DBE_PROPERTY` event so subscribers re-read enum choices / control
    /// metadata.
    ///
    /// Unlike [`Self::post_fields`] (which posts `DBE_VALUE | DBE_LOG`) this
    /// signals a *property* change, not a value change: a driver that re-keys
    /// its enum strings has not produced a new reading, only new choice
    /// labels. Returns the field names actually posted.
    pub async fn post_property_fields(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
    ) -> CaResult<Vec<String>> {
        self.post_fields_with_mask(name, fields, crate::server::recgbl::EventMask::PROPERTY)
            .await
    }

    /// Shared body of [`Self::post_fields`] / [`Self::post_property_fields`]:
    /// write+notify each field under one record-write lock, posting `mask`.
    async fn post_fields_with_mask(
        &self,
        name: &str,
        fields: Vec<(String, EpicsValue)>,
        mask: crate::server::recgbl::EventMask,
    ) -> CaResult<Vec<String>> {
        let rec = {
            let records = self.inner.records.read().await;
            records.get(name).cloned()
        };
        let rec = rec.ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?;
        let mut inst = rec.write().await;
        let mut posted = Vec::with_capacity(fields.len());
        for (field, value) in fields {
            inst.record.put_field_internal(&field, value)?;
            // Snapshot-cache contract: a metadata-class write must
            // invalidate the cache before the monitor snapshot is built.
            inst.notify_field_written(&field);
            inst.notify_field(&field, mask);
            posted.push(field);
        }
        Ok(posted)
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
    pub(crate) async fn link_target_field_type(
        &self,
        link: &str,
    ) -> Option<crate::types::DbFieldType> {
        let db = match crate::server::record::parse_link_v2(link) {
            crate::server::record::ParsedLink::Db(db) => db,
            _ => return None,
        };
        let rec = self.get_record(&db.record).await?;
        let inst = rec.read().await;
        let field = if db.field.is_empty() {
            "VAL"
        } else {
            db.field.as_str()
        };
        inst.record
            .field_list()
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(field))
            .map(|f| f.dbf_type)
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
    ) -> tokio::task::JoinHandle<()> {
        let db = self.clone();
        tokio::spawn(async move {
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
    /// [`ProcessAction::WriteDbLinkNotify`], which wires the completion
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
        link_str: &str,
        value: EpicsValue,
    ) -> Option<crate::runtime::sync::oneshot::Receiver<()>> {
        let (src_putf, src_alarm) = {
            let rec = {
                let records = self.inner.records.read().await;
                records.get(record_name)?.clone()
            };
            let instance = rec.read().await;
            (
                instance.common.putf,
                super::links::LinkAlarm {
                    stat: instance.common.stat,
                    sevr: instance.common.sevr,
                    amsg: instance.common.amsg.clone(),
                },
            )
        };
        let (waitset, completion) = Self::new_put_notify();
        if !link_str.is_empty() {
            let parsed = crate::server::record::parse_output_link_v2(link_str);
            // Seed the cycle-guard with the source so a target linking back
            // does not re-process it, exactly as a top-level OUT-link write
            // does (`process_record_with_links_inner` inserts its own name).
            let mut visited = HashSet::new();
            visited.insert(record_name.to_string());
            self.write_out_link_value(
                &parsed,
                value,
                super::links::OutLinkSrc {
                    putf: src_putf,
                    notify: Some(&waitset),
                    alarm: &src_alarm,
                },
                &mut visited,
                0,
            )
            .await;
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
    async fn resolve_asub_dynamic_subroutine(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
    ) -> Option<AsubDynamicSub> {
        let (subl, onam) = {
            let inst = rec.read().await;
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
            (read_str("SUBL"), read_str("ONAM"))
        };

        // Read the current name from SUBL. A constant link's text IS the name
        // (C `recGblInitConstantLink` / `dbGetLink` on a constant); a
        // DB/CA/PVA link is read for its string value.
        let name: Option<String> = if subl.is_empty() {
            Some(String::new())
        } else {
            match crate::server::record::parse_link_v2(&subl) {
                crate::server::record::ParsedLink::Constant(s) => Some(s),
                other => self.read_link_with_alarm(&other).await.0.map(|v| match v {
                    EpicsValue::String(s) => s.as_str_lossy().into_owned(),
                    o => o.to_f64().map(|f| f.to_string()).unwrap_or_default(),
                }),
            }
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
            match self.find_subroutine_named(&name).await {
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

    async fn process_record_with_links_inner(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        depth: usize,
        is_continuation: bool,
        acquire_gate: bool,
        // This cycle is driven by a driver interrupt callback
        // (`asyn:READBACK` / SCAN="I/O Intr" output), not a put/FLNK/scan.
        // For an output record it forces the read-back-no-write contract
        // (C `devAsynInt32.c::processBo` `newOutputCallbackValue` branch).
        // Always `false` for client/FLNK/scan entries.
        device_callback: bool,
    ) -> CaResult<()> {
        const MAX_LINK_DEPTH: usize = 16;
        const MAX_LINK_OPS: usize = 256;

        // Normalise to the canonical record name once at entry — both
        // for cycle-detection (`visited` would otherwise treat alias
        // and canonical as distinct entries) and for the records-map
        // lookup below. Mirrors epics-base PR #336.
        let canonical_owned;
        let name: &str = if let Some(target) = self.resolve_alias(name).await {
            canonical_owned = target;
            &canonical_owned
        } else {
            name
        };

        if depth >= MAX_LINK_DEPTH {
            eprintln!("link chain depth limit reached at record {name}");
            return Ok(());
        }
        if visited.len() >= MAX_LINK_OPS {
            eprintln!("link chain ops budget exhausted at record {name}");
            return Ok(());
        }
        if !visited.insert(name.to_string()) {
            return Ok(()); // Cycle detected, skip
        }

        let rec = {
            let records = self.inner.records.read().await;
            records.get(name).cloned()
        };

        let rec = match rec {
            Some(r) => r,
            None => return Err(CaError::ChannelNotFound(name.to_string())),
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
            Some(self.lock_record(name).await)
        } else {
            None
        };

        // 0a. PACT entry guard — mirrors C `dbProcess` (dbAccess.c:537-559).
        // If the record is currently mid-async (PACT=true), do NOT re-enter
        // the body. Instead increment LCNT; after MAX_LOCK=10 consecutive
        // attempts raise SCAN_ALARM/INVALID with "Async in progress" and
        // post a monitor on VAL (DBE_VALUE|DBE_LOG). Up to MAX_LOCK we just
        // bail out silently so transient back-to-back scans don't immediately
        // alarm the record.
        //
        // Without this guard, FLNK / scan-loop / event scans dispatched onto
        // a record whose first cycle is still pending (async device support,
        // CA put_notify on PUTF) would re-enter `record.process()` while the
        // device's first response is still in flight — corrupting the
        // record's internal state machine and bypassing the C-parity
        // contract that callers see for `dbProcess`. The pre-existing
        // `dispatch_cp_targets` path already did this check (sets RPRO=true
        // and skips); the main entry was missing it.
        if !is_continuation {
            const MAX_LOCK: i16 = 10;
            let mut instance = rec.write().await;
            if instance.is_processing() {
                // C `dbAccess.c:539-541` — when TPRO is set on a record
                // whose PACT is true, print the diagnostic line before
                // the bail decision. The C path emits:
                //   "%s: dbProcess of Active '%s' with RPRO=%d"
                // mirroring the same context format the regular trace
                // path below uses (thread/client name + record name +
                // current RPRO bit). Without this, an operator
                // debugging a stuck async record sees NO sign that the
                // entry guard is firing — they only notice the
                // eventual SCAN_ALARM after MAX_LOCK=10 attempts.
                if instance.common.tpro {
                    eprintln!(
                        "[TPRO] {}: dbProcess of Active '{}' with RPRO={}",
                        instance.name,
                        instance.name,
                        if instance.common.rpro { 1 } else { 0 },
                    );
                }
                let stat = instance.common.stat;
                let already_invalid =
                    instance.common.sevr >= crate::server::record::AlarmSeverity::Invalid;
                let already_scan_alarm = stat == crate::server::recgbl::alarm_status::SCAN_ALARM;
                let lcnt_before = instance.common.lcnt;
                instance.common.lcnt = lcnt_before.saturating_add(1);
                if already_scan_alarm || lcnt_before < MAX_LOCK || already_invalid {
                    // Bail out without raising alarm yet.
                    return Ok(());
                }
                // Raise SCAN_ALARM/INVALID, reset alarm transition,
                // and post VAL monitor (DBE_VALUE | DBE_LOG).
                crate::server::recgbl::rec_gbl_set_sevr_msg(
                    &mut instance.common,
                    crate::server::recgbl::alarm_status::SCAN_ALARM,
                    crate::server::record::AlarmSeverity::Invalid,
                    "Async in progress",
                );
                let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);
                // Post VAL with VALUE|LOG|ALARM (C `db_post_events(prec,
                // &VAL, DBE_VALUE|DBE_LOG)` plus recGblResetAlarms'
                // `val_mask = DBE_ALARM` for the fresh transition). The
                // alarm fields carry their C per-field masks
                // (recGbl.c:201-220): this guard only runs on a fresh
                // SCAN_ALARM/INVALID raise, so sevr AND stat both moved —
                // SEVR posts DBE_VALUE, STAT/AMSG post the shared
                // `stat_mask` = DBE_ALARM|DBE_VALUE.
                use crate::server::recgbl::EventMask;
                let stat_mask = EventMask::ALARM | EventMask::VALUE;
                let mut changed_fields = Vec::new();
                if let Some(val) = instance.record.val() {
                    changed_fields.push((
                        "VAL".to_string(),
                        val,
                        EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
                    ));
                }
                changed_fields.push((
                    "SEVR".to_string(),
                    EpicsValue::Short(instance.common.sevr as i16),
                    EventMask::VALUE,
                ));
                changed_fields.push((
                    "STAT".to_string(),
                    EpicsValue::Short(instance.common.stat as i16),
                    stat_mask,
                ));
                // Include AMSG so subscribers reading the alarm text
                // observe "Async in progress" alongside the SCAN_ALARM
                // transition (C `recGbl.c:210-211` posts STAT and AMSG
                // together when `stat_mask` is non-zero).
                changed_fields.push((
                    "AMSG".to_string(),
                    EpicsValue::String(instance.common.amsg.clone().into()),
                    stat_mask,
                ));
                let snapshot = crate::server::record::ProcessSnapshot { changed_fields };
                drop(instance);
                let inst = rec.read().await;
                inst.notify_from_snapshot(&snapshot);
                return Ok(());
            }
            // Not pact: reset lcnt (mirrors C `else { precord->lcnt = 0; }`
            // at dbAccess.c:559) so the next async cycle starts clean.
            instance.common.lcnt = 0;
        }

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
        {
            let (sdis_link, disv, diss) = {
                let instance = rec.read().await;
                (
                    instance.parsed_sdis.clone(),
                    instance.common.disv,
                    instance.common.diss,
                )
            };

            // C `dbGetLink(&precord->sdis, DBR_SHORT, &precord->disa, 0, 0)`
            // reads the SDIS link regardless of its type (DB / CA / PVA /
            // constant) via the lset. The pre-fix port only refreshed
            // `disa` from a `ParsedLink::Db` SDIS, so a remote-sourced
            // (CA/PVA) or constant enable/disable was silently ignored.
            if let Some(val) = self.read_link_value_no_process(&sdis_link).await {
                let disa_val = val.to_f64().unwrap_or(0.0) as i16;
                let mut instance = rec.write().await;
                instance.common.disa = disa_val;
            }

            let disa = rec.read().await.common.disa;
            if disa == disv {
                let notify = {
                    let mut instance = rec.write().await;
                    // C `dbAccess.c:575-577` — clear rpro/putf and arm
                    // notifyCompletion BEFORE the alarm check. Disabled
                    // records skip processing entirely, so any pending
                    // reprocess request is dropped (the next non-
                    // disabled cycle will pick up fresh state) and the
                    // CA put-notify caller must be released. A disabled
                    // record drives no FLNK/OUT chain, so leaving the
                    // wait-set here is its whole contribution.
                    instance.common.rpro = false;
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
                        instance.common.sevr = diss;
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

        // 0.3. TSEL link: C `recGblGetTimeStampSimm` (recGbl.c:310-323).
        //
        // When `TSEL` is a non-constant link, C distinguishes two
        // cases by the link target field:
        //   * the link points at another record's `.TIME` field
        //     (`DBLINK_FLAG_TSELisTIME`) — copy that record's
        //     timestamp directly into `prec->time`;
        //   * otherwise `dbGetLink(&tsel, DBR_SHORT, &prec->tse)` —
        //     load `TSE` from the link before the event lookup.
        {
            let tsel_link = {
                let instance = rec.read().await;
                instance.parsed_tsel.clone()
            };
            // A TSEL link pointing at a `.TIME` field copies that record's
            // timestamp+utag into `time`/`utag` and marks TSE=-2 so
            // `apply_timestamp` leaves them alone. C `TSEL_modified`
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
            let tsel_is_time = match &tsel_link {
                crate::server::record::ParsedLink::Db(link) => {
                    link.field.eq_ignore_ascii_case("TIME")
                }
                crate::server::record::ParsedLink::Ca(ca) => ca_tsel_time_record(&ca.pv).is_some(),
                _ => false,
            };
            if tsel_is_time {
                // C `dbGetTimeStampTag(plink, &prec->time, &prec->utag)`
                // (recGbl.c:317) copies BOTH the link's time AND utag.
                // Read the pair as one consistent snapshot per source.
                let src_time = match &tsel_link {
                    crate::server::record::ParsedLink::Db(link) => {
                        // C `dbInitLink` locality (`dbLink.c:115-130`):
                        // `TSEL_modified` sets the `TSELisTIME` flag and
                        // strips `.TIME` BEFORE the DB-vs-CA decision
                        // (dbLink.c:115-118), so a TSEL `.TIME` link whose
                        // record is not local still becomes a CA link and
                        // reads its remote `.TIME` via the CA lset
                        // `getTimeStampTag`. Local arm reads the source
                        // record's `(time, utag)`; the non-local arm routes
                        // `ca://REC` through `external_link_time` (CA
                        // carries no userTag, so utag is 0) — uniform with
                        // the `Ca` arm below and the `read_db_link_value`
                        // read-locality fallback.
                        if self.has_name_no_resolve(&link.record).await {
                            match self.get_record(&link.record).await {
                                Some(src) => {
                                    let g = src.read().await;
                                    Some((g.common.time, g.common.utag))
                                }
                                None => None,
                            }
                        } else {
                            self.external_link_time(&format!("ca://{}", link.record))
                                .await
                                .map(ext_time_pair)
                        }
                    }
                    crate::server::record::ParsedLink::Ca(ca) => {
                        // Strip `.TIME` (C dbLink.c:82-84) and read the CA
                        // link's cached timestamp. `external_link_time`
                        // routes `ca://` to the ungated CA lset
                        // `time_stamp` (CA has no `time=` option; gated
                        // only on `connected`, like C `dbGetTimeStamp`
                        // failing on a disconnected link). CA wire carries
                        // no userTag, so the source contributes utag 0.
                        match ca_tsel_time_record(&ca.pv) {
                            Some(rec_name) => self
                                .external_link_time(&format!("ca://{rec_name}"))
                                .await
                                .map(ext_time_pair),
                            None => None,
                        }
                    }
                    _ => None,
                };
                // C returns after the TSELisTIME branch even when the read
                // fails (recGbl.c:317-320): keep the record's current time
                // rather than falling through to load TSE from the value.
                if let Some((src_time, src_utag)) = src_time {
                    let mut instance = rec.write().await;
                    instance.common.time = src_time;
                    instance.common.utag = src_utag;
                    instance.common.tse = -2;
                }
            } else if let Some(val) = self.read_link_value_no_process(&tsel_link).await {
                // Non-`.TIME` TSEL: C `dbGetLink(&tsel, DBR_SHORT,
                // &prec->tse)` loads TSE from the link regardless of its
                // type. The pre-fix port only read a `ParsedLink::Db`
                // TSEL, ignoring a CA/PVA/constant TSE source.
                let tse_val = val.to_f64().unwrap_or(0.0) as i16;
                let mut instance = rec.write().await;
                instance.common.tse = tse_val;
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
        // C `switch (prec->simm)` `default:` on an OUTPUT record: the body runs
        // (it is `writeValue`, at the END of `process()`, that refuses), but the
        // device / OUT-link / SIOL write is suppressed. Set only by
        // `SimOutcome::IllegalMode { is_output: true }`.
        let mut sim_illegal_out = false;
        let sim_output = match self.check_simulation_mode(&rec).await {
            SimOutcome::NotSimulated => None,
            SimOutcome::Simulated => {
                self.run_forward_link_tail(name, &rec, visited, depth).await;
                return Ok(());
            }
            SimOutcome::IllegalMode { is_output } => {
                if is_output {
                    // `writeValue` follows the body, so only the write is lost.
                    sim_illegal_out = true;
                    None
                } else {
                    // `readValue` precedes the body and IS the body's input, so
                    // nothing of the body is left to run. SOFT_ALARM/INVALID is
                    // already pending; commit it, post the monitors and fire the
                    // forward link — C `process()` runs `checkAlarms`,
                    // `monitor()` and `recGblFwdLink()` regardless of the -1.
                    {
                        let mut instance = rec.write().await;
                        sim_process_tail(&mut instance, SimTailAlarm::None, false);
                    }
                    self.run_forward_link_tail(name, &rec, visited, depth).await;
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
                    let instance = rec.write().await;
                    instance
                        .processing
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                self.schedule_delayed_reprocess(name, delay).await;
                return Ok(());
            }
            SimOutcome::RedirectOutputToSiol {
                siol,
                sims,
                raw_mode,
            } => Some((siol, sims, raw_mode)),
        };
        {
            let mut instance = rec.write().await;
            if instance.record.simulation_substitutes_input_stage() {
                instance.record.set_simulation_active(sim_input_stage);
            }
        }

        // 1. Read INP link value and DOL link (outside lock)
        let (inp_parsed, is_soft, dol_info) = {
            let instance = rec.read().await;
            let rtype = instance.record.record_type();

            let inp = instance.parsed_inp.clone();
            let is_soft = crate::server::device_support::is_soft_dtyp(&instance.common.dtyp);

            // DOL link info for output records with OMSL=CLOSED_LOOP.
            //
            // C parity: every record type whose DBD declares both an
            // OMSL `menuOmsl` field AND a DOL link field must honour
            // the closed-loop binding. `dfanoutRecord.c:115-122` shows
            // dfanout doing this directly via `dbGetLink(&prec->dol,
            // DBR_DOUBLE, &prec->val, ...)` when `omsl ==
            // menuOmslclosed_loop`. The Rust port previously omitted
            // `dfanout`, so a dfanout configured with OMSL=closed_loop
            // never sourced VAL from DOL — every cycle silently used
            // the previously-cached VAL, breaking any cascaded
            // setpoint-distribution chain that relied on dfanout to
            // re-read the input.
            //
            // The `aao` (array analog output) record is the only other
            // OMSL-bearing C record, and it IS implemented (a `WaveformRecord`
            // alias, `waveform.rs` `pub type AaoRecord`). Its
            // `OMSL=closed_loop` pull is an ARRAY copy — C
            // `aaoRecord.c::fetchValue` reads `DOL` into the value array — not
            // the scalar `dbGetLink(&prec->dol, DBR_DOUBLE, &prec->val)` this
            // arm models, so aao sources DOL record-locally via
            // `WaveformRecord::pre_input_link_actions` and is deliberately
            // absent from this scalar match. Not a missing record.
            let dol = match rtype {
                "ao" | "longout" | "int64out" | "bo" | "mbbo" | "mbboDirect" | "stringout"
                | "lso" | "dfanout" => {
                    let omsl = instance
                        .record
                        .get_field("OMSL")
                        .and_then(|v| {
                            if let EpicsValue::Short(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let oif = instance
                        .record
                        .get_field("OIF")
                        .and_then(|v| {
                            if let EpicsValue::Short(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
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
                            .map(|s| {
                                crate::server::record::parse_link_v2(s.as_str_lossy().as_ref())
                            })
                            .unwrap_or(crate::server::record::ParsedLink::None);
                        // C `!dbLinkIsConstant(&prec->dol)` gates the per-cycle
                        // DOL fetch in every OMSL record (e.g.
                        // `aoRecord.c:442`, `boRecord.c:227`,
                        // `dfanoutRecord.c:115`): a *constant* DOL is applied to
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
                }
                _ => None,
            };

            (inp, is_soft, dol)
        };

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
        {
            let pre_input_actions = {
                let mut instance = rec.write().await;
                let ctx = instance.common.process_context();
                instance.record.set_process_context(&ctx);
                instance.record.pre_input_link_actions()
            };
            if !pre_input_actions.is_empty() {
                self.execute_process_actions(name, &rec, pre_input_actions, visited, depth)
                    .await;
            }
        }

        // Read INP value
        let inp_value = self
            .read_link_value_soft(&inp_parsed, is_soft, visited, depth)
            .await;

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
        // reach `LINK_ALARM`, matching pvxs `pvalink_lset.cpp`
        // `recGblSetSevrMsg`.
        let inp_link_alarm: Option<(
            crate::server::record::MonitorSwitch,
            super::links::LinkAlarm,
        )> = if is_soft {
            match inp_parsed {
                crate::server::record::ParsedLink::Db(ref db) => {
                    let (_v, alarm) = self.read_link_with_alarm(&inp_parsed).await;
                    alarm.map(|a| (db.monitor_switch, a))
                }
                crate::server::record::ParsedLink::Pva(_)
                | crate::server::record::ParsedLink::PvaJson(_) => {
                    // PVA: the lset already applied the MS/NMS/MSI gate,
                    // so the returned severity is final — fold it as
                    // MaximizeStatus to preserve the remote stat+msg
                    // (pvxs `pvalink_lset.cpp`).
                    let (_v, alarm) = self.read_link_with_alarm(&inp_parsed).await;
                    alarm.map(|a| (crate::server::record::MonitorSwitch::MaximizeStatus, a))
                }
                crate::server::record::ParsedLink::Ca(ref ca) => {
                    // CA: apply the link's own
                    // MS/NMS/MSI/MSS gate at the fold boundary, uniform
                    // with the Db arm above — the resolver returned the
                    // *raw* remote alarm, not a gated one.
                    let (_v, alarm) = self.read_link_with_alarm(&inp_parsed).await;
                    alarm.map(|a| (ca.monitor_switch, a))
                }
                _ => None,
            }
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
        // `pvalink_lset.cpp:427`.
        let inp_link_remote_time: Option<(i64, i32, u64)> = match inp_parsed.external_pv_name() {
            Some(name) => self.external_link_time(&name).await,
            None => None,
        };

        // Read DOL value
        let dol_value = if let Some((ref dol_parsed, _oif)) = dol_info {
            self.read_link_value(dol_parsed, visited, depth).await
        } else {
            None
        };

        // 1.45. Sel NVL link: resolve NVL -> SELN BEFORE the input fetch.
        // C `selRecord.c::fetch_values` reads NVL into SELN first, then in
        // `Specified` mode fetches ONLY INP[SELN] (lines 421-431) — the
        // non-selected inputs are never read. Resolving the selector here
        // (rather than after the fetch) lets `select_input_links` restrict
        // the fetch list, so non-selected links raise no monitors and no
        // spurious link-alarm SEVR.
        // Captured for the Specified-mode fetch gate: SELM==0 and
        // whether an NVL link is configured. C `selRecord.c::process`
        // (114) skips `do_sel` when `fetch_values` fails, and in
        // Specified mode a failed NVL read is one such failure.
        let mut sel_is_specified = false;
        let mut sel_nvl_present = false;
        let sel_nvl_value: Option<EpicsValue> = {
            let instance = rec.read().await;
            if instance.record.record_type() == "sel" {
                sel_is_specified =
                    matches!(instance.record.get_field("SELM"), Some(EpicsValue::Enum(0)));
                let nvl_str = instance
                    .record
                    .get_field("NVL")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                sel_nvl_present = !nvl_str.is_empty();
                if sel_nvl_present {
                    drop(instance); // release read lock before async read
                    let parsed =
                        crate::server::record::parse_link_v2(nvl_str.as_str_lossy().as_ref());
                    self.read_link_value(&parsed, visited, depth).await
                } else {
                    None
                }
            } else {
                None
            }
        };
        // Selector index for `select_input_links`: the freshly-resolved NVL
        // value when present, else `None` (the hook falls back to the
        // record's current SELN).
        let sel_selector: Option<u16> = sel_nvl_value
            .as_ref()
            .and_then(|v| v.to_f64())
            .map(|f| f as u16);

        // 1.5. Multi-input link fetch (calc/calcout/sel/sub)
        // Also collect alarm info from source records for MS/NMS propagation.
        let multi_input_values: Vec<(String, EpicsValue)>;
        let mut link_alarms: Vec<(
            crate::server::record::MonitorSwitch,
            super::links::LinkAlarm,
        )> = Vec::new();
        // Link fields (the `multi_input_links` first element) whose
        // fetch actually produced a value this cycle — pushed to the
        // record via `set_resolved_input_links` so its `process()` can
        // observe link-fetch success (C `RTN_SUCCESS(dbGetLink(...))`).
        let mut resolved_link_fields: Vec<&'static str> = Vec::new();
        // sel `Specified`-mode fetch gate. C `selRecord.c::process`
        // (114) runs `do_sel` only when `fetch_values` succeeds. In
        // Specified mode the fetch list is exactly INP[SELN] (via
        // `select_input_links`), so the gate fails when the NVL link or the
        // selected input was configured but did not resolve this cycle.
        let sel_fetch_failed: bool;
        // This cycle's `fetch_values()` outcome — non-zero status in C, i.e.
        // "the record body must not run". Derived from the record's declared
        // `InputFetchPolicy` (see the loop below) and folded with the sel gate
        // into ONE boolean, which is then delivered to its single consumer:
        // `Record::set_fetch_gate_failed` for records that compute in their own
        // `process()` (calc/calcout/scalcout/acalcout/swait/sel), and
        // `RecordInstance::suppress_subroutine_run` for the two whose body is
        // the framework-dispatched subroutine (sub/aSub).
        let mut fetch_values_failed = false;
        {
            let input_fetch_policy;
            let link_info: Vec<(String, &'static str, String)> = {
                let instance = rec.read().await;
                input_fetch_policy = instance.record.input_fetch_policy();
                // Restrict to the record's active inputs this cycle (sel
                // `Specified` → only INP[SELN]); `None` = fetch every link.
                let links = instance
                    .record
                    .select_input_links(sel_selector)
                    .unwrap_or_else(|| instance.record.multi_input_links().to_vec());
                links
                    .iter()
                    .map(|(lf, vf)| {
                        let link_str = instance
                            .record
                            .get_field(lf)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        (link_str.as_str_lossy().into_owned(), *lf, vf.to_string())
                    })
                    .collect()
            }; // read lock dropped
            let mut results = Vec::new();
            for (link_str, link_field, val_field) in &link_info {
                if !link_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(link_str);
                    // C `dbGetLink`: a `ProcessPassive` DB input link
                    // processes its passive source record before the
                    // value is read. `read_link_with_alarm` does a bare
                    // `get_pv`, so process the source here first —
                    // matching the single-INP `read_link_value_soft`
                    // path. Without this, calc/sel/sub/aSub INPA..INPL
                    // PP links read a stale source value.
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        self.process_passive_db_source(db, visited, depth).await;
                    }
                    let (value, alarm) = self.read_link_with_alarm(&parsed).await;
                    let read_failed = value.is_none();
                    if let Some(value) = value {
                        results.push((val_field.clone(), value));
                        resolved_link_fields.push(link_field);
                    }
                    // B2 / multi-input alarm propagation
                    // covers external links too. `Db` and `Ca` carry an
                    // explicit `MonitorSwitch` (CA's was parsed from its
                    // `MS`/`NMS`/`MSI`/`MSS` modifier); `Pva` is gated by
                    // its lset, so its already-final severity folds as
                    // `MaximizeStatus` (preserving remote stat+msg).
                    if let Some(alarm) = alarm {
                        match &parsed {
                            crate::server::record::ParsedLink::Db(db) => {
                                link_alarms.push((db.monitor_switch, alarm));
                            }
                            crate::server::record::ParsedLink::Ca(ca) => {
                                link_alarms.push((ca.monitor_switch, alarm));
                            }
                            crate::server::record::ParsedLink::Pva(_)
                            | crate::server::record::ParsedLink::PvaJson(_) => {
                                link_alarms.push((
                                    crate::server::record::MonitorSwitch::MaximizeStatus,
                                    alarm,
                                ));
                            }
                            _ => {}
                        }
                    }
                    // The record's declared fetch shape decides what a failed
                    // read means. The failed link's own alarm is already folded
                    // above in every shape: C's `dbGetLink` raises the MS
                    // severity for the link it failed on before returning.
                    if read_failed {
                        match input_fetch_policy {
                            // C `transformRecord.c::process` (531-545): read on,
                            // and compute anyway.
                            InputFetchPolicy::ReadAll => {}
                            // C `calcRecord.c::fetch_values` (427-443):
                            // `if (status == 0) status = newStatus;` — the loop
                            // runs to the end, so the inputs behind the failure
                            // still refresh (and post), but the first failing
                            // status is what `process` (:120) gates the calc on.
                            InputFetchPolicy::ReadAllGateOnFailure => {
                                fetch_values_failed = true;
                            }
                            // C `subRecord.c::fetch_values` (407-418):
                            // `if (dbGetLink(plink, ...)) return -1;` — the loop
                            // stops dead at the first failing link. Every input
                            // behind it is never read, so its value field keeps
                            // the previous cycle's value (no monitor, no PP of
                            // that source, no link-alarm inheritance), and the
                            // record body is skipped below.
                            InputFetchPolicy::AbortOnFirstFailure => {
                                fetch_values_failed = true;
                                break;
                            }
                        }
                    }
                }
            }
            multi_input_values = results;

            // Evaluate the Specified-mode fetch gate while
            // `link_info` is in scope. A *configured* (non-empty) selected
            // input that did not reach `resolved_link_fields`, or a
            // configured NVL link that did not resolve, means C
            // `fetch_values` returned failure. An empty selected link is
            // NOT a failure — C `dbGetLink` on an unset constant link
            // returns success and the NaN-initialised field flows into
            // `do_sel`. High/Low/Median (`!sel_is_specified`) never gate.
            sel_fetch_failed = sel_is_specified
                && ((sel_nvl_present && sel_nvl_value.is_none())
                    || (link_info.iter().any(|(s, _, _)| !s.is_empty())
                        && resolved_link_fields.is_empty()));
        }
        // 1.6. String-input link fetch — C `sCalcoutRecord.c::fetch_values`'s
        // SECOND loop (890-941), over INAA..INLL → AA..LL. It is a separate
        // loop here for the same reason it is one in C: it does not feed the
        // fetch gate (`return(0)` at :941, so a failing string link never
        // suppresses sCalcPerform), a failed read writes a diagnostic INTO the
        // value field instead of leaving it alone, and a multi-element
        // DBF_CHAR/DBF_UCHAR source is read as escaped text. See
        // `Record::string_input_links`.
        let string_input_values: Vec<(String, EpicsValue)>;
        {
            let link_info: Vec<(String, &'static str)> = {
                let instance = rec.read().await;
                instance
                    .record
                    .string_input_links()
                    .iter()
                    .map(|(lf, vf)| {
                        let link_str = instance
                            .record
                            .get_field(lf)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        (link_str.as_str_lossy().into_owned(), *vf)
                    })
                    .collect()
            }; // read lock dropped
            let mut results = Vec::with_capacity(link_info.len());
            for (link_str, val_field) in &link_info {
                // C (:895-911): an unset link is neither CA_LINK nor DB_LINK, so
                // neither `dbGetLink` branch runs, `status` stays 0, and the
                // string field keeps whatever was last put to it.
                if link_str.is_empty() {
                    continue;
                }
                let parsed = crate::server::record::parse_link_v2(link_str);
                if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                    self.process_passive_db_source(db, visited, depth).await;
                }
                let (value, alarm) = self.read_link_with_alarm(&parsed).await;
                if let Some(alarm) = alarm {
                    match &parsed {
                        crate::server::record::ParsedLink::Db(db) => {
                            link_alarms.push((db.monitor_switch, alarm));
                        }
                        crate::server::record::ParsedLink::Ca(ca) => {
                            link_alarms.push((ca.monitor_switch, alarm));
                        }
                        crate::server::record::ParsedLink::Pva(_)
                        | crate::server::record::ParsedLink::PvaJson(_) => {
                            link_alarms.push((
                                crate::server::record::MonitorSwitch::MaximizeStatus,
                                alarm,
                            ));
                        }
                        _ => {}
                    }
                }
                let text = match value {
                    Some(value) => string_link_text(&value),
                    // C (:939-940): `epicsSnprintf(*psvalue, STRING_SIZE-1,
                    // "%s:fetch(%s) failed", pcalc->name, sFldnames[i])` — the
                    // failed fetch REPLACES the value with the diagnostic; the
                    // previous string is not kept, and the record still computes.
                    None => truncate_string_field(PvString::from(format!(
                        "{name}:fetch({val_field}) failed"
                    ))),
                };
                results.push((val_field.to_string(), EpicsValue::String(text)));
            }
            string_input_values = results;
        }

        // PR #d0cf47c continued: feed the INP alarm (if any) into the
        // same `link_alarms` list the lock-section iterates over. Order
        // doesn't matter — `rec_gbl_set_sevr_msg` takes the maximum
        // severity across all sources.
        if let Some(pair) = inp_link_alarm {
            link_alarms.push(pair);
        }

        // aSub LFLG=READ: re-read the subroutine name from the SUBL link and,
        // if it changed, re-resolve the function — computed here, before the
        // process write lock, so the SUBL link read cannot deadlock against
        // this record (C `aSubRecord.c::fetch_values`). `None` for everything
        // that is not an aSub in READ mode.
        let asub_dynamic = self.resolve_asub_dynamic_subroutine(&rec).await;

        // 2. Lock record, apply INP/DOL, process, evaluate alarms, build snapshot
        let (
            snapshot,
            out_info,
            flnk_name,
            process_actions,
            alarm_posts,
            result_is_defer_output,
            sim_skip_out,
        ) = 'epilogue: {
            let mut instance = rec.write().await;

            // Apply DOL value for output records (OMSL=CLOSED_LOOP)
            if let Some(dol_val) = dol_value {
                let oif = dol_info.as_ref().map(|(_, oif)| *oif).unwrap_or(0);
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
            }

            // Apply INP value. "Soft Channel" sets VAL directly
            // (C `read_xxx` return 2, skip RVAL→VAL conversion).
            // "Raw Soft Channel" routes the value into RVAL and lets
            // the record's RVAL→VAL convert run (epics-base
            // f2fe9d12: devBiSoftRaw applies MASK after the read).
            // Records opt into the raw path via
            // `Record::accepts_raw_soft_input` so DTYPs on records
            // that haven't wired raw soft channel stay on the legacy
            // VAL-direct path.
            let is_raw_soft = instance.common.dtyp == "Raw Soft Channel"
                && instance.record.accepts_raw_soft_input();
            let soft_inp_applied = inp_value.is_some() && !is_raw_soft;
            if let Some(inp_val) = inp_value {
                if is_raw_soft {
                    let _ = instance.record.apply_raw_input(inp_val);
                } else {
                    let _ = instance.record.set_val(inp_val);
                }
            } else if is_soft
                && matches!(
                    inp_parsed,
                    crate::server::record::ParsedLink::Db(_)
                        | crate::server::record::ParsedLink::Ca(_)
                        | crate::server::record::ParsedLink::Pva(_)
                        | crate::server::record::ParsedLink::PvaJson(_)
                )
            {
                // epics-base PR #4737901: soft-channel `read_xxx` must
                // surface link-read failures via the alarm tree, not
                // silently succeed. When the INP link is a real
                // Db/Ca/Pva link (i.e. operator expected a value) and
                // the read returned None, attach LINK_ALARM/INVALID
                // so downstream consumers can react. ParsedLink::None
                // and Constant don't fall into this branch — the
                // former is "no link configured", the latter has its
                // own None-as-no-value semantics.
                use crate::server::recgbl::{alarm_status, rec_gbl_set_sevr};
                rec_gbl_set_sevr(
                    &mut instance.common,
                    alarm_status::LINK_ALARM,
                    crate::server::record::AlarmSeverity::Invalid,
                );
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
            // zero-filled (aCalcoutRecord.c:1096-1099 for INAA..INLL -> AA..LL);
            // a scalar field is a one-element destination, so it takes element 0
            // (`dbGetLink(..., DBR_DOUBLE, pvalue, 0, 0)`, calcRecord.c:434).
            // `to_f64()` answers None for every array variant, so routing every
            // value through it dropped array-valued links outright — AA..LL never
            // populated and the record calculated on an empty array.
            for (val_field, value) in &multi_input_values {
                if value.is_array() {
                    if instance
                        .record
                        .put_field_internal(val_field, value.clone())
                        .is_ok()
                    {
                        continue;
                    }
                    // The target is a scalar field: element 0, as C's
                    // one-element destination takes.
                    if let Some(f) = value.first_element().and_then(|v| v.to_f64()) {
                        let _ = instance
                            .record
                            .put_field_internal(val_field, EpicsValue::Double(f));
                    }
                } else if let Some(f) = value.to_f64() {
                    let _ = instance
                        .record
                        .put_field_internal(val_field, EpicsValue::Double(f));
                }
            }

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
                if let Some(f) = scalar.and_then(|v| v.to_f64()) {
                    let _ = instance
                        .record
                        .put_field("SELN", EpicsValue::UShort(f as u16));
                }
            }

            // Apply the string-input values (scalcout INAA..INLL -> AA..LL),
            // fetched in step 1.6 above. `put_field_internal` is the coercion
            // owner: it converts to the target field's declared `DbFieldType`,
            // which is `String` for every one of these.
            for (val_field, value) in string_input_values {
                let _ = instance.record.put_field_internal(&val_field, value);
            }

            // Device support read (input records only, not output records)
            let is_soft = instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
            let is_output = instance.record.can_device_write();
            let mut device_actions: Vec<crate::server::record::ProcessAction> = Vec::new();
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
            // and wrongly clear UDF. `is_raw_soft`
            // (Raw Soft Channel, `devAiSoftRaw` returns 0) is excluded —
            // it deliberately wants the RVAL→VAL convert.
            //
            // Gated on `soft_channel_skips_convert()` so this only
            // suppresses an `RVAL → VAL` convert step. Records such as
            // `epid` also override `set_device_did_compute` but treat it
            // as "skip the whole built-in compute" (the PID loop); they
            // return `false` here so a Soft-Channel `epid` still runs
            // `do_pid()` in `process()`.
            let soft_input_skips_convert = is_soft
                && !is_output
                && !is_raw_soft
                && instance.record.soft_channel_skips_convert();
            let mut device_did_compute = (soft_inp_applied && is_soft) || soft_input_skips_convert;
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
                            device_did_compute = read_outcome.did_compute;
                            device_actions = read_outcome.actions;
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
            // Also collect ReadDbLink from device actions
            let mut deferred_device_actions = Vec::new();
            for action in device_actions {
                if matches!(
                    action,
                    crate::server::record::ProcessAction::ReadDbLink { .. }
                ) {
                    pre_actions.push(action);
                } else {
                    deferred_device_actions.push(action);
                }
            }
            if !pre_actions.is_empty() {
                let rec_name = instance.name.clone();
                drop(instance);
                let pre_resolved = self
                    .execute_read_db_links(&rec_name, &rec, &pre_actions, visited, depth)
                    .await;
                instance = rec.write().await;
                resolved_link_fields.extend(pre_resolved);
            }

            // Tell the record which input link fields actually resolved
            // a value this cycle — the union of the multi-input fetch and
            // the pre-process ReadDbLink reads; the framework analogue of
            // C device support inspecting `RTN_SUCCESS(dbGetLink(...))`
            // (`epidRecord.c:191-193`, `motorRecord.cc:3687-3698`).
            instance
                .record
                .set_resolved_input_links(&resolved_link_fields);

            // The cycle's single `fetch_values()` outcome: a link read that
            // failed under a gating `InputFetchPolicy`, or sel's Specified-mode
            // selected-input read that did not resolve (C `selRecord.c::process`
            // (114) skips `do_sel` on it). Every C record that gates its body on
            // `if (fetch_values(prec) == 0)` reads it from here — one boolean,
            // one hook — and a record with no gate ignores it (default no-op).
            let fetch_gate_failed = fetch_values_failed || sel_fetch_failed;
            instance.record.set_fetch_gate_failed(fetch_gate_failed);

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
            }

            // TPRO: trace processing (C EPICS dbProcess prints context when TPRO>0)
            if instance.common.tpro {
                eprintln!(
                    "[TPRO] {}: process (SCAN={:?}, PACT={})",
                    instance.name,
                    instance.common.scan,
                    instance
                        .processing
                        .load(std::sync::atomic::Ordering::Relaxed)
                );
            }

            // MS-class alarm propagation from input links. Mirrors C
            // `recGblInheritSevrMsg` (recGbl.c::260):
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
            for (ms, alarm) in &link_alarms {
                super::links::inherit_sevr_msg(&mut instance.common, *ms, alarm);
            }

            // Push framework-owned common state (UDF/UDFS/NSEV/PHAS/TSE/TSEL) so
            // the record's process() can see it — C records read
            // `dbCommon` directly (`epidRecord.c:195` checks
            // `pepid->udf`, `timestampRecord.c:90` checks `tse`,
            // `transformRecord.c:554` checks `ptran->nsev`).
            {
                let ctx = instance.common.process_context();
                instance.record.set_process_context(&ctx);
            }

            // Apply the aSub LFLG=READ resolution computed above (outside the
            // lock). The single apply owner; the bad-sub skip is carried on the
            // instance and consumed by `run_registered_subroutine`.
            if let Some(ds) = &asub_dynamic {
                apply_asub_dynamic_sub(&mut instance, ds);
            }

            // C `subRecord.c:145-146` / `aSubRecord.c:216-218`:
            //     status = fetch_values(prec);
            //     if (status == 0) status = do_sub(prec);
            // A failed input link means the subroutine does not run this cycle
            // — VAL (and aSub's VALA..VALU) freeze, and none of `do_sub`'s
            // alarms (BAD_SUB / SOFT at BRSV) or its `udf = isnan(val)` update
            // happen. Same one-shot flag the aSub bad-SNAM skip arms, consumed
            // by the single owner `run_registered_subroutine`; OR-ed in so
            // whichever reason fired first still suppresses the run. Same
            // `fetch_values()` outcome the `set_fetch_gate_failed` hook above
            // carries — sub/aSub differ only in WHERE their body runs.
            if fetch_gate_failed {
                instance.suppress_subroutine_run = true;
            }

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
            outcome.actions.extend(deferred_device_actions);
            let process_result = outcome.result;
            let process_actions = outcome.actions;
            // Captured before the `AsyncPendingNotify` `if let` below moves
            // `process_result`; consulted after the monitor epilogue to defer
            // the OUT/OEVT/FLNK tail (swait ODLY — see `CompleteDeferOutput`).
            let result_is_defer_output =
                process_result == crate::server::record::RecordProcessResult::CompleteDeferOutput;
            // Alarm-epilogue-only cycle (C `transformRecord.c:554-560`): the
            // alarm/timestamp commit below runs, the value side does not. See
            // `RecordProcessResult::CompleteAlarmOnly` and the `'epilogue`
            // break after `apply_timestamp`.
            let result_is_alarm_only =
                process_result == crate::server::record::RecordProcessResult::CompleteAlarmOnly;

            if process_result == crate::server::record::RecordProcessResult::AsyncPending {
                // C `dbProcess` contract: when device support / record body
                // signals "async pending", `pact` MUST be true so subsequent
                // dbProcess attempts on the same record bail at the entry
                // guard. Previous Rust port assumed `process_local` had
                // already set it via the swap-true at function entry, but
                // this main path bypasses `process_local` and calls
                // `record.process()` directly — leaving `processing=false`.
                // Mirrors `aiRecord.c:122` and similar: `prec->pact = TRUE;
                // return 0;` before async work.
                instance
                    .processing
                    .store(true, std::sync::atomic::Ordering::Release);

                // PACT stays set; skip alarm/timestamp/snapshot/OUT/FLNK.
                // But still execute any actions (e.g., ReprocessAfter for delayed re-entry).
                let rec_name = instance.name.clone();
                drop(instance);
                self.execute_process_actions(&rec_name, &rec, process_actions, visited, depth)
                    .await;
                return Ok(());
            }
            if process_result == crate::server::record::RecordProcessResult::CompleteNoEmit {
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
                // poll frame per hop up to MAX_LINK_DEPTH; the write guard
                // `instance` is released on return).
                debug_assert!(
                    process_actions.is_empty(),
                    "CompleteNoEmit must carry no process actions"
                );
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
                if !is_soft {
                    if let Some(mut dev) = instance.device.take() {
                        let _ = dev.write(&mut *instance.record);
                        instance.device = Some(dev);
                    }
                }
                apply_timestamp(&mut instance.common, is_soft);
                // Filter out fields that haven't changed, update MLST/last_posted.
                // Each intermediate post carries DBE_VALUE|DBE_LOG — C motor's
                // mid-move `db_post_events` calls use `DBE_VAL_LOG`
                // (motorRecord.cc:2606 DMOV, and every other do_work post);
                // no alarm transition ran on this pending pass, so no
                // DBE_ALARM bit.
                let mut changed_fields = Vec::new();
                for (name, val) in fields {
                    let changed = match instance.posted_value(&name) {
                        Some(prev) => prev != &val,
                        None => true,
                    };
                    if changed {
                        if name == "VAL" {
                            if let Some(f) = val.to_f64() {
                                instance.put_coerced("MLST", f);
                                instance.common.mlst = Some(f);
                            }
                        }
                        instance.record_value_post(&name, val.clone());
                        changed_fields.push((
                            name,
                            val,
                            crate::server::recgbl::EventMask::VALUE
                                | crate::server::recgbl::EventMask::LOG,
                        ));
                    }
                }
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
                let holds_pact_until_continuation = process_actions
                    .iter()
                    .any(|a| matches!(a, crate::server::record::ProcessAction::ReprocessAfter(_)));
                if holds_pact_until_continuation {
                    instance
                        .processing
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                let snapshot = crate::server::record::ProcessSnapshot { changed_fields };
                let rec_name = instance.name.clone();
                let rec_clone = rec.clone();
                drop(instance);
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
                self.execute_process_actions(&rec_name, &rec, link_writes, visited, depth)
                    .await;
                {
                    let inst = rec_clone.read().await;
                    inst.notify_from_snapshot(&snapshot);
                }
                self.execute_process_actions(&rec_name, &rec, deferred_actions, visited, depth)
                    .await;
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
            if is_continuation {
                instance
                    .processing
                    .store(false, std::sync::atomic::Ordering::Release);
            }

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
            if instance.record.clears_udf() {
                instance.common.udf = instance.record.value_is_undefined();
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

            // pvalink `time=true` adopts the latched upstream timestamp
            // into the owning record. `external_link_time` returned
            // `None` unless the lset signalled the option, so a `Some`
            // here is the operator-requested remote timestamp: the remote
            // NT `timeStamp` while connected, or the disconnect-event time
            // while the subscription is down (pvxs `snap_time = e.time`,
            // adopted on the invalid read — `pvalink_lset.cpp:268-270`).
            // Apply BEFORE `apply_timestamp` so the upstream value
            // survives the soft-channel TSE=0 default (`apply_timestamp`
            // would otherwise stamp wall-clock-now on top).
            if let Some((secs, ns, utag)) = inp_link_remote_time {
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
                // TSE=-2 marks "device-set time" — `apply_timestamp`
                // honours this by leaving `common.time` untouched,
                // mirroring the device-support timestamp branch above.
                instance.common.tse = -2;
            }

            // dfanout drives its OUT links HERE — C `dfanoutRecord.c:127-146`
            // runs `push_values` between `checkAlarms` and (in `monitor`)
            // `recGblResetAlarms`, gating the push on the pending `nsev`. A
            // failed `dbPutLink` raises LINK_ALARM/MAJOR (line 312), and a
            // Specified `seln` out of range raises SOFT_ALARM/INVALID (line
            // 317), both into that pending `nsev` — so the write alarm folds
            // into THIS cycle's committed SEVR and its VAL monitor post. The
            // fanout/seq multi-out dispatch stays in the forward-link tail:
            // they drive no value and raise no write alarm. The OUT writes
            // need this record's lock released (a self/cyclic OUT link would
            // otherwise deadlock on the non-reentrant gate, exactly as the
            // tail dispatch already runs unlocked), so release `instance`,
            // dispatch, then re-acquire before the commit below.
            if instance.record.record_type() == "dfanout" {
                let pending_sevr = instance.common.nsev;
                drop(instance);
                let push_alarm = self
                    .dispatch_multi_output(&rec, Some(pending_sevr), visited, depth)
                    .await;
                instance = rec.write().await;
                if let Some((stat, sevr)) = push_alarm {
                    crate::server::recgbl::rec_gbl_set_sevr(&mut instance.common, stat, sevr);
                }
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
            // AFTER `checkAlarms` (aoRecord.c:196 -> :582 / boRecord.c:219 ->
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

            // Transfer nsta/nsev -> sevr/stat, detect alarm change
            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            // Apply timestamp based on TSE
            apply_timestamp(&mut instance.common, is_soft);
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
            if result_is_alarm_only {
                let alarm_posts = alarm_field_posts(&instance.common, &alarm_result);
                break 'epilogue (
                    crate::server::record::ProcessSnapshot {
                        changed_fields: Vec::new(),
                    },
                    None,
                    None,
                    Vec::new(),
                    alarm_posts,
                    false,
                    true,
                );
            }

            // IVOA check for output records with INVALID alarm. Gate on the
            // real (pre-SIMM) severity `real_sev` snapshotted above — C decides
            // IVOA from `prec->nsev` at the `writeValue` call, before
            // `writeValue` raises SIMM_ALARM, so a `SIMS=INVALID` simulation
            // severity does not trigger the veto (the committed `sevr` may be
            // INVALID from SIMM while the record's own alarm is not).
            let skip_out = if real_sev == crate::server::record::AlarmSeverity::Invalid {
                let ivoa = instance
                    .record
                    .get_field("IVOA")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
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
                        if let Some(ivov) = instance.record.get_field("IVOV") {
                            let _ = instance.record.apply_invalid_output_value(ivov);
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
            let out_time_links = instance.record.output_time_input_links();
            if !skip_out && !out_time_links.is_empty() && instance.record.should_output() {
                let reads: Vec<(String, &'static str)> = out_time_links
                    .iter()
                    .filter_map(|(link_field, value_field)| {
                        let link = match instance.record.get_field(link_field) {
                            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
                            _ => return None,
                        };
                        (!link.is_empty()).then_some((link, *value_field))
                    })
                    .collect();
                if !reads.is_empty() {
                    drop(instance);
                    let mut fetched: Vec<(&'static str, EpicsValue)> = Vec::new();
                    for (link, value_field) in reads {
                        // A bare read, no `process_passive_db_source`: C's DOL
                        // is a `recDynLink` (CA-style) input, which never
                        // process-passives its source.
                        let parsed = crate::server::record::parse_link_v2(&link);
                        if let (Some(value), _) = self.read_link_with_alarm(&parsed).await {
                            fetched.push((value_field, value));
                        }
                    }
                    instance = rec.write().await;
                    for (field, value) in fetched {
                        let _ = instance.record.put_field(field, value);
                    }
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
                    crate::runtime::task::spawn(async move {
                        db.post_event_named(&event_name).await;
                    });
                }
            }

            // OUT stage: soft channel -> link put, non-soft -> device.write()
            // Must run BEFORE check_deadband_ext so MLST is not prematurely
            // updated for async writes that return early.
            let can_dev_write = instance.record.can_device_write();
            let is_soft_out =
                instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
            let record_should_output = instance.record.should_output();
            let out_info = if sim_output.is_some() {
                // Simulated OUTPUT record: C `writeValue` redirects the output
                // to SIOL (`dbPutLink(&prec->siol, ..., &prec->oval)`) INSTEAD
                // of the real device write / soft OUT-link write. The redirect
                // is applied from the OUT epilogue by `write_simulated_output_siol`
                // (it reads the post-body OVAL/RVAL), so the normal device/OUT
                // write is suppressed here.
                None
            } else if sim_illegal_out {
                // C `writeValue` `default:` arm: `recGblSetSevr(SOFT_ALARM,
                // INVALID_ALARM); status = -1;` — it returns BEFORE both the
                // device write and the SIOL redirect, so this cycle performs no
                // output at all.
                None
            } else if skip_out {
                None
            } else if !can_dev_write {
                // Non-output records (calcout, etc.) may still have a
                // soft OUT link (DB or external ca://`/`pva://`).
                // Write OVAL to OUT when the record says should_output().
                if record_should_output && instance.parsed_out.is_writable_out_link() {
                    let out_val = instance.record.output_link_value();
                    out_val.map(|v| (instance.parsed_out.clone(), v))
                } else {
                    None
                }
            } else if is_soft_out {
                if !record_should_output {
                    // epics-base 7.0.8 OOPT: gate the soft OUT-link
                    // write on the record's `should_output()`. For
                    // longout/calcout with OOPT != 0 this lets a
                    // condition-not-met cycle silently skip the link
                    // write without disturbing alarms / monitors.
                    None
                } else if instance.parsed_out.is_writable_out_link() {
                    let out_val = instance.record.output_link_value();
                    out_val.map(|v| (instance.parsed_out.clone(), v))
                } else {
                    None
                }
            } else if device_callback {
                // Driver-callback (`asyn:READBACK`) cycle on a hardware output:
                // the new value was read back into VAL by the read stage above;
                // writing it here would re-assert the setpoint to the driver and
                // re-trigger it (the AD `Acquire` loop). C
                // `devAsynInt32.c::processBo` takes the `newOutputCallbackValue`
                // readback branch and never calls `processCallbackOutput`'s
                // `write()` on a callback cycle.
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
                    match dev.write_begin(&mut *instance.record) {
                        Ok(Some(completion)) => {
                            // Async write submitted -- set PACT, return early.
                            // complete_async_record will handle deadband, snapshot,
                            // notification, and FLNK when the write completes.
                            instance
                                .processing
                                .store(true, std::sync::atomic::Ordering::Release);
                            instance.device = Some(dev);
                            let rec_name = instance.name.clone();
                            let timeout = std::time::Duration::from_secs(5);
                            let db = self.clone();
                            tokio::spawn(async move {
                                let _ =
                                    tokio::task::spawn_blocking(move || completion.wait(timeout))
                                        .await;
                                let _ = db.complete_async_record(&rec_name).await;
                            });
                            return Ok(());
                        }
                        Ok(None) => {
                            // No async support -- fall back to synchronous write
                            if let Err(e) = dev.write(&mut *instance.record) {
                                eprintln!("device write error on {}: {e}", instance.name);
                                instance.common.stat =
                                    crate::server::recgbl::alarm_status::WRITE_ALARM;
                                instance.common.sevr =
                                    crate::server::record::AlarmSeverity::Invalid;
                            } else {
                                // OOPT 7.0.8: notify the record so it can
                                // latch transition state (e.g. longout.pval)
                                // for the next cycle.
                                instance.record.on_output_complete();
                            }
                        }
                        Err(e) => {
                            eprintln!("device write_begin error on {}: {e}", instance.name);
                            instance.common.stat = crate::server::recgbl::alarm_status::WRITE_ALARM;
                            instance.common.sevr = crate::server::record::AlarmSeverity::Invalid;
                        }
                    }
                    instance.device = Some(dev);
                }
                None
            };

            // Compute per-field posting masks (after OUT stage so async
            // writes don't update MLST/ALST prematurely before returning
            // early)
            use crate::server::recgbl::EventMask;

            let (include_val, include_archive) = match instance.record.monitor_value_changed() {
                // lsi/lso post VALUE|LOG only when the string actually
                // changed (C `lsiRecord.c`/`lsoRecord.c` monitor: `len !=
                // olen || memcmp(oval, val, len)`); they have no MDEL/ADEL
                // deadband to express that, so the gate is explicit. The
                // MPST/APST `menuPost` "Always" override OR-adds DBE_VALUE /
                // DBE_LOG even on an unchanged cycle (C monitor: `if (mpst ==
                // menuPost_Always) events |= DBE_VALUE; if (apst ==
                // menuPost_Always) events |= DBE_LOG;`).
                Some(changed) => {
                    let (val_always, archive_always) = instance.record.monitor_always_post();
                    (changed || val_always, changed || archive_always)
                }
                None => {
                    if instance.record.uses_monitor_deadband() {
                        instance.check_deadband_ext()
                    } else {
                        // Binary records (bi/bo/busy/mbbi/mbbo): always post monitors
                        (true, true)
                    }
                }
            };
            // C `recGblResetAlarms` returns `val_mask = DBE_ALARM`
            // (recGbl.c:194/203/212) when the severity/status OR the
            // alarm message moved — every monitored-value post this
            // cycle carries DBE_ALARM so a `DBE_ALARM`-only subscriber
            // sees the value at the moment the alarm changed.
            let alarm_bits = if alarm_result.alarm_changed || alarm_result.amsg_changed {
                EventMask::ALARM
            } else {
                EventMask::NONE
            };

            // Build snapshot
            let mut changed_fields = Vec::new();
            // The deadband-tracked field posts with the classes that
            // actually fired: MDEL crossing → DBE_VALUE, ADEL crossing
            // → DBE_LOG, alarm movement → DBE_ALARM — and nothing else
            // (C `monitor()` per-field masks: motorRecord.cc:3477-3507
            // RBV, aiRecord.c VAL). For most records the tracked field
            // IS the primary value; a record like motor deadbands its
            // readback, and its VAL routes through the generic
            // change-detection loop below — an unchanged setpoint is
            // not re-posted on every readback poll.
            let deadband_field = instance.record.monitor_deadband_field();
            // The mask every change-detected aux field posts with — owned by
            // `AuxPostMask`, the single resolver of the record's declared
            // narrowings of C's default `monitor_mask | DBE_VALUE | DBE_LOG`.
            let aux_post = AuxPostMask::of(instance.record.as_ref());
            // The deadband field's post — mask owned by `deadband_post`, the
            // single assembler for C's `db_post_events(&prec->val, monitor_mask)`.
            let deadband = instance.deadband_post(alarm_bits, include_val, include_archive);
            let deadband_mask = deadband.mask;
            if let Some((field, value)) = deadband.field {
                changed_fields.push((field, value, deadband_mask));
            }
            // The cycle's subscriber posts — assembled by the single owner
            // `RecordInstance::collect_subscriber_posts`, shared by every
            // processing path so no rule can hold on one path and not another.
            changed_fields.extend(instance.collect_subscriber_posts(
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
            if instance.array_hash_changed {
                if let Some(h) = instance.resolve_field("HASH") {
                    changed_fields.push(("HASH".to_string(), h, EventMask::VALUE));
                }
            }
            // The SEVR/STAT/AMSG/ACKS posts `recGblResetAlarms` makes, each
            // with its own C mask — see `alarm_field_posts`. Deferred to
            // dedicated `notify_field` calls fired after the snapshot notify
            // below. The `CompleteAlarmOnly` break above uses the same helper,
            // so the alarm-post masks have a single owner.
            let alarm_posts = alarm_field_posts(&instance.common, &alarm_result);
            // UDF rides along whenever any monitored post fired this
            // cycle, carrying the union of the cycle's posted classes.
            let cycle_mask = changed_fields
                .iter()
                .fold(EventMask::NONE, |m, (_, _, fm)| m | *fm);
            if !cycle_mask.is_empty() {
                changed_fields.push((
                    "UDF".to_string(),
                    EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
                    cycle_mask,
                ));
            }
            let snapshot = crate::server::record::ProcessSnapshot { changed_fields };

            let flnk_name = if instance.record.should_fire_forward_link() {
                if let crate::server::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };

            // Put-notify completion is NOT fired here. Firing before the
            // OUT/FLNK/process-action tail (below) would report the
            // WRITE_NOTIFY done while the chain it triggers — including
            // an async FLNK target — is still running (C `dbNotify.c`
            // keeps the originating record in the waitList until the
            // chain settles). The originating record instead `leave`s
            // the wait-set at the END of this function, after every PP
            // target it drives has joined. See `complete_put_notify`
            // at the tail.

            (
                snapshot,
                out_info,
                flnk_name,
                process_actions,
                alarm_posts,
                result_is_defer_output,
                skip_out,
            )
        };

        // 3. Notify subscribers (outside lock)
        {
            // Write guard: a value-class post advances the record's
            // already-published state (`RecordInstance::record_value_post`),
            // so posting is a `&mut` operation.
            let mut instance = rec.write().await;
            instance.notify_from_snapshot(&snapshot);
            // Post the alarm fields (SEVR/STAT/AMSG/ACKS) with their
            // individual C masks — see recGblResetAlarms above.
            for &(field, mask) in &alarm_posts {
                instance.notify_field(field, mask);
            }
        }

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
        // `CompleteNoEmit` note above; it overflowed the chain-depth guard).
        // Holding `processing=true` also makes the tail's putf-clear (gated on
        // `!is_processing()`) a no-op, leaving putf for the continuation.
        if result_is_defer_output {
            let holds_pact_until_continuation = process_actions
                .iter()
                .any(|a| matches!(a, crate::server::record::ProcessAction::ReprocessAfter(_)));
            if holds_pact_until_continuation {
                let instance = rec.write().await;
                instance
                    .processing
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }

        // Snapshot source PUTF + put-notify wait-set for the C
        // `processTarget` / `dbNotifyAdd` invariants (see
        // `write_db_link_value` doc). Captured once here so every OUT /
        // multi-OUT / FLNK dispatch in this cycle propagates the same
        // bit and joins the same wait-set. The committed alarm is
        // captured the same way for `recGblInheritSevrMsg` MS-class
        // propagation into the OUT-link target.
        let (src_putf, src_notify, src_alarm) = {
            let guard = rec.read().await;
            (
                guard.common.putf,
                guard.notify.clone(),
                super::links::LinkAlarm {
                    stat: guard.common.stat,
                    sevr: guard.common.sevr,
                    amsg: guard.common.amsg.clone(),
                },
            )
        };

        // 4. OUT link — DB *or* external `ca://`/`pva://`. C
        // `dbLink.c::dbPutLink` (dbLink.c:434-448) routes every link
        // write through the link set's `putValue`, so the OUTPUT side
        // dispatches by scheme exactly as the INPUT side does (B
        // `resolve_external_pv`). An external link with no registered
        // lset fails gracefully inside `write_out_link_value`.
        if let Some((ref link, ref out_val)) = out_info {
            self.write_out_link_value(
                link,
                out_val.clone(),
                super::links::OutLinkSrc {
                    putf: src_putf,
                    notify: src_notify.as_ref(),
                    alarm: &src_alarm,
                },
                visited,
                depth,
            )
            .await;
            // OOPT 7.0.8: latch the record's post-output state so the
            // next cycle's `should_output` sees the right pval.
            {
                let mut instance = rec.write().await;
                instance.record.on_output_complete();
            }
        }

        // Simulated OUTPUT record: apply the SIOL redirect. C `writeValue` does
        // `dbPutLink(&prec->siol, ..., &prec->oval)` before `recGblFwdLink`, so
        // it runs here — after the OUT-link write point, before the forward-link
        // tail below. `out_info` is `None` for a simulated record (the device /
        // soft-OUT write was suppressed above), so this is the record's single
        // output this cycle. The read-of-OVAL + write lives in a dedicated
        // helper so the `EpicsValue` it materialises stays out of this giant
        // function's async state (which is polled MAX_LINK_DEPTH-deep on a FLNK
        // chain — see the depth-limit tests).
        self.write_simulated_output_siol(&rec, &sim_output, sim_skip_out)
            .await;

        // 7b. C record support performs a record's OUT/link writes BEFORE
        // its forward link: `transformRecord` calls `dbPutLink()`
        // (transformRecord.c:608-619) before `monitor()` +
        // `recGblFwdLink()`, `scalerRecord` writes COUT/COUTP
        // (scalerRecord.c:457-480) before its FLNK block, `throttleRecord`
        // writes the selected OUT link (throttleRecord.c:562-580) before
        // `recGblFwdLink()`, and `tableRecord` drives speed/drive links
        // (tableRecord.c:573-597) before its final FLNK. The
        // `ProcessAction::WriteDbLink` contract is documented as "before
        // FLNK", so split the requested actions: link writes run now;
        // delayed/reprocess and device-command actions (whose timing must
        // stay after the FLNK tail) run afterward. A downstream FLNK
        // target therefore reads the freshly written value, matching C.
        let (link_writes, deferred_actions): (Vec<_>, Vec<_>) =
            process_actions.into_iter().partition(|a| {
                matches!(
                    a,
                    crate::server::record::ProcessAction::WriteDbLink { .. }
                        | crate::server::record::ProcessAction::WriteDbLinkNotify { .. }
                )
            });
        self.execute_process_actions(name, &rec, link_writes, visited, depth)
            .await;

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
                &rec,
                flnk_name.as_deref(),
                PutNotifyCtx {
                    putf: src_putf,
                    notify: src_notify.as_ref(),
                },
                visited,
                depth,
            )
            .await;
        }

        // 8. Execute the deferred ProcessActions after the FLNK tail:
        // `ReprocessAfter` schedules a later reprocess (the current
        // cycle's FLNK must proceed first) and `DeviceCommand` posts its
        // own monitors after this cycle's snapshot.
        self.execute_process_actions(name, &rec, deferred_actions, visited, depth)
            .await;

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
        {
            let guard = rec.read().await;
            if !guard.is_processing() {
                drop(guard);
                let mut guard = rec.write().await;
                guard.common.putf = false;
            }
        }

        // Put-notify completion: the record `leave`s the wait-set only
        // here, after its full OUT/FLNK/process-action tail has run — so
        // every PP target it drove has already joined (`enter`ed). Gated
        // on `is_put_complete`: a record reporting more work (e.g. motor
        // mid-move via `is_put_complete()==false`) keeps its membership
        // and leaves on the later cycle that completes the put — matching
        // the old fire site's gate. An async-pending record returned
        // earlier and is handled in `complete_async_record_inner`. The
        // completion oneshot fires on the `leave` that empties the set.
        {
            let mut guard = rec.write().await;
            if guard.record.is_put_complete() {
                complete_put_notify(&mut guard);
            }
        }

        Ok(())
    }

    /// Forward-link / CP / RPRO tail for the simulation-mode path.
    ///
    /// C `aiRecord.c:151-168`: a record in SIMM mode handles the value
    /// inside `readValue()`, then `process()` still runs `monitor` +
    /// `recGblFwdLink(prec)`. The simulation path in
    /// `process_record_with_links_inner` does its own monitor posting,
    /// so this drives the forward-link / CP / RPRO tail that
    /// `recGblFwdLink` would. `flnk_name` and `src_putf` are derived
    /// fresh from the record (a simulated cycle does not change FLNK,
    /// and SIOL reads/writes do not carry a foreign PUTF into the
    /// chain).
    async fn run_forward_link_tail(
        &self,
        name: &str,
        rec: &Arc<RwLock<RecordInstance>>,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) {
        let (flnk_name, src_putf, src_notify) = {
            let instance = rec.read().await;
            let flnk = if instance.record.should_fire_forward_link() {
                if let crate::server::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };
            (flnk, instance.common.putf, instance.notify.clone())
        };
        self.run_forward_link_tail_with_putf(
            name,
            rec,
            flnk_name.as_deref(),
            PutNotifyCtx {
                putf: src_putf,
                notify: src_notify.as_ref(),
            },
            visited,
            depth,
        )
        .await;
    }

    /// Steps 4.5 - 7 of the process chain: multi-output dispatch,
    /// event-record posting, generic OUTA..OUTP links, FLNK forward
    /// link, CP-target dispatch, and RPRO reprocess. Shared by the
    /// main process path and the simulation-mode path so both run the
    /// identical `recGblFwdLink` equivalent.
    async fn run_forward_link_tail_with_putf(
        &self,
        name: &str,
        rec: &Arc<RwLock<RecordInstance>>,
        flnk_name: Option<&str>,
        src: PutNotifyCtx<'_>,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) {
        // 4.5. Multi-output dispatch (fanout/seq). dfanout dispatches
        // pre-commit in `process_record_with_links_inner` so its OUT-link
        // write failure folds LINK_ALARM/MAJOR into the same-cycle SEVR
        // (C `dfanoutRecord.c` push_values runs before `recGblResetAlarms`);
        // the `None` phase argument skips dfanout here.
        let _ = self.dispatch_multi_output(rec, None, visited, depth).await;

        // 4.55. event record: post the named software event.
        self.dispatch_event_record(rec).await;

        // 4.6. Generic multi-output links (scalcout / acalcout OUT->OVAL).
        // Only scalcout + acalcout implement `Record::multi_output_links`
        // (the trait default is empty), so they are the only record types
        // that reach this dispatch.
        //
        // SINGLE-OWNER INVARIANT: a record type whose link groups are
        // dispatched by `dispatch_multi_output` (§4.5 above) MUST be
        // skipped here — otherwise its `LNKn`/`OUTn` would be written
        // twice per cycle. `sseq` previously also implemented the
        // `Record::multi_output_links` trait method, so this block
        // re-dispatched every selected `LNKn` after §4.5 already drove
        // it. The `multi_output_dispatch_owned` gate makes the
        // double-dispatch structurally impossible — not just removed
        // at the `SseqRecord` call site.
        {
            let multi_out = {
                let instance = rec.read().await;
                // Framework IVOA=Don't_drive veto for the multi-output OUT
                // path, mirroring the single-OUT `skip_out` gate in the IVOA
                // block: on an INVALID cycle with IVOA=Don't_drive the OUT
                // write is suppressed. The parsed_out single-OUT path is
                // already gated, but `multi_output_links` (scalcout/acalcout
                // OUT) was not — so a non-calc-fail INVALID (INP LINK_ALARM,
                // SIMM, NaN-VAL UDF, …) + Don't_drive wrote OUT where C
                // suppresses (execOutput nsev>=INVALID → Don't_drive break,
                // sCalcoutRecord.c:794). The record-level OOPT/calc-fail
                // decision still gates via `multi_output_links()` itself; this
                // is the framework IVOA layer on top, for every INVALID source.
                let ivoa_dont_drive = instance.common.sevr
                    == crate::server::record::AlarmSeverity::Invalid
                    && matches!(
                        instance.record.get_field("IVOA"),
                        Some(EpicsValue::Short(1))
                    );
                let links = if ivoa_dont_drive
                    || super::links::multi_output_dispatch_owned(instance.record.record_type())
                {
                    &[][..]
                } else {
                    instance.record.multi_output_links()
                };
                if links.is_empty() {
                    None
                } else {
                    let mut pairs = Vec::new();
                    for &(link_field, val_field) in links {
                        let link_str = instance
                            .record
                            .get_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        if link_str.is_empty() {
                            continue;
                        }
                        if let Some(val) = instance.record.get_field(val_field) {
                            pairs.push((link_str, val));
                        }
                    }
                    if pairs.is_empty() { None } else { Some(pairs) }
                }
            };
            if let Some(pairs) = multi_out {
                // Source committed alarm for `recGblInheritSevrMsg`
                // MS-class propagation into each OUT-link target —
                // captured once, same lifecycle as `src.putf`.
                let src_alarm = {
                    let guard = rec.read().await;
                    super::links::LinkAlarm {
                        stat: guard.common.stat,
                        sevr: guard.common.sevr,
                        amsg: guard.common.amsg.clone(),
                    }
                };
                for (link_str, val) in pairs {
                    // `multi_output_links` carries record OUT links
                    // (sseq `LNKn`, scalcout `OUTn` — all `DBF_OUTLINK`)
                    // driven via `dbPutLink` → `dbDbPutValue`
                    // (`dbDbLink.c:388`): a bare DB link is NPP, the
                    // value is written but the target is NOT processed.
                    // `parse_output_link_v2` applies the
                    // OUT-link-correct NPP default; `parse_link_v2` would
                    // wrongly default a bare link to ProcessPassive and
                    // re-process the target. An external `ca://`/`pva://`
                    // OUT link is routed through the link set's
                    // `putValue` (C `dbLink.c::dbPutLink`,
                    // dbLink.c:434-448).
                    let parsed = crate::server::record::parse_output_link_v2(
                        link_str.as_str_lossy().as_ref(),
                    );
                    self.write_out_link_value(
                        &parsed,
                        val,
                        super::links::OutLinkSrc {
                            putf: src.putf,
                            notify: src.notify,
                            alarm: &src_alarm,
                        },
                        visited,
                        depth,
                    )
                    .await;
                }
            }
        }

        // 5. FLNK -- only process if target is Passive (like C dbScanFwdLink).
        // FLNK goes through C `dbScanPassive` -> `processTarget`, which
        // propagates `src.putf` to the target the same way OUT links do.
        if let Some(flnk) = flnk_name {
            if let Some(target_rec) = self.get_record(flnk).await {
                let (target_scan, should_process) = {
                    let mut tg = target_rec.write().await;
                    let pact = tg.is_processing();
                    let on_chain = visited.contains(flnk);
                    let scan = tg.common.scan;
                    if !pact {
                        tg.common.putf = src.putf;
                        // C `dbNotifyAdd` (dbDbLink.c:460) lives inside
                        // `processTarget`, which `dbScanPassive` reaches
                        // ONLY for a passive target (it returns early for
                        // non-passive — dbDbLink.c:431). Gate the join on
                        // the same passive condition as the process call
                        // below: a non-passive FLNK target is dropped here
                        // and must NOT join, or it would `enter` the
                        // wait-set without ever processing to `leave` it,
                        // hanging the completion forever.
                        if scan == crate::server::record::ScanType::Passive {
                            join_put_notify(&mut tg, src.notify);
                        }
                    } else if src.putf && !on_chain {
                        tg.common.rpro = true;
                        tg.common.putf = false;
                    }
                    (scan, !pact)
                };
                if should_process && target_scan == crate::server::record::ScanType::Passive {
                    // recursive FLNK within one chain — gate
                    // already held by the foreign entry record.
                    let _ = self
                        .process_record_with_links_recursive(flnk, visited, depth + 1)
                        .await;
                }
            }
        }

        // 5b. FLNK whose target is external (`pva://`/`ca://`): C
        // `dbScanFwdLink` dispatches it through the link set's
        // `scanForward` (pvalink `pvaScanForward`), a process-only trigger
        // of the remote target. The `flnk_name` above only ever names a
        // local DB target, so a non-DB FLNK is forwarded here through the
        // single owner.
        self.dispatch_external_forward_link(rec).await;

        // 6. CP link targets -- process records that have CP input links from this record
        self.dispatch_cp_targets(name, visited, depth).await;

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
        // scanOnce queue: the reprocess runs with a clean (empty)
        // `visited` set and starts at depth 0, so it cannot be
        // silently skipped by the current chain's cycle guard nor hit
        // the MAX_LINK_DEPTH / MAX_LINK_OPS budget the current chain
        // has already consumed.
        {
            let needs_rpro = {
                let mut instance = rec.write().await;
                if instance.common.rpro {
                    instance.common.rpro = false;
                    true
                } else {
                    false
                }
            };
            if needs_rpro {
                let db = self.clone();
                let rpro_name = name.to_string();
                crate::runtime::task::spawn(async move {
                    let mut fresh_visited = std::collections::HashSet::new();
                    let _ = db
                        .process_record_with_links(&rpro_name, &mut fresh_visited, 0)
                        .await;
                });
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
    /// the owning record (`pvxs/ioc/pvalink_lset.cpp:677-679`). This raises
    /// the same *pending* LINK/INVALID alarm via [`rec_gbl_set_sevr_msg`],
    /// promoted by the next `recGblResetAlarms` — exactly as the C late-set
    /// inside `recGblFwdLink` (after the record's own alarm/monitor stage)
    /// is.
    async fn dispatch_external_forward_link(&self, rec: &Arc<RwLock<RecordInstance>>) {
        let target = {
            let instance = rec.read().await;
            if !instance.record.should_fire_forward_link() {
                return;
            }
            match &instance.parsed_flnk {
                crate::server::record::ParsedLink::Pva(_)
                | crate::server::record::ParsedLink::PvaJson(_)
                | crate::server::record::ParsedLink::Ca(_) => instance
                    .parsed_flnk
                    .external_pv_name()
                    .map(|s| s.to_string()),
                // A DB FLNK is processed by the local §5 scanOnce path;
                // every other kind (Constant/Hw/Calc/None) carries no
                // forward action.
                _ => None,
            }
        };
        let Some(target) = target else {
            return;
        };
        if let Err(e) = self.scan_forward_external_pv(&target).await {
            let _ = e;
            let mut instance = rec.write().await;
            crate::server::recgbl::rec_gbl_set_sevr_msg(
                &mut instance.common,
                crate::server::recgbl::alarm_status::LINK_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
                "Disconn",
            );
        }
    }

    /// Execute ReadDbLink actions before process().
    /// Reads linked PV values and writes them into record fields via put_field_internal.
    /// Returns the `link_field` names whose read produced a value, so the
    /// caller can fold them into the per-cycle `set_resolved_input_links`
    /// report (C `RTN_SUCCESS(dbGetLink(...))`). An empty link is skipped
    /// and NOT reported — it is a CONSTANT link in C, which records must
    /// not treat as a failed fetch.
    async fn execute_read_db_links(
        &self,
        _record_name: &str,
        rec: &Arc<crate::runtime::sync::RwLock<RecordInstance>>,
        actions: &[crate::server::record::ProcessAction],
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Vec<&'static str> {
        use crate::server::record::ProcessAction;
        let mut resolved = Vec::new();
        for action in actions {
            if let ProcessAction::ReadDbLink {
                link_field,
                target_field,
            } = action
            {
                let link_str = {
                    let instance = rec.read().await;
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
                if link_str.is_empty() {
                    continue;
                }
                let parsed = crate::server::record::parse_link_v2(link_str.as_str_lossy().as_ref());
                if let Some(value) = self.read_link_value(&parsed, visited, depth).await {
                    let mut instance = rec.write().await;
                    let _ = instance.record.put_field_internal(target_field, value);
                    resolved.push(*link_field);
                }
            }
        }
        resolved
    }

    /// Execute ProcessActions returned by a record's process() call.
    ///
    /// Actions are executed in order:
    /// - ReadDbLink: reads a linked PV value and writes it into a record field
    ///   (bypasses read-only checks via put_field_internal)
    /// - WriteDbLink: writes a value to a linked PV
    /// - ReprocessAfter: schedules a delayed re-process via tokio::spawn
    pub(super) async fn execute_process_actions(
        &self,
        record_name: &str,
        rec: &Arc<crate::runtime::sync::RwLock<RecordInstance>>,
        actions: Vec<crate::server::record::ProcessAction>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        use crate::server::record::ProcessAction;

        for action in actions {
            match action {
                ProcessAction::ReadDbLink {
                    link_field,
                    target_field,
                } => {
                    // 1. Get the link string from the record
                    let link_str = {
                        let instance = rec.read().await;
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
                    if link_str.is_empty() {
                        continue;
                    }
                    // 2. Parse and read the linked PV
                    let parsed =
                        crate::server::record::parse_link_v2(link_str.as_str_lossy().as_ref());
                    if let Some(value) = self.read_link_value(&parsed, visited, depth).await {
                        // 3. Write into the record field (internal put bypasses read-only)
                        let mut instance = rec.write().await;
                        let _ = instance.record.put_field_internal(target_field, value);
                    }
                }
                ProcessAction::WriteDbLink { link_field, value } => {
                    // 1. Get the link string (record fields → common fields)
                    // and the source PUTF for processTarget propagation,
                    // plus the committed alarm for `recGblInheritSevrMsg`
                    // MS-class propagation into the OUT-link target.
                    let (link_str, src_putf, src_notify, src_alarm) = {
                        let instance = rec.read().await;
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
                            super::links::LinkAlarm {
                                stat: instance.common.stat,
                                sevr: instance.common.sevr,
                                amsg: instance.common.amsg.clone(),
                            },
                        )
                    };
                    if link_str.is_empty() {
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
                    self.write_out_link_value(
                        &parsed,
                        value,
                        super::links::OutLinkSrc {
                            putf: src_putf,
                            notify: src_notify.as_ref(),
                            alarm: &src_alarm,
                        },
                        visited,
                        depth,
                    )
                    .await;
                }
                ProcessAction::DeviceCommand { command, ref args } => {
                    let mut instance = rec.write().await;
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
                ProcessAction::ReprocessAfter(delay) => {
                    // Owner-driven delayed re-entry, mirroring C
                    // `callbackRequestDelayed` dispatching to
                    // `(*prset->process)(prec)` directly (callback.c). The
                    // mint-token + delayed-fire is the single
                    // `schedule_delayed_reprocess` owner, shared with the
                    // SDLY async-simulation defer.
                    self.schedule_delayed_reprocess(record_name, delay).await;
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
                        let instance = rec.read().await;
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
                        tokio::spawn(async move {
                            let mut visited = HashSet::new();
                            let _ = db.process_record_with_links(&name, &mut visited, 0).await;
                        });
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
                        let instance = rec.read().await;
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
                            super::links::LinkAlarm {
                                stat: instance.common.stat,
                                sevr: instance.common.sevr,
                                amsg: instance.common.amsg.clone(),
                            },
                        )
                    };
                    // Mint the re-entry token BEFORE issuing the put so a
                    // synchronous downstream completion cannot fire the
                    // oneshot before the waiter is wired. The mint supersedes
                    // any prior pending re-entry for this record (newer
                    // token), exactly like ReprocessAfter.
                    let token = match self.mint_async_token(record_name).await {
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
                            &parsed,
                            value,
                            super::links::OutLinkSrc {
                                putf: src_putf,
                                notify: Some(&waitset),
                                alarm: &src_alarm,
                            },
                            visited,
                            depth,
                        )
                        .await;
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
                    self.cancel_async_reentry(record_name).await;
                }
            }
        }
    }

    /// Complete an asynchronous record's post-process steps.
    /// Call after device support signals completion (clears PACT, runs alarms, snapshot, OUT, FLNK).
    pub fn complete_async_record<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut visited = HashSet::new();
            self.complete_async_record_inner(name, &mut visited, 0)
                .await
        })
    }

    async fn complete_async_record_inner(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> CaResult<()> {
        // Alias-aware entry — same pattern as
        // `process_record_with_links_inner`. `name` may arrive as an
        // alias from an async device-support callback that captured
        // the original record name; normalise to canonical so the
        // records-map lookup, the `visited` cycle set, and downstream
        // FLNK/OUT dispatches all see the same canonical name.
        let canonical_owned;
        let name: &str = if let Some(target) = self.resolve_alias(name).await {
            canonical_owned = target;
            &canonical_owned
        } else {
            name
        };

        let rec = {
            let records = self.inner.records.read().await;
            records
                .get(name)
                .cloned()
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?
        };

        // Seed the cycle guard with this record's own name — mirrors
        // the synchronous main path (`process_record_with_links_inner`
        // does `visited.insert(name)` before the body). Without this
        // the async-completion FLNK / OUT / CP dispatch can re-enter
        // the just-completed record: an async FLNK chain that loops
        // back (A async -> completes -> FLNK -> B -> FLNK -> A) would
        // re-process A unbounded, because PACT is cleared below before
        // the FLNK dispatch and nothing else blocks the re-entry.
        if !visited.insert(name.to_string()) {
            return Ok(()); // Cycle detected, skip
        }

        let (snapshot, out_info, flnk_name, alarm_posts) = {
            let mut instance = rec.write().await;

            // UDF update before alarm evaluation (C parity — see the
            // sync process path). A NaN/undefined value keeps UDF true
            // so `recGblCheckUDF` raises UDF_ALARM this cycle.
            if instance.record.clears_udf() {
                instance.common.udf = instance.record.value_is_undefined();
            }
            // Per-record alarm hook (C `checkAlarms()`).
            {
                let inst = &mut *instance;
                inst.record.check_alarms(&mut inst.common);
            }

            // Evaluate alarms
            instance.evaluate_alarms();

            let is_soft = instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";

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

            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            apply_timestamp(&mut instance.common, is_soft);
            // UDF was already updated before `evaluate_alarms` above.

            // Clear PACT
            instance
                .processing
                .store(false, std::sync::atomic::Ordering::Release);

            // Put-notify completion is NOT fired here. The async device
            // round-trip has finished, but the OUT/FLNK/process-action
            // tail it drives (below) may itself reach an async target;
            // firing now would report WRITE_NOTIFY done while that chain
            // still runs. The originating record `leave`s the wait-set at
            // the END of this function, after every PP target it drives
            // has joined. See `complete_put_notify` at the tail.

            use crate::server::recgbl::EventMask;
            let (include_val, include_archive) = match instance.record.monitor_value_changed() {
                // lsi/lso post VALUE|LOG only when the string actually
                // changed (C `lsiRecord.c`/`lsoRecord.c` monitor: `len !=
                // olen || memcmp(oval, val, len)`); they have no MDEL/ADEL
                // deadband to express that, so the gate is explicit. The
                // MPST/APST `menuPost` "Always" override OR-adds DBE_VALUE /
                // DBE_LOG even on an unchanged cycle (C monitor: `if (mpst ==
                // menuPost_Always) events |= DBE_VALUE; if (apst ==
                // menuPost_Always) events |= DBE_LOG;`).
                Some(changed) => {
                    let (val_always, archive_always) = instance.record.monitor_always_post();
                    (changed || val_always, changed || archive_always)
                }
                None => {
                    if instance.record.uses_monitor_deadband() {
                        instance.check_deadband_ext()
                    } else {
                        // Binary records (bi/bo/busy/mbbi/mbbo): always post monitors
                        (true, true)
                    }
                }
            };
            // C `recGblResetAlarms` `val_mask = DBE_ALARM`
            // (recGbl.c:194/203/212) — same parity rule as the main
            // process path above (see comment there).
            let alarm_bits = if alarm_result.alarm_changed || alarm_result.amsg_changed {
                EventMask::ALARM
            } else {
                EventMask::NONE
            };

            let mut changed_fields = Vec::new();
            // Same deadband-field routing and per-field mask as the main
            // process path: the tracked field posts the classes that
            // actually fired (MDEL → DBE_VALUE, ADEL → DBE_LOG, alarm
            // movement → DBE_ALARM); a non-primary deadband field
            // (motor RBV) leaves VAL to the generic change-detection
            // loop below.
            let deadband_field = instance.record.monitor_deadband_field();
            // The mask every change-detected aux field posts with — owned by
            // `AuxPostMask`, the single resolver of the record's declared
            // narrowings of C's default `monitor_mask | DBE_VALUE | DBE_LOG`.
            let aux_post = AuxPostMask::of(instance.record.as_ref());
            // The deadband field's post — mask owned by `deadband_post`, the
            // single assembler for C's `db_post_events(&prec->val, monitor_mask)`.
            let deadband = instance.deadband_post(alarm_bits, include_val, include_archive);
            let deadband_mask = deadband.mask;
            if let Some((field, value)) = deadband.field {
                changed_fields.push((field, value, deadband_mask));
            }
            // C `recGblResetAlarms` (recGbl.c:201-220) posts each alarm
            // field with its OWN per-field mask. Mirror the synchronous
            // link path (`process_record_with_links_inner`) and
            // `process_local` exactly: SEVR=DBE_VALUE on a sevr change;
            // STAT/AMSG share `stat_mask` which carries DBE_ALARM when
            // sevr OR amsg moved and DBE_VALUE on a stat change;
            // ACKS=DBE_VALUE only when an alarm field moved AND
            // recGblResetAlarms raised it. Collapsing these into
            // `changed_fields` would post them all on one shared mask —
            // losing C's per-field granularity for `.SEVR`/`.STAT`-only
            // subscribers.
            let sevr_changed = instance.common.sevr != alarm_result.prev_sevr;
            let stat_changed = instance.common.stat != alarm_result.prev_stat;
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
            let mut alarm_posts: Vec<(&'static str, EventMask)> = Vec::new();
            if sevr_changed {
                alarm_posts.push(("SEVR", EventMask::VALUE));
            }
            if !stat_mask.is_empty() {
                alarm_posts.push(("STAT", stat_mask));
                alarm_posts.push(("AMSG", stat_mask));
            }
            // C parity (recGbl.c:216): ACKS is posted (DBE_VALUE) only
            // when an alarm field moved AND recGblResetAlarms raised it.
            if alarm_result.acks_changed && !stat_mask.is_empty() {
                alarm_posts.push(("ACKS", EventMask::VALUE));
            }
            // The cycle's subscriber posts — assembled by the single owner
            // `RecordInstance::collect_subscriber_posts`. Without change
            // detection here, every async-completion cycle would re-send every
            // subscribed auxiliary field even when unchanged; without the shared
            // owner, this path would drift from the scan path on which unchanged
            // fields C still posts.
            changed_fields.extend(instance.collect_subscriber_posts(
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
            if instance.array_hash_changed {
                if let Some(h) = instance.resolve_field("HASH") {
                    changed_fields.push(("HASH".to_string(), h, EventMask::VALUE));
                }
            }
            // UDF rides along whenever any monitored post fired this
            // cycle, carrying the union of the cycle's posted classes —
            // same rule as the main process path.
            let cycle_mask = changed_fields
                .iter()
                .fold(EventMask::NONE, |m, (_, _, fm)| m | *fm);
            if !cycle_mask.is_empty() {
                changed_fields.push((
                    "UDF".to_string(),
                    EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
                    cycle_mask,
                ));
            }
            let snapshot = crate::server::record::ProcessSnapshot { changed_fields };

            // IVOA check
            let skip_out = if instance.common.sevr == crate::server::record::AlarmSeverity::Invalid
            {
                let ivoa = instance
                    .record
                    .get_field("IVOA")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                match ivoa {
                    1 => true,
                    2 => {
                        // See the IVOA=2 comment in
                        // `process_record_with_links_inner` — IVOA=2
                        // delegates to the per-record
                        // `apply_invalid_output_value` so OVAL/RVAL/VAL
                        // get the C-convention values.
                        if let Some(ivov) = instance.record.get_field("IVOV") {
                            let _ = instance.record.apply_invalid_output_value(ivov);
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
            // `process_record_with_links_inner`. This is the async-completion
            // path (`complete_async_record_inner`), where the §4.6
            // multi-output OUT write also runs, so OEVT must post here too.
            if !skip_out {
                if let Some(event_name) = instance.record.output_event() {
                    let db = self.clone();
                    crate::runtime::task::spawn(async move {
                        db.post_event_named(&event_name).await;
                    });
                }
            }

            let can_dev_write = instance.record.can_device_write();
            let is_soft_out =
                instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
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
            } else if is_soft_out {
                if instance.parsed_out.is_writable_out_link() {
                    let out_val = instance.record.output_link_value();
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

            let flnk_name = if instance.record.should_fire_forward_link() {
                if let crate::server::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };

            (snapshot, out_info, flnk_name, alarm_posts)
        };

        // Notify subscribers
        {
            // Write guard: a value-class post advances the record's
            // already-published state (`RecordInstance::record_value_post`),
            // so posting is a `&mut` operation.
            let mut instance = rec.write().await;
            instance.notify_from_snapshot(&snapshot);
            // Post the alarm fields (SEVR/STAT/AMSG/ACKS) with their
            // individual C masks — see recGblResetAlarms above.
            for &(field, mask) in &alarm_posts {
                instance.notify_field(field, mask);
            }
        }

        // Snapshot source PUTF + put-notify wait-set for processTarget /
        // dbNotifyAdd propagation (see `write_db_link_value` doc). For the
        // async-completion path PUTF would have been set when the put
        // landed on the record; it (and wait-set membership) must
        // propagate through the (now-completing) OUT / FLNK chain so an
        // async target reached here also defers WRITE_NOTIFY completion.
        // The committed alarm propagates the same way for
        // `recGblInheritSevrMsg` MS-class inheritance.
        let (src_putf, src_notify, src_alarm) = {
            let guard = rec.read().await;
            (
                guard.common.putf,
                guard.notify.clone(),
                super::links::LinkAlarm {
                    stat: guard.common.stat,
                    sevr: guard.common.sevr,
                    amsg: guard.common.amsg.clone(),
                },
            )
        };

        // OUT link — DB *or* external `ca://`/`pva://`. Same scheme
        // dispatch as the sync path (C `dbLink.c::dbPutLink`,
        // dbLink.c:434-448).
        if let Some((link, out_val)) = out_info {
            self.write_out_link_value(
                &link,
                out_val,
                super::links::OutLinkSrc {
                    putf: src_putf,
                    notify: src_notify.as_ref(),
                    alarm: &src_alarm,
                },
                visited,
                depth,
            )
            .await;
        }

        // Multi-output dispatch (fanout/seq). This is the async-device
        // write-completion path; dfanout has no device support so it never
        // completes async — its OUT links are driven pre-commit on the
        // synchronous process path. Pass `None` (tail phase): a dfanout
        // reaching here would be skipped, which is correct (it has already
        // dispatched, or never had a value to push).
        let _ = self.dispatch_multi_output(&rec, None, visited, depth).await;

        // event record: post the named software event.
        self.dispatch_event_record(&rec).await;

        // Generic multi-output links (transform OUTA..OUTP -> A..P,
        // scalcout OUT->OVAL, epid OUTL).
        //
        // SINGLE-OWNER INVARIANT: skip any record type owned by
        // `dispatch_multi_output` (called above) so its `LNKn`/`OUTn`
        // is not dispatched twice — see the sync-path twin in
        // `run_forward_link_tail_with_putf` §4.6.
        {
            let multi_out = {
                let instance = rec.read().await;
                // Framework IVOA=Don't_drive veto for the multi-output OUT
                // path, mirroring the single-OUT `skip_out` gate in the IVOA
                // block: on an INVALID cycle with IVOA=Don't_drive the OUT
                // write is suppressed. The parsed_out single-OUT path is
                // already gated, but `multi_output_links` (scalcout/acalcout
                // OUT) was not — so a non-calc-fail INVALID (INP LINK_ALARM,
                // SIMM, NaN-VAL UDF, …) + Don't_drive wrote OUT where C
                // suppresses (execOutput nsev>=INVALID → Don't_drive break,
                // sCalcoutRecord.c:794). The record-level OOPT/calc-fail
                // decision still gates via `multi_output_links()` itself; this
                // is the framework IVOA layer on top, for every INVALID source.
                let ivoa_dont_drive = instance.common.sevr
                    == crate::server::record::AlarmSeverity::Invalid
                    && matches!(
                        instance.record.get_field("IVOA"),
                        Some(EpicsValue::Short(1))
                    );
                let links = if ivoa_dont_drive
                    || super::links::multi_output_dispatch_owned(instance.record.record_type())
                {
                    &[][..]
                } else {
                    instance.record.multi_output_links()
                };
                if links.is_empty() {
                    None
                } else {
                    let mut pairs = Vec::new();
                    for &(link_field, val_field) in links {
                        let link_str = instance
                            .record
                            .get_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        if link_str.is_empty() {
                            continue;
                        }
                        if let Some(val) = instance.record.get_field(val_field) {
                            pairs.push((link_str, val));
                        }
                    }
                    if pairs.is_empty() { None } else { Some(pairs) }
                }
            };
            if let Some(pairs) = multi_out {
                for (link_str, val) in pairs {
                    // `multi_output_links` carries record OUT links
                    // (sseq `LNKn`, scalcout `OUTn` — all `DBF_OUTLINK`):
                    // a bare DB link is NPP (`dbDbLink.c:388`).
                    // `parse_output_link_v2` applies the OUT-link-correct
                    // NPP default; an external `ca://`/`pva://` link is
                    // routed through the link set's `putValue` — see the
                    // sync-path twin above.
                    let parsed = crate::server::record::parse_output_link_v2(
                        link_str.as_str_lossy().as_ref(),
                    );
                    self.write_out_link_value(
                        &parsed,
                        val,
                        super::links::OutLinkSrc {
                            putf: src_putf,
                            notify: src_notify.as_ref(),
                            alarm: &src_alarm,
                        },
                        visited,
                        depth,
                    )
                    .await;
                }
            }
        }

        // FLNK -- only process if target is Passive (C `dbScanFwdLink` ->
        // `dbScanPassive` -> `processTarget` propagates PUTF the same way
        // OUT links do).
        if let Some(ref flnk) = flnk_name {
            if let Some(target_rec) = self.get_record(flnk).await {
                let (target_scan, should_process) = {
                    let mut tg = target_rec.write().await;
                    let pact = tg.is_processing();
                    let on_chain = visited.contains(flnk);
                    let scan = tg.common.scan;
                    if !pact {
                        tg.common.putf = src_putf;
                        // C `dbNotifyAdd` (dbDbLink.c:460) is reached only
                        // inside `processTarget`, which `dbScanPassive`
                        // calls solely for a passive target. Gate the join
                        // on the same passive condition as the process
                        // call below so a dropped (non-passive) target
                        // never `enter`s the wait-set without `leave`ing.
                        if scan == crate::server::record::ScanType::Passive {
                            join_put_notify(&mut tg, src_notify.as_ref());
                        }
                    } else if src_putf && !on_chain {
                        tg.common.rpro = true;
                        tg.common.putf = false;
                    }
                    (scan, !pact)
                };
                if should_process && target_scan == crate::server::record::ScanType::Passive {
                    // recursive FLNK within one chain — gate
                    // already held by the foreign entry record.
                    let _ = self
                        .process_record_with_links_recursive(flnk, visited, depth + 1)
                        .await;
                }
            }
        }

        // FLNK whose target is external (`pva://`/`ca://`): forwarded
        // through the same single owner as the synchronous tail (C
        // `dbScanFwdLink` → lset `scanForward`). `flnk_name` above only
        // names a local DB target.
        self.dispatch_external_forward_link(&rec).await;

        // CP link targets
        self.dispatch_cp_targets(name, visited, depth).await;

        // RPRO: C `recGblFwdLink` consumes a pending reprocess via
        // `scanOnce` — queued, not recursed. Mirror the synchronous
        // path: spawn a fresh process pass (clean `visited`, depth 0).
        {
            let needs_rpro = {
                let mut guard = rec.write().await;
                if guard.common.rpro {
                    guard.common.rpro = false;
                    true
                } else {
                    false
                }
            };
            if needs_rpro {
                let db = self.clone();
                let rpro_name = name.to_string();
                crate::runtime::task::spawn(async move {
                    let mut fresh_visited = std::collections::HashSet::new();
                    let _ = db
                        .process_record_with_links(&rpro_name, &mut fresh_visited, 0)
                        .await;
                });
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
            let mut guard = rec.write().await;
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
            let mut guard = rec.write().await;
            complete_put_notify(&mut guard);
        }

        Ok(())
    }

    /// Dispatch CP-link targets that take a CP/CPP input link from `name`.
    ///
    /// C parity (a4bc0db): the CP-driven dispatch is the moral equivalent of
    /// dbCaTask's CA_DBPROCESS handler invoking `db_process(prec)`. Before
    /// processing each target, set PUTF=true; if the target is already
    /// processing (async record mid-flight), set RPRO=true instead so the
    /// in-flight pass reprocesses on completion. Already-visited targets
    /// (current process chain) are skipped via the `visited` cycle guard.
    async fn dispatch_cp_targets(
        &self,
        name: &str,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) {
        let cp_targets = self.get_cp_targets(name).await;
        for target in cp_targets {
            self.process_one_cp_target(&target, visited, depth).await;
        }
    }

    /// Process a single CP/CPP target edge, applying the CPP passive gate
    /// and the PACT/RPRO pre-check. This is the single owner of the
    /// scan-time CP-dispatch decision, shared by the local-source path
    /// ([`Self::dispatch_cp_targets`]) and the cross-IOC path
    /// ([`Self::dispatch_external_cp_targets`]) so both honour the same
    /// `dbCa.c` semantics.
    async fn process_one_cp_target(
        &self,
        target: &super::CpTarget,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) {
        if visited.contains(&target.record) {
            return;
        }
        let target_rec = {
            let records = self.inner.records.read().await;
            records.get(&target.record).cloned()
        };
        let mut skip = false;
        if let Some(ref t) = target_rec {
            let mut tg = t.write().await;
            if target.passive_only && tg.common.scan != crate::server::record::ScanType::Passive {
                // CPP gate (`dbCa.c:854,994,1072`): a CPP link adds
                // `CA_DBPROCESS` only when the link-holder's SCAN is
                // Passive. A non-Passive target is reached by its own
                // periodic/event scan, so skip it here — no process,
                // no RPRO. A CP link (`passive_only == false`) never
                // takes this branch and always processes.
                skip = true;
            } else if tg.processing.load(std::sync::atomic::Ordering::Acquire) {
                tg.common.rpro = true;
                skip = true;
            }
            // else (not processing): fall through and process below.
            // epics-base PR #3fb10b6: PUTF must remain false on
            // CP-driven targets — only the record directly receiving
            // the dbPut reports PUTF=1 to dbNotify/onChange observers,
            // so we deliberately do NOT set PUTF here.
        }
        if skip {
            return;
        }
        // recursive CP-target fan-out within one chain —
        // gate already held by the foreign entry record.
        let _ = self
            .process_record_with_links_recursive(&target.record, visited, depth + 1)
            .await;
    }

    /// Process every holder of an EXTERNAL CP/CPP link to `external_pv` —
    /// the cross-IOC twin of [`Self::dispatch_cp_targets`]. Called by the
    /// calink/pvalink CA monitor callback on every remote change, this is
    /// the Rust equivalent of C `dbCa.c eventCallback` adding
    /// `CA_DBPROCESS` for a CP (or Passive CPP) link (`dbCa.c:993-994`)
    /// and the worker thread running `db_process(prec)` (`dbCa.c:1295`).
    /// A cross-IOC source never processes locally, so this callback is the
    /// only trigger; without it a `CP`/`CPP` link's holder never processes
    /// on a remote change.
    ///
    /// A fresh `visited` set and `depth = 0` start a new process chain —
    /// the monitor event is an independent external trigger, like a scan,
    /// not a continuation of an in-flight local chain.
    pub async fn dispatch_external_cp_targets(&self, external_pv: &str) {
        let targets = self.get_external_cp_targets(external_pv).await;
        if targets.is_empty() {
            return;
        }
        let mut visited = std::collections::HashSet::new();
        for target in targets {
            self.process_one_cp_target(&target, &mut visited, 0).await;
        }
    }

    /// Write a simulation value to an output record's SIOL link,
    /// dispatching by link type and locality exactly as C `dbPutLink`
    /// (reached from `writeValue` for a SIMM-mode output record):
    ///
    /// - a **local DB** target uses the already-locked write — writing
    ///   VAL is an internal step of this record's processing chain,
    ///   which already holds the entry record's advisory write gate, so
    ///   a SIOL pointing back at a chain record must not re-acquire the
    ///   non-reentrant gate (same reasoning as `write_db_link_value`);
    /// - a **non-local DB** target (`dbInitLink` made it a CA link) and
    ///   an explicit **`Ca`/`Pva`** link route through the lset put path;
    /// - constant / hardware / none SIOL targets are not writable — no-op
    ///   (C `dbPutLink` -> `S_db_noLSET`).
    async fn write_sim_siol_value(
        &self,
        siol: &crate::server::record::ParsedLink,
        value: EpicsValue,
    ) {
        match siol {
            crate::server::record::ParsedLink::Db(link) => {
                let pv_name = if link.field == "VAL" {
                    link.record.clone()
                } else {
                    format!("{}.{}", link.record, link.field)
                };
                if self.has_name_no_resolve(&link.record).await {
                    let _ = self.put_pv_already_locked(&pv_name, value).await;
                } else if let Err(e) = self
                    .write_external_pv(&pv_name, value, crate::server::database::LinkPutOp::Plain)
                    .await
                {
                    eprintln!("SIOL simulation write to external PV '{pv_name}' failed: {e}");
                }
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                let name = siol
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                if let Err(e) = self
                    .write_external_pv(&name, value, crate::server::database::LinkPutOp::Plain)
                    .await
                {
                    eprintln!("SIOL simulation write to external PV '{name}' failed: {e}");
                }
            }
            _ => {}
        }
    }

    /// Apply the SIMM-mode OUTPUT redirect (the `writeValue` half of
    /// simulation). C `writeValue` substitutes the device write with
    /// `dbPutLink(&prec->siol, ..., &prec->oval)` at the END of `process()`,
    /// so this runs from the OUT epilogue after the body computed OVAL/RVAL.
    /// `sim_output` is `None` for a non-simulated record or a simulated INPUT
    /// (whose `readValue` ran up-front); `skip_out` carries the IVOA
    /// Don't_drive veto so the SIOL write is suppressed exactly as the real
    /// device write would be.
    ///
    /// Kept as its own `async fn` so the `EpicsValue` it reads out of the
    /// record never enters `process_record_with_links_inner`'s async state —
    /// that future is polled `MAX_LINK_DEPTH` frames deep on a FLNK chain, and
    /// bloating it overflows the stack (the depth-limit regression tests).
    async fn write_simulated_output_siol(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
        sim_output: &Option<(crate::server::record::ParsedLink, i16, bool)>,
        skip_out: bool,
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
            let instance = rec.read().await;
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
            self.write_sim_siol_value(siol, value).await;
        }
    }

    /// C `dbGetLink` / `dbTryGetLink` on a SIMULATION link (SIML or SIOL),
    /// classified into the three outcomes C's `(status, buffer)` pair can
    /// carry — see [`crate::server::recgbl::simm::SimLinkFetch`].
    ///
    /// The generic [`Self::read_link_value_no_process`] collapses two of them:
    /// it hands back the CONSTANT link's parsed text as if the link had
    /// delivered it this cycle, and `None` both for "constant with nothing to
    /// give" and for "the read failed". C keeps them apart —
    /// `dbConstGetValue` (`dbConstLink.c:219-225`) returns SUCCESS and writes
    /// nothing, because a constant's value was already loaded into the
    /// record's buffer at `init_record` — and every simulation-mode decision
    /// hangs off that distinction. So the simulation path gets its own
    /// classifier; the constant's value still reaches the record, through
    /// [`Self::rec_gbl_init_simm`].
    pub(crate) async fn fetch_sim_link(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> crate::server::recgbl::simm::SimLinkFetch {
        use crate::server::recgbl::simm::SimLinkFetch;
        if crate::server::recgbl::simm::is_constant(link) {
            return SimLinkFetch::NoData;
        }
        match self.read_link_value_no_process(link).await {
            Some(v) => SimLinkFetch::Value(v),
            None => SimLinkFetch::Failed,
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
    pub(crate) async fn rec_gbl_get_simm(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
        siml: &crate::server::record::ParsedLink,
    ) {
        use crate::server::recgbl::simm::SimLinkFetch;
        // `recGblSaveSimm(*psscn, poldsimm, *psimm)` — latch the outgoing mode
        // BEFORE the SIML read can move SIMM.
        {
            let mut instance = rec.write().await;
            instance.rec_gbl_save_simm();
        }
        // `dbTryGetLink`: a CONSTANT (or unset) SIML delivers NOTHING here —
        // its value was loaded into SIMM once, at init (`rec_gbl_init_simm`).
        // So a `caput REC.SIMM YES` on a record with a constant SIML STAYS
        // YES; re-reading the constant every cycle (the pre-fix behaviour of
        // `read_link_value_no_process`) would stomp the operator's put back to
        // the constant on the very next process.
        match self.fetch_sim_link(siml).await {
            SimLinkFetch::Value(v) => {
                let simm = v.to_f64().unwrap_or(0.0) as i16;
                let mut instance = rec.write().await;
                let _ = instance
                    .record
                    .put_field_internal("SIMM", EpicsValue::Short(simm));
            }
            // status 0, nothing written — SIMM keeps what init loaded.
            SimLinkFetch::NoData => {}
            // The read FAILED. Two C shapes, keyed on which SIML reader the
            // record's support uses (`Record::uses_recgbl_simm_helpers`):
            SimLinkFetch::Failed => {
                let mut instance = rec.write().await;
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
                    // (dbLink.c:319-323) — a full
                    // `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "field %s")`.
                    crate::server::recgbl::rec_gbl_set_sevr_msg(
                        &mut instance.common,
                        crate::server::recgbl::alarm_status::LINK_ALARM,
                        crate::server::record::AlarmSeverity::Invalid,
                        "field SIML",
                    );
                }
            }
        }
        // `recGblCheckSimm(pcommon, psscn, *poldsimm, *psimm)` — a SIML-driven
        // SIMM transition swaps SCAN with SSCN exactly like a `caput REC.SIMM`
        // does.
        self.apply_simm_scan_swap(rec).await;
    }

    /// Run C `recGblCheckSimm` on a record and hand the resulting scan move to
    /// the scan-index owner (`update_scan_index`) — the `scanDelete`/`scanAdd`
    /// pair inside it. The record lock is taken and released here: the
    /// scan-index update re-enters the database.
    pub(crate) async fn apply_simm_scan_swap(&self, rec: &Arc<RwLock<RecordInstance>>) {
        use crate::server::record::CommonFieldPutResult;
        let (name, result) = {
            let mut instance = rec.write().await;
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
            self.update_scan_index(&name, old_scan, new_scan, phas, phas)
                .await;
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
    /// [`Self::fetch_sim_link`] enforces; without it a `field(SIOL, "42")`
    /// would never reach SVAL at all.
    ///
    /// Must be called once per record, after its fields are applied — the
    /// `init_record(1)` sites (`ioc_builder`, `dbLoadRecords`).
    pub(crate) async fn rec_gbl_init_simm(&self, rec: &Arc<RwLock<RecordInstance>>) {
        let mut instance = rec.write().await;
        // No SIMM field -> no simulation block -> nothing to init.
        if instance.record.get_field("SIMM").is_none() {
            return;
        }
        // `recGblSaveSimm(*psscn, poldsimm, *psimm)` — the latch, before the
        // constant SIML can move SIMM.
        instance.rec_gbl_save_simm();
        let link_of = |instance: &RecordInstance, field: &str| {
            instance.record.get_field(field).and_then(|v| {
                if let EpicsValue::String(s) = v {
                    Some(crate::server::record::parse_link_v2(
                        s.as_str_lossy().as_ref(),
                    ))
                } else {
                    None
                }
            })
        };
        // `if (dbLinkIsConstant(psiml)) dbLoadLink(psiml, DBF_USHORT, psimm);`
        if let Some(siml) = link_of(&instance, "SIML") {
            if let Some(v) = crate::server::recgbl::simm::constant_load_value(&siml) {
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
        drop(instance);
        self.apply_simm_scan_swap(rec).await;
    }

    /// Check simulation mode for a record. Returns
    /// `SimOutcome::Simulated` when a simulated INPUT handled the value (the
    /// caller still runs the forward-link tail),
    /// `SimOutcome::RedirectOutputToSiol` when a simulated OUTPUT needs the
    /// uniform body to run first, or `SimOutcome::NotSimulated` when normal
    /// processing should proceed.
    async fn check_simulation_mode(&self, rec: &Arc<RwLock<RecordInstance>>) -> SimOutcome {
        // Read SIML, SIMM, SIOL, SIMS, SDLY from the record
        let (siml_link, siol_link, sims, sdly, _rtype, is_input, input_stage, pact_held) = {
            let instance = rec.read().await;
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
            // VAL back out. `waveform` is exact: the SIOL array lands in VAL via
            // `set_val`. `histogram` reads SIOL but lands the scalar in VAL via
            // the shared `set_val` path, which no-ops against the bin-count
            // array — so a simulated histogram is frozen (no SIOL->SVAL feed, no
            // bin accumulation). That residual is pre-existing and unmodeled;
            // the classification only ensures a simulated histogram no longer
            // performs the real device read or corrupts the SIOL target.
            //
            // `aai` is also a SIOL-reading input, but the SIOL read lives in
            // its soft DEVICE support, not the record support. `aaiRecord.c::
            // readValue` (:348) raises SIMM_ALARM then calls `read_aai`, and
            // `devAaiSoft.c::read_aai` (:88) reads
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
                );

            let siml = instance
                .record
                .get_field("SIML")
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let siol = instance
                .record
                .get_field("SIOL")
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let sims = instance
                .record
                .get_field("SIMS")
                .and_then(|v| {
                    if let EpicsValue::Short(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            // SDLY ("Sim. Mode Async Delay", DBF_DOUBLE, dbd initial
            // "-1.0"). Absent on record types whose SIMM group Rust does not
            // yet fully model — default to -1.0 (synchronous) so the async
            // branch is a no-op there, exactly as a record with the C default
            // behaves.
            let sdly = instance
                .record
                .get_field("SDLY")
                .and_then(|v| v.to_f64())
                .unwrap_or(-1.0);

            // The entry gate is the SIM BLOCK's own marker — the SIMM field.
            // C's `readValue`/`writeValue` exists only on a record whose dbd
            // declares SIMM, and it dispatches on SIMM alone; the SIML/SIOL
            // links are read INSIDE that dispatch, never as a precondition for
            // it. Gating on "SIML and SIOL are both empty" (the pre-fix gate)
            // made `caput REC.SIMM 1` + `caput REC.SVAL 42` — simulate against
            // a constant, the standard idiom — a complete no-op on every
            // record, because an unset SIOL is exactly the case C serves from
            // SVAL (R12-61).
            if instance.record.get_field("SIMM").is_none() {
                return SimOutcome::NotSimulated; // no simulation block
            }

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
            self.rec_gbl_get_simm(rec, &siml_link).await;
        }

        // Check SIMM. The dispatch is the record's own C `switch (prec->simm)`,
        // whose legal arms are the choices of ITS SIMM menu — `resolve_sim_mode`
        // is the single owner of that fact.
        let mode = {
            let instance = rec.read().await;
            crate::server::recgbl::simm::resolve_sim_mode(&*instance.record)
        };

        if !mode.is_simulated() {
            return SimOutcome::NotSimulated; // menuSimmNO — proceed normally
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
            let mut instance = rec.write().await;
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
            // record must be left idle.
            if pact_held {
                instance
                    .processing
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            let is_output = !is_input;
            drop(instance);
            return SimOutcome::IllegalMode { is_output };
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
            return SimOutcome::DeferRead(std::time::Duration::from_secs_f64(sdly));
        }

        // INPUT-STAGE record (swait). C `swaitRecord.c:415-421`:
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
            let fetch = self.fetch_sim_link(&siol_link).await;
            let mut instance = rec.write().await;
            // C `:417-420` — `if (status == 0) { val = sval; udf = FALSE; }`.
            // A CONSTANT (or unset) SIOL is `status == 0` with SVAL untouched
            // (`dbConstGetValue`), so it still copies SVAL into VAL; only a
            // FAILED read changes neither VAL nor UDF. The SIMM_ALARM below is
            // unconditional either way.
            if fetch.is_ok() {
                if let crate::server::recgbl::simm::SimLinkFetch::Value(v) = fetch {
                    let sval = EpicsValue::Double(v.to_f64().unwrap_or(0.0));
                    let _ = instance.record.put_field_internal("SVAL", sval);
                }
                if let Some(sval) = instance.record.get_field("SVAL") {
                    let _ = instance.record.set_val(sval);
                }
                instance.common.udf = false;
            }
            let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
            crate::server::recgbl::rec_gbl_set_sevr(
                &mut instance.common,
                crate::server::recgbl::alarm_status::SIMM_ALARM,
                sev,
            );
            return SimOutcome::SimulatedInputStage;
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
            if pact_held {
                let instance = rec.write().await;
                instance
                    .processing
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            return SimOutcome::RedirectOutputToSiol {
                siol: siol_link,
                sims,
                raw_mode,
            };
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
        {
            // Read from SIOL -> SVAL -> VAL/RVAL. Uniform across Db (with
            // locality fallback) / Ca / Pva / constant via `fetch_sim_link`
            // (C `dbGetLink`), which keeps C's three outcomes apart: a value,
            // a CONSTANT link's "status 0 with the buffer untouched", and a
            // failure.
            let fetch = self.fetch_sim_link(&siol_link).await;
            let mut instance = rec.write().await;

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
                crate::server::recgbl::simm::SimLinkFetch::Value(v) => {
                    if has_sval {
                        // `put_field_internal` is the DBR-coercion owner
                        // (C `dbGetLink(DBF_<sval>)`).
                        let _ = instance.record.put_field_internal("SVAL", v.clone());
                        instance.record.get_field("SVAL")
                    } else {
                        Some(v.clone())
                    }
                }
                crate::server::recgbl::simm::SimLinkFetch::NoData => {
                    if has_sval {
                        instance.record.get_field("SVAL")
                    } else {
                        None
                    }
                }
                crate::server::recgbl::simm::SimLinkFetch::Failed => None,
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
                    let rval_type = instance
                        .record
                        .field_list()
                        .iter()
                        .find(|f| f.name == "RVAL")
                        .map(|f| f.dbf_type)
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
                    let ctx = instance.common.process_context();
                    instance.record.set_process_context(&ctx);
                    let _ = instance.record.process();
                } else {
                    // Records without RVAL fall back to SIMM=YES
                    // semantics: the SIOL value goes straight into
                    // VAL; no conversion to run.
                    let _ = instance.record.set_val(siol_val);
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
            sim_process_tail(&mut instance, SimTailAlarm::Simm(sims), fetch.is_ok());
        }

        // C `readValue`/`writeValue` clears `pact` on the synchronous branch
        // (`prec->pact = FALSE`, aiRecord.c:496 / aoRecord.c:578). On the
        // SDLY continuation this releases the PACT held across the delay so the
        // forward-link tail and any subsequent foreign process see the record
        // idle (C posts `monitor()` + `recGblFwdLink` with pact already
        // FALSE). An entry that never held PACT (a fresh `sdly < 0` cycle, or a
        // `pact=FALSE` re-trigger) has nothing to release, so the clear is gated
        // on `pact_held` to avoid a needless write-lock there.
        if pact_held {
            let instance = rec.write().await;
            instance
                .processing
                .store(false, std::sync::atomic::Ordering::Release);
        }

        SimOutcome::Simulated
    }
}

/// Which alarm the simulated cycle's tail raises before it commits.
///
/// C raises SIMM_ALARM at SIMS on the YES/RAW arms only
/// (`recGblSetSevr(prec, SIMM_ALARM, prec->sims)`). The `default:` arm raises
/// SOFT_ALARM/INVALID *instead*, and does so where it is detected — before the
/// tail — so the tail has nothing left to raise.
#[derive(Debug, Clone, Copy)]
enum SimTailAlarm {
    /// The YES/RAW arms: `recGblSetSevr(prec, SIMM_ALARM, prec->sims)`.
    Simm(i16),
    /// The `default:` arm: SOFT_ALARM/INVALID is already pending; raise nothing.
    None,
}

/// Shared tail of a simulated (`SIMM` != NO) process cycle — the part of
/// C `process()` that still runs when `readValue`/`writeValue` divert to
/// the SIOL (`aiRecord.c` and every SIML/SIMM-bearing record):
/// `recGblSetSevr(prec, SIMM_ALARM, prec->sims)` — a MAXIMIZE into the
/// pending nsta/nsev raised first so it wins severity ties (C order:
/// readValue before checkAlarms) — then `checkAlarms`,
/// `recGblResetAlarms`, and `monitor()`, so the simulated value still
/// trips its own limit/state alarms and the SIMM severity maximizes
/// against them.
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
///   raised it (recGbl.c:201-220);
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
fn sim_process_tail(instance: &mut RecordInstance, alarm: SimTailAlarm, clear_udf: bool) {
    use crate::server::recgbl::EventMask;

    apply_timestamp(&mut instance.common, true);
    // C clears UDF only on a `status == 0` SIOL read (`longinRecord.c:418`,
    // `waveformRecord.c:352`) — a failed read leaves the record undefined.
    if clear_udf {
        instance.common.udf = false;
    }

    if let SimTailAlarm::Simm(sims) = alarm {
        let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
        crate::server::recgbl::rec_gbl_set_sevr(
            &mut instance.common,
            crate::server::recgbl::alarm_status::SIMM_ALARM,
            sev,
        );
    }
    {
        let inst = &mut *instance;
        inst.record.check_alarms(&mut inst.common);
    }
    instance.evaluate_alarms();
    let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

    let alarm_bits = if alarm_result.alarm_changed || alarm_result.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
    };

    let (include_val, include_archive) = match instance.record.monitor_value_changed() {
        Some(changed) => {
            let (val_always, archive_always) = instance.record.monitor_always_post();
            (changed || val_always, changed || archive_always)
        }
        None => {
            if instance.record.uses_monitor_deadband() {
                instance.check_deadband_ext()
            } else {
                (true, true)
            }
        }
    };
    let deadband_field = instance.record.monitor_deadband_field();
    // The mask every change-detected aux field posts with — owned by
    // `AuxPostMask`, the single resolver of the record's declared narrowings of
    // C's default `monitor_mask | DBE_VALUE | DBE_LOG`.
    let aux_post = AuxPostMask::of(instance.record.as_ref());
    // The deadband field's post — mask owned by `deadband_post`, the single
    // assembler for C's `db_post_events(&prec->val, monitor_mask)`.
    let deadband = instance.deadband_post(alarm_bits, include_val, include_archive);
    let deadband_mask = deadband.mask;
    let mut changed_fields = Vec::new();
    if let Some((field, value)) = deadband.field {
        changed_fields.push((field, value, deadband_mask));
    }

    let sevr_changed = instance.common.sevr != alarm_result.prev_sevr;
    let stat_changed = instance.common.stat != alarm_result.prev_stat;
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

    // The cycle's subscriber posts — assembled by the single owner
    // `RecordInstance::collect_subscriber_posts`. The simulation path is a
    // process cycle like any other, so it obeys the same rules (this copy used
    // to omit the `process_posted_fields` gate; the shared owner applies it).
    changed_fields.extend(instance.collect_subscriber_posts(
        deadband_field,
        deadband_mask,
        alarm_bits,
        aux_post,
        include_val,
    ));
    // C waveform/aai/aao `monitor()` posts HASH with a literal `DBE_VALUE`
    // only on a content-hash change (waveformRecord.c:317-319), independent
    // of the VAL post mask. `array_hash_changed` was set by
    // `check_deadband_ext` this cycle.
    if instance.array_hash_changed {
        if let Some(h) = instance.resolve_field("HASH") {
            changed_fields.push(("HASH".to_string(), h, EventMask::VALUE));
        }
    }
    let cycle_mask = changed_fields
        .iter()
        .fold(EventMask::NONE, |m, (_, _, fm)| m | *fm);
    if !cycle_mask.is_empty() {
        changed_fields.push((
            "UDF".to_string(),
            EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
            cycle_mask,
        ));
    }

    let snapshot = crate::server::record::ProcessSnapshot { changed_fields };
    instance.notify_from_snapshot(&snapshot);
    if sevr_changed {
        instance.notify_field("SEVR", EventMask::VALUE);
    }
    if !stat_mask.is_empty() {
        instance.notify_field("STAT", stat_mask);
        instance.notify_field("AMSG", stat_mask);
    }
    if alarm_result.acks_changed && !stat_mask.is_empty() {
        instance.notify_field("ACKS", EventMask::VALUE);
    }
}
