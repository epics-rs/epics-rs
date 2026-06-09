use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::runtime::sync::RwLock;
use crate::server::record::{NotifyWaitSet, RecordInstance};
use crate::types::EpicsValue;

use super::{PvDatabase, apply_timestamp};

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
/// C `aiRecord.c:151-168` handles simulation entirely inside
/// `readValue()`; `process()` then ALWAYS runs `convert`/`checkAlarms`/
/// `monitor`/`recGblFwdLink(prec)`. A simulated record therefore must
/// NOT skip the forward-link / CP / RPRO tail — only the device read
/// and record-support body are replaced by the SIOL round-trip.
enum SimOutcome {
    /// SIMM disabled / no simulation link configured: run the record
    /// body normally.
    NotSimulated,
    /// Simulation handled the record value (SIOL read/write done).
    /// The caller must still run the forward-link / CP / RPRO tail
    /// exactly as `recGblFwdLink` does for a real process cycle.
    Simulated,
}

impl PvDatabase {
    /// Process a record by name (process_local + notify).
    /// Alias-aware (epics-base PR #336).
    pub async fn process_record(&self, name: &str) -> CaResult<()> {
        self.process_record_inner(name, true).await
    }

    /// `process_record` variant for a caller that already
    /// owns the record's advisory write gate — the QSRV atomic group
    /// PUT applying a `+proc` member. The gate `Mutex` is not
    /// reentrant; the atomic group path MUST use this entry. See
    /// [`crate::server::database::PvDatabase::lock_records`].
    pub async fn process_record_already_locked(&self, name: &str) -> CaResult<()> {
        self.process_record_inner(name, false).await
    }

    async fn process_record_inner(&self, name: &str, acquire_gate: bool) -> CaResult<()> {
        let rec = self.get_record(name).await;

        if let Some(rec) = rec {
            // advisory write gate (`dbScanLock` analogue). A
            // QSRV atomic group with a `+proc` member holds this
            // record's gate via `lock_records`; a direct
            // `process_record` on the same backing record must block
            // until the atomic group transaction completes. Skipped
            // when the caller already owns the gate.
            let _record_gate = if acquire_gate {
                let canonical = self
                    .resolve_alias(name)
                    .await
                    .unwrap_or_else(|| name.to_string());
                Some(self.lock_record(&canonical).await)
            } else {
                None
            };
            let (snapshot, alarm_posts) = {
                let mut instance = rec.write().await;
                instance.process_local()?
            };
            // Notify outside lock
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
            // Post the alarm fields (SEVR/STAT/ACKS) with their
            // individual C masks — see `process_local` / recGblResetAlarms.
            for &(field, mask) in &alarm_posts {
                instance.notify_field(field, mask);
            }
            Ok(())
        } else {
            Err(CaError::ChannelNotFound(name.to_string()))
        }
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
            self.process_record_with_links_inner(name, visited, depth, false, true)
                .await
        })
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
            self.process_record_with_links_inner(name, visited, depth, false, false)
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
            self.process_record_with_links_inner(name, visited, depth, false, false)
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
            self.process_record_with_links_inner(name, visited, depth, true, true)
                .await
        })
    }

    async fn process_record_with_links_inner(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        depth: usize,
        is_continuation: bool,
        acquire_gate: bool,
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
                // Post VAL event with VALUE | LOG mask (mirrors C
                // `db_post_events(prec, &VAL, DBE_VALUE|DBE_LOG)`).
                let mut changed_fields = Vec::new();
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
                changed_fields.push((
                    "SEVR".to_string(),
                    EpicsValue::Short(instance.common.sevr as i16),
                ));
                changed_fields.push((
                    "STAT".to_string(),
                    EpicsValue::Short(instance.common.stat as i16),
                ));
                // Include AMSG so subscribers reading the alarm text
                // observe "Async in progress" alongside the SCAN_ALARM
                // transition (C `recGbl.c:210-211` posts STAT and AMSG
                // together when `stat_mask` is non-zero).
                changed_fields.push((
                    "AMSG".to_string(),
                    EpicsValue::String(instance.common.amsg.clone().into()),
                ));
                let snapshot = crate::server::record::ProcessSnapshot {
                    changed_fields,
                    event_mask: crate::server::recgbl::EventMask::VALUE
                        | crate::server::recgbl::EventMask::LOG
                        | crate::server::recgbl::EventMask::ALARM,
                };
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
        // C `aiRecord.c:151-168`: simulation is handled inside
        // `readValue()`, then `process()` ALWAYS runs `convert` /
        // `checkAlarms` / `monitor` / `recGblFwdLink(prec)`. A
        // simulated record therefore must still run the forward-link /
        // CP / RPRO tail — only the device read and record-support
        // body are replaced by the SIOL round-trip. Returning early
        // here would silently break every FLNK / CP chain downstream
        // of any record in SIMM mode.
        match self.check_simulation_mode(&rec).await {
            SimOutcome::NotSimulated => {}
            SimOutcome::Simulated => {
                self.run_forward_link_tail(name, &rec, visited, depth).await;
                return Ok(());
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
            // OMSL-bearing C record; the Rust port does not implement
            // aao (confirmed: no `crates/epics-base-rs/src/server/records/aao*.rs`),
            // so it is a future gap, not a same-defect-not-fixed site.
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
                        Some((dol_parsed, oif))
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
            Some(name) => self.external_link_time(name).await,
            None => None,
        };

        // Read DOL value
        let dol_value = if let Some((ref dol_parsed, _oif)) = dol_info {
            self.read_link_value(dol_parsed, visited, depth).await
        } else {
            None
        };

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
        {
            let link_info: Vec<(String, &'static str, String)> = {
                let instance = rec.read().await;
                instance
                    .record
                    .multi_input_links()
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
                }
            }
            multi_input_values = results;
        }
        // PR #d0cf47c continued: feed the INP alarm (if any) into the
        // same `link_alarms` list the lock-section iterates over. Order
        // doesn't matter — `rec_gbl_set_sevr_msg` takes the maximum
        // severity across all sources.
        if let Some(pair) = inp_link_alarm {
            link_alarms.push(pair);
        }

        // 1.6. Sel NVL link: resolve NVL -> SELN
        let sel_nvl_value: Option<EpicsValue> = {
            let instance = rec.read().await;
            if instance.record.record_type() == "sel" {
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
                if !nvl_str.is_empty() {
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

        // 2. Lock record, apply INP/DOL, process, evaluate alarms, build snapshot
        let (snapshot, out_info, flnk_name, process_actions, alarm_posts) = {
            let mut instance = rec.write().await;

            // Apply DOL value for output records (OMSL=CLOSED_LOOP)
            if let Some(dol_val) = dol_value {
                let oif = dol_info.as_ref().map(|(_, oif)| *oif).unwrap_or(0);
                if oif == 1 {
                    // Incremental: VAL += DOL value
                    if let (Some(cur), Some(dol_f)) = (
                        instance.record.val().and_then(|v| v.to_f64()),
                        dol_val.to_f64(),
                    ) {
                        let _ = instance.record.set_val(EpicsValue::Double(cur + dol_f));
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
            for (val_field, value) in &multi_input_values {
                if let Some(f) = value.to_f64() {
                    let _ = instance
                        .record
                        .put_field_internal(val_field, EpicsValue::Double(f));
                }
            }

            // Tell the record which input link fields actually resolved
            // a value this cycle — the framework analogue of C device
            // support inspecting `RTN_SUCCESS(dbGetLink(...))`
            // (`epidRecord.c:191-193`).
            instance
                .record
                .set_resolved_input_links(&resolved_link_fields);

            // Apply sel NVL -> SELN
            if let Some(nvl_val) = sel_nvl_value {
                if let Some(f) = nvl_val.to_f64() {
                    let _ = instance
                        .record
                        .put_field("SELN", EpicsValue::Short(f as i16));
                }
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
            if !is_soft && !is_output {
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
                self.execute_read_db_links(&rec_name, &rec, &pre_actions, visited, depth)
                    .await;
                instance = rec.write().await;
            }

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

            // Push framework-owned common state (UDF/PHAS/TSE/TSEL) so
            // the record's process() can see it — C records read
            // `dbCommon` directly (`epidRecord.c:195` checks
            // `pepid->udf`, `timestampRecord.c:90` checks `tse`).
            {
                let ctx = instance.common.process_context();
                instance.record.set_process_context(&ctx);
            }

            // Process
            let mut outcome = instance.record.process()?;
            // Merge deferred device actions into process outcome actions
            outcome.actions.extend(deferred_device_actions);
            let process_result = outcome.result;
            let process_actions = outcome.actions;

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
            if let crate::server::record::RecordProcessResult::AsyncPendingNotify(fields) =
                process_result
            {
                // Intermediate notification (e.g. DMOV=0 at move start).
                // Execute device write first so the move command reaches the driver,
                // then flush DMOV=0 etc. to monitors.
                if !is_soft {
                    if let Some(mut dev) = instance.device.take() {
                        let _ = dev.write(&mut *instance.record);
                        instance.device = Some(dev);
                    }
                }
                apply_timestamp(&mut instance.common, is_soft);
                // Filter out fields that haven't changed, update MLST/last_posted.
                let mut changed_fields = Vec::new();
                for (name, val) in fields {
                    let changed = match instance.last_posted.get(&name) {
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
                        instance.last_posted.insert(name.clone(), val.clone());
                        changed_fields.push((name, val));
                    }
                }
                let event_mask = if changed_fields.is_empty() {
                    crate::server::recgbl::EventMask::NONE
                } else {
                    crate::server::recgbl::EventMask::VALUE
                        | crate::server::recgbl::EventMask::ALARM
                };
                let snapshot = crate::server::record::ProcessSnapshot {
                    changed_fields,
                    event_mask,
                };
                let rec_clone = rec.clone();
                drop(instance);
                {
                    let inst = rec_clone.read().await;
                    inst.notify_from_snapshot(&snapshot);
                }
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
            // Previous version treated Maximize and MaximizeStatus
            // identically, propagating source stat + amsg through both
            // — that matches MSS but is wrong for MS (and MSI), which
            // C says should always surface as LINK_ALARM with no msg.
            // The per-mode switch is shared with the DB OUT-link write
            // path via `inherit_sevr_msg` so the two sides cannot drift.
            for (ms, alarm) in &link_alarms {
                super::links::inherit_sevr_msg(&mut instance.common, *ms, alarm);
            }

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

            // Transfer nsta/nsev -> sevr/stat, detect alarm change
            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            // Apply timestamp based on TSE
            apply_timestamp(&mut instance.common, is_soft);
            // NOTE: UDF was already updated before `evaluate_alarms`
            // above — keyed on `value_is_undefined()` so a NaN result
            // keeps UDF true and UDF_ALARM is raised this cycle. Do
            // NOT clear UDF unconditionally here.

            // IVOA check for output records with INVALID alarm
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

            // OUT stage: soft channel -> link put, non-soft -> device.write()
            // Must run BEFORE check_deadband_ext so MLST is not prematurely
            // updated for async writes that return early.
            let can_dev_write = instance.record.can_device_write();
            let is_soft_out =
                instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
            let record_should_output = instance.record.should_output();
            let out_info = if skip_out {
                None
            } else if !can_dev_write {
                // Non-output records (calcout, etc.) may still have a
                // soft OUT link (DB or external ca://`/`pva://`).
                // Write OVAL to OUT when the record says should_output().
                if record_should_output && instance.parsed_out.is_writable_out_link() {
                    let oval = instance.record.get_field("OVAL");
                    let val = instance.record.val();
                    let out_val = oval.or(val);
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
                    let out_val = instance
                        .record
                        .get_field("OVAL")
                        .or_else(|| instance.record.val());
                    out_val.map(|v| (instance.parsed_out.clone(), v))
                } else {
                    None
                }
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

            // Compute event mask (after OUT stage so async writes don't
            // update MLST/ALST prematurely before returning early)
            use crate::server::recgbl::EventMask;
            let mut event_mask = EventMask::NONE;

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
            if include_val {
                event_mask |= EventMask::VALUE;
            }
            if include_archive {
                event_mask |= EventMask::LOG;
            }
            if alarm_result.alarm_changed || alarm_result.amsg_changed {
                // C `recGbl.c:194/203` — amsg-only OR sevr-changed sets
                // `stat_mask = DBE_ALARM`, which raises `val_mask =
                // DBE_ALARM` at line 212. Without this, an alarm whose
                // sevr/stat is unchanged but whose amsg shifted (e.g.
                // device re-flagging the same severity with a different
                // human-readable cause) silently drops the AMSG event
                // and any DBE_ALARM-only subscribers stay stale.
                event_mask |= EventMask::ALARM;
            }

            // Build snapshot
            let mut changed_fields = Vec::new();
            // C `recGblResetAlarms` returns `val_mask = DBE_ALARM`
            // when any alarm field moved — the record's VAL is posted
            // with DBE_ALARM even if the value deadband did not fire,
            // so a `DBE_ALARM`-only subscriber sees the value at the
            // moment the alarm changed.
            let val_on_alarm = alarm_result.alarm_changed || alarm_result.amsg_changed;
            if include_val || val_on_alarm {
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
            }
            // Add subscribed fields that actually changed since last notification.
            let mut sub_updates: Vec<(String, EpicsValue)> = Vec::new();
            for (field, subs) in &instance.subscribers {
                if !subs.is_empty()
                    && field != "VAL"
                    && field != "SEVR"
                    && field != "STAT"
                    && field != "AMSG"
                    && field != "UDF"
                {
                    if let Some(val) = instance.resolve_field(field) {
                        let changed = match instance.last_posted.get(field) {
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
                    instance.last_posted.insert(field.clone(), val.clone());
                }
                changed_fields.extend(sub_updates);
                event_mask |= crate::server::recgbl::EventMask::VALUE;
            }
            // C `recGblResetAlarms` (recGbl.c:201-220) posts each
            // alarm field with its own per-field mask:
            //   * SEVR — DBE_VALUE, ONLY when `prev_sevr != new_sevr`.
            //   * STAT/AMSG — `stat_mask` = DBE_ALARM (on sevr- or
            //     amsg-change) | DBE_VALUE (on stat-change).
            //   * ACKS — DBE_VALUE when `stat_mask != 0`.
            // The pre-fix port pushed SEVR + STAT together on any
            // `alarm_changed`, over-posting SEVR on a stat-only
            // transition and collapsing the per-field mask into one
            // record-wide mask. Posting these via `notify_field` with
            // their individual masks restores C's granularity.
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
            if !stat_mask.is_empty() {
                // C `val_mask = DBE_ALARM` — the value field carries
                // DBE_ALARM whenever any alarm field moved.
                event_mask |= EventMask::ALARM;
            }
            // Defer the SEVR/STAT/AMSG/ACKS posts to dedicated
            // `notify_field` calls (collected here, fired after the
            // snapshot notify below) so each gets its exact C mask.
            let mut alarm_posts: Vec<(&'static str, EventMask)> = Vec::new();
            if sevr_changed {
                alarm_posts.push(("SEVR", EventMask::VALUE));
            }
            if !stat_mask.is_empty() {
                alarm_posts.push(("STAT", stat_mask));
                alarm_posts.push(("AMSG", stat_mask));
            }
            // C parity (recGbl.c:216): ACKS is posted (DBE_VALUE) only
            // when `stat_mask != 0` AND recGblResetAlarms raised it.
            if alarm_result.acks_changed && !stat_mask.is_empty() {
                alarm_posts.push(("ACKS", EventMask::VALUE));
            }
            if !event_mask.is_empty() {
                changed_fields.push((
                    "UDF".to_string(),
                    EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
                ));
            }
            let snapshot = crate::server::record::ProcessSnapshot {
                changed_fields,
                event_mask,
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

            // Put-notify completion is NOT fired here. Firing before the
            // OUT/FLNK/process-action tail (below) would report the
            // WRITE_NOTIFY done while the chain it triggers — including
            // an async FLNK target — is still running (C `dbNotify.c`
            // keeps the originating record in the waitList until the
            // chain settles). The originating record instead `leave`s
            // the wait-set at the END of this function, after every PP
            // target it drives has joined. See `complete_put_notify`
            // at the tail.

            (snapshot, out_info, flnk_name, process_actions, alarm_posts)
        };

        // 3. Notify subscribers (outside lock)
        {
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
            // Post the alarm fields (SEVR/STAT/AMSG/ACKS) with their
            // individual C masks — see recGblResetAlarms above.
            for &(field, mask) in &alarm_posts {
                instance.notify_field(field, mask);
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
        let (link_writes, deferred_actions): (Vec<_>, Vec<_>) = process_actions
            .into_iter()
            .partition(|a| matches!(a, crate::server::record::ProcessAction::WriteDbLink { .. }));
        self.execute_process_actions(name, &rec, link_writes, visited, depth)
            .await;

        // 4.5 - 7. Multi-output / event / generic-multi-out / FLNK /
        // CP / RPRO tail. Shared with the simulation-mode path so a
        // simulated record runs the exact same `recGblFwdLink`
        // equivalent (C `aiRecord.c:168`).
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
        // 4.5. Multi-output dispatch (fanout/dfanout/seq)
        self.dispatch_multi_output(rec, visited, depth).await;

        // 4.55. event record: post the named software event.
        self.dispatch_event_record(rec).await;

        // 4.6. Generic multi-output links (transform OUTA..OUTP -> A..P,
        // scalcout OUT->OVAL, epid OUTL).
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
                let links =
                    if super::links::multi_output_dispatch_owned(instance.record.record_type()) {
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
    async fn execute_read_db_links(
        &self,
        _record_name: &str,
        rec: &Arc<crate::runtime::sync::RwLock<RecordInstance>>,
        actions: &[crate::server::record::ProcessAction],
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        use crate::server::record::ProcessAction;
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
                }
            }
        }
    }

    /// Execute ProcessActions returned by a record's process() call.
    ///
    /// Actions are executed in order:
    /// - ReadDbLink: reads a linked PV value and writes it into a record field
    ///   (bypasses read-only checks via put_field_internal)
    /// - WriteDbLink: writes a value to a linked PV
    /// - ReprocessAfter: schedules a delayed re-process via tokio::spawn
    async fn execute_process_actions(
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
                    // (dbLink.c:434-448).
                    let parsed =
                        crate::server::record::parse_link_v2(link_str.as_str_lossy().as_ref());
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
                    // Use generation counter for timer cancellation.
                    // Bump generation now; the spawned task only fires if
                    // the generation hasn't been bumped again (i.e., no newer
                    // timer replaced this one).
                    let (gen_counter, gen_val) = {
                        let instance = rec.read().await;
                        let val = instance
                            .reprocess_generation
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        (instance.reprocess_generation.clone(), val)
                    };
                    let db = self.clone();
                    let rec_name = record_name.to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        // Only fire if no newer timer has been scheduled
                        let current = gen_counter.load(std::sync::atomic::Ordering::Relaxed);
                        if current == gen_val {
                            let mut visited = HashSet::new();
                            // Owner-driven continuation: bypass the PACT
                            // entry guard so the timer fire reaches the
                            // record's process() (which advances the
                            // async state machine — e.g. scaler DLY
                            // expiry, calc AFTC). Mirrors C
                            // `callbackRequestDelayed` dispatching to
                            // `(*prset->process)(prec)` directly.
                            let _ = db
                                .process_record_continuation(&rec_name, &mut visited, 0)
                                .await;
                        }
                    });
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
            let mut event_mask = EventMask::NONE;
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
            if include_val {
                event_mask |= EventMask::VALUE;
            }
            if include_archive {
                event_mask |= EventMask::LOG;
            }
            if alarm_result.alarm_changed || alarm_result.amsg_changed {
                // C `recGbl.c:194/203` — same parity rule as the main
                // process path above (see comment there): amsg-only OR
                // sevr/stat change → DBE_ALARM.
                event_mask |= EventMask::ALARM;
            }

            let mut changed_fields = Vec::new();
            if include_val {
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
            }
            // C `recGblResetAlarms` (recGbl.c:201-220) posts each alarm
            // field with its OWN per-field mask, not the record-wide
            // `event_mask`. Mirror the synchronous link path
            // (`process_record_with_links_inner`) and `process_local`
            // exactly: SEVR=DBE_VALUE on a sevr change; STAT/AMSG share
            // `stat_mask` which carries DBE_ALARM when sevr OR amsg
            // moved and DBE_VALUE on a stat change; ACKS=DBE_VALUE only
            // when an alarm field moved AND recGblResetAlarms raised it.
            // Collapsing these into `changed_fields` would post them all
            // on one record-wide mask — losing C's per-field
            // granularity for `.SEVR`/`.STAT`-only subscribers.
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
                // C `val_mask = DBE_ALARM` — the value field carries
                // DBE_ALARM whenever any alarm field moved.
                event_mask |= EventMask::ALARM;
            }
            // C parity (recGbl.c:216): ACKS is posted (DBE_VALUE) only
            // when an alarm field moved AND recGblResetAlarms raised it.
            if alarm_result.acks_changed && !stat_mask.is_empty() {
                alarm_posts.push(("ACKS", EventMask::VALUE));
            }
            if !event_mask.is_empty() {
                changed_fields.push((
                    "UDF".to_string(),
                    EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
                ));
            }
            // Add subscribed non-{VAL,SEVR,STAT,AMSG,UDF} fields that
            // actually changed since last notification — mirrors the
            // main-path snapshot gate (process_record_with_links_inner
            // L794-820). Without this, every async-completion cycle
            // re-sends every subscribed auxiliary field even when its
            // value is unchanged, multiplying the monitor traffic for
            // any record that pairs an async write with a sticky
            // metadata field.
            let mut sub_updates: Vec<(String, EpicsValue)> = Vec::new();
            for (field, subs) in &instance.subscribers {
                if !subs.is_empty()
                    && field != "VAL"
                    && field != "SEVR"
                    && field != "STAT"
                    && field != "AMSG"
                    && field != "UDF"
                {
                    if let Some(val) = instance.resolve_field(field) {
                        let changed = match instance.last_posted.get(field) {
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
                    instance.last_posted.insert(field.clone(), val.clone());
                }
                changed_fields.extend(sub_updates);
                event_mask |= crate::server::recgbl::EventMask::VALUE;
            }
            let snapshot = crate::server::record::ProcessSnapshot {
                changed_fields,
                event_mask,
            };

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
                    let out_val = instance
                        .record
                        .get_field("OVAL")
                        .or_else(|| instance.record.val());
                    out_val.map(|v| (instance.parsed_out.clone(), v))
                } else {
                    None
                }
            } else if is_soft_out {
                if instance.parsed_out.is_writable_out_link() {
                    let out_val = instance
                        .record
                        .get_field("OVAL")
                        .or_else(|| instance.record.val());
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
            let instance = rec.read().await;
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

        // Multi-output dispatch (fanout/dfanout/seq/sseq)
        self.dispatch_multi_output(&rec, visited, depth).await;

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
                let links =
                    if super::links::multi_output_dispatch_owned(instance.record.record_type()) {
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
    /// on a remote change (Regression R0604-CALINK-CP-NO-PROCESS-1).
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
                    .write_external_pv(name, value, crate::server::database::LinkPutOp::Plain)
                    .await
                {
                    eprintln!("SIOL simulation write to external PV '{name}' failed: {e}");
                }
            }
            _ => {}
        }
    }

    /// Check simulation mode for a record. Returns
    /// `SimOutcome::Simulated` when simulation handled the value (the
    /// caller must still run the forward-link tail), or
    /// `SimOutcome::NotSimulated` when normal processing should proceed.
    async fn check_simulation_mode(&self, rec: &Arc<RwLock<RecordInstance>>) -> SimOutcome {
        // Read SIML, SIMM, SIOL, SIMS from the record
        let (siml_link, siol_link, sims, _rtype, is_input) = {
            let instance = rec.read().await;
            let rtype = instance.record.record_type().to_string();
            // Every input record whose DBD declares SIML/SIOL/SIMM/SIMS.
            // `mbbi`/`mbbiDirect` are input records: `mbbiRecord.c:125-126`
            // (and mbbiDirectRecord.c) declare SIML+SIOL, and
            // `mbbiRecord.c:388-394` reads `dbGetLink(&prec->siol,
            // DBR_ULONG, &prec->sval)` then `rval = sval` — input
            // semantics. Omitting them sent a simulated mbbi down the
            // OUTPUT branch, which writes VAL out to SIOL instead of
            // reading the value in from it.
            let is_input = matches!(
                rtype.as_str(),
                "ai" | "bi"
                    | "mbbi"
                    | "mbbiDirect"
                    | "longin"
                    | "int64in"
                    | "stringin"
                    | "lsi"
                    | "event"
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

            if siml.is_empty() && siol.is_empty() {
                return SimOutcome::NotSimulated; // No simulation configured
            }

            let siml_parsed = crate::server::record::parse_link_v2(siml.as_str_lossy().as_ref());
            let siol_parsed = crate::server::record::parse_link_v2(siol.as_str_lossy().as_ref());

            (siml_parsed, siol_parsed, sims, rtype, is_input)
        };

        // Read SIML -> update SIMM. C `dbGetLink(&prec->siml, DBR_USHORT,
        // &prec->simm, 0, 0)` reads the SIML link for any type; the
        // pre-fix port only read a `ParsedLink::Db` SIML, ignoring a
        // CA/PVA/constant simulation-mode source.
        if let Some(val) = self.read_link_value_no_process(&siml_link).await {
            let simm_val = val.to_f64().unwrap_or(0.0) as i16;
            let mut instance = rec.write().await;
            let _ = instance
                .record
                .put_field("SIMM", EpicsValue::Short(simm_val));
        }

        // Check SIMM
        let simm = {
            let instance = rec.read().await;
            instance
                .record
                .get_field("SIMM")
                .and_then(|v| {
                    if let EpicsValue::Short(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        };

        if simm == 0 {
            return SimOutcome::NotSimulated; // NO simulation, proceed normally
        }

        // epics-base 7.0.7 (SIMM menu):
        //   1 = YES — read/write via SIOL using the cooked VAL
        //   2 = RAW — read/write via SIOL using the raw RVAL when the
        //             record carries one (ai/ao only); falls back to
        //             VAL when no RVAL is present. Mirrors the C
        //             implementation, which treats records lacking
        //             a raw value as "YES" since there's nothing
        //             else to copy.
        let raw_mode = simm == 2;
        let raw_field = if raw_mode { "RVAL" } else { "VAL" };

        // SIMM=YES(1) / SIMM=RAW(2): read/write the SIOL link. C
        // `readValue`/`writeValue` for a SIMM-mode record go through
        // `dbGetLink`/`dbPutLink`, which dispatch by link type — a local
        // DB target, a CA target (a bare non-local name or an explicit
        // `CA`/`ca://` link), or a constant. The pre-fix port special-
        // cased a local `ParsedLink::Db` SIOL only, so a non-local or
        // external SIOL neither read nor wrote yet still returned
        // `Simulated` — the record froze with no value and no alarm.
        // Dispatch uniformly through the same link read/write owners as
        // every other link; the alarm/timestamp/notify tail below now
        // runs for every SIOL link type.
        {
            if is_input {
                // Input record: read from SIOL -> set VAL/RVAL. Uniform
                // across Db (with locality fallback) / Ca / Pva / constant
                // via `read_link_value_no_process` (C `dbGetLink`).
                if let Some(siol_val) = self.read_link_value_no_process(&siol_link).await {
                    let mut instance = rec.write().await;
                    let target_supports_raw =
                        raw_mode && instance.record.get_field("RVAL").is_some();
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
                    apply_timestamp(&mut instance.common, true);
                    instance.common.udf = false;

                    // Simulation alarm + alarm tail. C `aiRecord.c`
                    // (and every soft-input record): `readValue()` raises
                    // `recGblSetSevr(prec, SIMM_ALARM, prec->sims)` —
                    // MAXIMIZE into the pending nsta/nsev, not a direct
                    // commit — and then `process()` ALWAYS runs
                    // `checkAlarms` + `recGblResetAlarms` even for a
                    // simulated value, so the sim VAL still trips its own
                    // limit/state alarms and the SIMM severity maximizes
                    // against them. Set SIMM_ALARM first so it wins
                    // severity ties (C order: readValue before checkAlarms).
                    let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
                    crate::server::recgbl::rec_gbl_set_sevr(
                        &mut instance.common,
                        crate::server::recgbl::alarm_status::SIMM_ALARM,
                        sev,
                    );
                    {
                        let inst = &mut *instance;
                        inst.record.check_alarms(&mut inst.common);
                    }
                    instance.evaluate_alarms();
                    let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

                    // Build snapshot and notify
                    let mut changed_fields = Vec::new();
                    if let Some(val) = instance.record.val() {
                        changed_fields.push(("VAL".to_string(), val));
                    }
                    changed_fields.push((
                        "SEVR".to_string(),
                        EpicsValue::Short(instance.common.sevr as i16),
                    ));
                    changed_fields.push((
                        "STAT".to_string(),
                        EpicsValue::Short(instance.common.stat as i16),
                    ));
                    let snapshot = crate::server::record::ProcessSnapshot {
                        changed_fields,
                        event_mask: crate::server::recgbl::EventMask::VALUE
                            | crate::server::recgbl::EventMask::ALARM,
                    };
                    instance.notify_from_snapshot(&snapshot);
                }
            } else {
                // Output record: write VAL (or RVAL for SIMM=RAW) to
                // SIOL (skip device write).
                let out_val = {
                    let instance = rec.read().await;
                    if raw_mode {
                        // RAW path: prefer RVAL when the record has
                        // one. Otherwise fall through to VAL.
                        instance
                            .record
                            .get_field(raw_field)
                            .or_else(|| instance.record.val())
                    } else {
                        instance.record.val()
                    }
                };
                if let Some(val) = out_val {
                    // Write VAL to the SIOL target, dispatching by link
                    // type/locality (C `dbPutLink`). A local DB target
                    // uses the `_already_locked` write — writing VAL is an
                    // internal step of this record's processing chain,
                    // which already holds the entry record's advisory
                    // write gate, so a SIOL that points back at a chain
                    // record cannot dead-lock on a non-reentrant gate
                    // (same reasoning as the OUT-link write in
                    // `write_db_link_value`). A non-local or external
                    // SIOL routes through the lset put path.
                    self.write_sim_siol_value(&siol_link, val).await;
                }

                let mut instance = rec.write().await;
                apply_timestamp(&mut instance.common, true);
                instance.common.udf = false;

                // Simulation alarm + alarm tail (same C parity as the
                // input branch above): maximize SIMM_ALARM into the
                // pending state, then run the record's checkAlarms and
                // recGblResetAlarms so the sim output value trips its own
                // alarms and the SIMM severity maximizes against them.
                let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
                crate::server::recgbl::rec_gbl_set_sevr(
                    &mut instance.common,
                    crate::server::recgbl::alarm_status::SIMM_ALARM,
                    sev,
                );
                {
                    let inst = &mut *instance;
                    inst.record.check_alarms(&mut inst.common);
                }
                instance.evaluate_alarms();
                let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

                // Notify subscribers of simulation output
                let mut changed_fields = Vec::new();
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
                changed_fields.push((
                    "SEVR".to_string(),
                    EpicsValue::Short(instance.common.sevr as i16),
                ));
                changed_fields.push((
                    "STAT".to_string(),
                    EpicsValue::Short(instance.common.stat as i16),
                ));
                let snapshot = crate::server::record::ProcessSnapshot {
                    changed_fields,
                    event_mask: crate::server::recgbl::EventMask::VALUE
                        | crate::server::recgbl::EventMask::ALARM,
                };
                instance.notify_from_snapshot(&snapshot);
            }
        }

        SimOutcome::Simulated
    }
}
