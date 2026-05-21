use std::collections::HashSet;
use std::sync::Arc;

use crate::runtime::sync::RwLock;
use crate::server::record::{AlarmSeverity, RecordInstance, ScanType};
use crate::types::EpicsValue;

use super::{PvDatabase, SelmKind, SelmResult, select_link_indices_ex};

/// Alarm state from a link source, used for MS/NMS propagation.
///
/// `amsg` is the alarm-message string — propagated from the source
/// record's `common.amsg` so a downstream MS link sees the same
/// human-readable explanation. Empty when the source has no message
/// or when the link source is not a DB record.
#[derive(Clone, Debug)]
pub(crate) struct LinkAlarm {
    pub stat: u16,
    pub sevr: AlarmSeverity,
    pub amsg: String,
}

/// One `seq` link group — C `linkGrp { dly, dol, dov, lnk }`.
#[derive(Clone, Debug)]
pub(crate) struct SeqGroup {
    /// DOLn input link string (empty when unset).
    pub dol: String,
    /// LNKn output link string (empty when unset).
    pub lnk: String,
    /// DLYn per-group delay in seconds.
    pub dly: f64,
    /// DOn value-storage field (`linkGrp.dov`) — used when DOLn is
    /// an empty/constant link.
    pub dov: f64,
}

/// One `sseq` link group — DOL / LNK plus the numeric `DO` and
/// string `STR` value-storage fields.
#[derive(Clone, Debug)]
pub(crate) struct SseqGroup {
    pub dol: String,
    pub lnk: String,
    pub do_val: f64,
    pub str_val: String,
    /// DLYn per-step delay in seconds.
    pub dly: f64,
}

/// Typed multi-output payload — replaces the legacy `\0`-packed
/// `Vec<String>` so a link string containing an embedded NUL can
/// never mis-split (parity review 04-L3).
pub(crate) enum MultiOut {
    /// fanout — 16 forward-link strings (LNK0..LNKF).
    Fanout(Vec<String>),
    /// dfanout — 16 output-link strings (OUTA..OUTP).
    Dfanout(Vec<String>),
    /// seq — 16 link groups (0..F).
    Seq(Vec<SeqGroup>),
    /// sseq — link groups with DO/STR value storage.
    Sseq(Vec<SseqGroup>),
}

impl MultiOut {
    /// Number of link slots — the `count` passed to the SELM selector.
    fn len(&self) -> usize {
        match self {
            MultiOut::Fanout(v) => v.len(),
            MultiOut::Dfanout(v) => v.len(),
            MultiOut::Seq(v) => v.len(),
            MultiOut::Sseq(v) => v.len(),
        }
    }
}

/// Record types whose multi-output link groups are dispatched by
/// [`PvDatabase::dispatch_multi_output`].
///
/// SINGLE-OWNER INVARIANT — each of these record types' output links
/// (fanout `LNKn`, dfanout `OUTn`, seq/sseq `LNKn`) is dispatched
/// (value written + target forward-link processed) **exactly once per
/// process cycle, by `dispatch_multi_output` and by nothing else**.
///
/// `dispatch_multi_output` is the sole owner because it is the only
/// path that performs the full C-record model: SELL→SELN resolution,
/// SELM/OFFS/SHFT selection, per-group DOLn input fetch, sseq
/// STR/DO value precedence, and per-group DLYn delay.
///
/// MUST NOT: the generic `multi_output_links` block in
/// `processing.rs` (run unconditionally for every record after
/// `dispatch_multi_output`) must skip any record type listed here.
/// Without that gate, an `sseq` record — which also implemented the
/// `Record::multi_output_links` trait method — was dispatched twice
/// per cycle, writing every selected `LNKn` value to its target a
/// second time. `multi_output_dispatch_owned` is consulted by that
/// block (see `run_forward_link_tail_with_putf` §4.6) so the
/// double-dispatch is structurally impossible, not merely removed at
/// one call site.
pub(crate) fn multi_output_dispatch_owned(record_type: &str) -> bool {
    matches!(record_type, "fanout" | "dfanout" | "seq" | "sseq")
}

/// True when a link string carries an explicit `PP` (or `CP`/`CPP`)
/// process modifier as a whitespace-separated token.
///
/// C `dbStaticLib.c` sets the link's process-passive flag (`ln`)
/// only when the modifier string contains `"PP"`. For a `DBF_OUTLINK`
/// the absence of `PP` means NPP — `dbDbPutLink` writes the value but
/// does not process the target. `parse_link_v2` wrongly defaults a
/// bare link to `ProcessPassive`, so the dfanout dispatch consults
/// this helper to recover the C-correct NPP-by-default for OUT links.
fn link_has_explicit_pp(raw: &str) -> bool {
    raw.split_whitespace()
        .any(|tok| tok == "PP" || tok == "CP" || tok == "CPP")
}

impl PvDatabase {
    /// Read a value from a parsed link (DB, Constant, or external Ca/Pva).
    ///
    /// `visited` / `depth` are the caller's processing-chain state so a
    /// PP source is processed within the same chain — see
    /// [`Self::process_passive_db_source`] for why a fresh set / depth 0
    /// would defeat the cycle guard.
    pub(crate) async fn read_link_value(
        &self,
        link: &crate::server::record::ParsedLink,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::None => None,
            crate::server::record::ParsedLink::Ca(ca) => self.resolve_external_pv(&ca.pv).await,
            crate::server::record::ParsedLink::Pva(name) => self.resolve_external_pv(name).await,
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) => {
                // PP: process source record if Passive before reading.
                // Threads the caller's `visited`/`depth` so an A↔B PP
                // cycle terminates at the existing cycle guard instead
                // of recursing with a fresh set.
                self.process_passive_db_source(db, visited, depth).await;
                let pv_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                self.get_pv(&pv_name).await.ok()
            }
            // Hardware links are dispatched by device support directly
            // — there's no canonical "value" available from a generic
            // read; return None so the framework treats the link as
            // unresolvable for value-read purposes.
            crate::server::record::ParsedLink::Hw(_) => None,
            // lnkCalc: fetch each input PV, evaluate the expr,
            // return the result. Timestamp passthrough is handled by
            // `read_calc_link_with_time` for callers that need it.
            crate::server::record::ParsedLink::Calc(calc) => self.evaluate_calc_link(calc).await,
        }
    }

    /// lnkCalc evaluation: fetch each input PV, bind to calc engine
    /// vars A..L, run `expr`, return the result as `EpicsValue::Double`.
    /// Returns `None` if any input fetch fails, expr compile fails, or
    /// eval fails — the caller treats the link as unresolvable.
    pub async fn evaluate_calc_link(
        &self,
        calc: &crate::server::record::CalcLink,
    ) -> Option<EpicsValue> {
        use crate::calc::engine::{CALC_NARGS, NumericInputs};
        // lnkCalc binds inputs to calc engine vars A..L (12). A link
        // string carrying more than `CALC_NARGS` inputs is malformed —
        // reject it rather than silently dropping the overflow args
        // (the pre-fix `.take(12)` masked the misconfiguration).
        if calc.args.len() > CALC_NARGS {
            return None;
        }
        let mut vars = [0.0f64; CALC_NARGS];
        for (i, arg) in calc.args.iter().enumerate() {
            let v = self.get_pv(arg).await.ok()?;
            vars[i] = v.to_f64()?;
        }
        let compiled = crate::calc::compile(&calc.expr).ok()?;
        let mut inputs = NumericInputs::with_vars(vars);
        let result = crate::calc::eval(&compiled, &mut inputs).ok()?;
        Some(EpicsValue::Double(result))
    }

    /// lnkCalc evaluation that also returns the timestamp pulled from
    /// the input named by `time_source` (e.g. `'A'` → first input).
    /// Returns `(value, Some(time))` when `time_source` is set and
    /// the referenced input record has a timestamp, `(value, None)`
    /// otherwise. The caller (link read path) uses `None` to mean
    /// "consumer keeps its own apply_timestamp time".
    pub async fn evaluate_calc_link_with_time(
        &self,
        calc: &crate::server::record::CalcLink,
    ) -> Option<(EpicsValue, Option<std::time::SystemTime>)> {
        let value = self.evaluate_calc_link(calc).await?;
        let time = match calc.time_source {
            Some(letter) => {
                let idx = (letter as u8).saturating_sub(b'A') as usize;
                let src = calc.args.get(idx)?;
                // Strip `.FIELD` suffix to land on the record name.
                let record_name = src.rsplit_once('.').map(|(r, _)| r).unwrap_or(src);
                let rec = self.get_record(record_name).await?;
                let inst = rec.read().await;
                Some(inst.common.time)
            }
            None => None,
        };
        Some((value, time))
    }

    /// Read value + alarm from a DB link. Returns (value, alarm) for MS/NMS propagation.
    pub(crate) async fn read_link_with_alarm(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> (Option<EpicsValue>, Option<LinkAlarm>) {
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                let pv_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                let value = self.get_pv(&pv_name).await.ok();
                // Read source record's alarm state — alias-aware
                // (epics-base PR #336) so a link target spelled with
                // an alias still propagates MS/NMS alarm correctly.
                let alarm = if let Some(rec) = self.get_record(&db.record).await {
                    let inst = rec.read().await;
                    Some(LinkAlarm {
                        stat: inst.common.stat,
                        sevr: inst.common.sevr,
                        amsg: inst.common.amsg.clone(),
                    })
                } else {
                    None
                };
                (value, alarm)
            }
            crate::server::record::ParsedLink::Constant(_) => (link.constant_value(), None),
            // External Pva/Ca link: the value comes from the lset's
            // cached snapshot, the alarm from the lset's accessors.
            //
            // PVA: the `?sevr=` modifier is stripped before epics-base-rs
            // parses the link, so the lset retains and applies the
            // `MS`/`NMS`/`MSI` gate itself — a returned `Some(sev)` is
            // already gated and the caller folds it as `MaximizeStatus`.
            //
            // CA (BRIDGE-FR-3): the `MS`/`NMS`/`MSI`/`MSS` modifier is
            // now carried in the `CaLink`, so the resolver returns the
            // *raw* remote alarm and record processing applies the gate
            // using `link.monitor_switch()`. Either way this fn just
            // reads the raw/gated alarm; the switch pairing happens in
            // `processing.rs`. Without this, a connected external link
            // carrying a remote MINOR/MAJOR severity never folded into
            // the owning record's LINK_ALARM (B2).
            crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::Ca(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva link carries a PV name");
                let value = self.resolve_external_pv(name).await;
                let alarm = self.external_link_alarm(name).await;
                (value, alarm)
            }
            _ => (None, None),
        }
    }

    /// BR-R19: latched upstream timestamp from the lset, when the
    /// link is configured with `time=true`. The lset gates internally
    /// (returning `None` for links without the `time` option), so a
    /// `Some` here is the authoritative remote timestamp the
    /// processing path should adopt into the owning record's
    /// `common.time`. Mirrors pvxs `pvalink_lset.cpp:427`.
    ///
    /// Returns `(seconds_since_epoch, nanoseconds)` exactly as the
    /// lset reports them; the caller folds them into the record's
    /// `SystemTime` via `UNIX_EPOCH + Duration::new(...)`.
    pub(crate) async fn external_link_time(&self, name: &str) -> Option<(i64, i32)> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset.
            let registry = self.inner.link_sets.read().await;
            for s in registry.schemes() {
                if let Some(lset) = registry.get(&s) {
                    if let Some(ts) = lset.time_stamp(name) {
                        return Some(ts);
                    }
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        lset.time_stamp(body)
    }

    /// Build a [`LinkAlarm`] from the registered lset's alarm
    /// accessors for an external (`pva://` / `ca://`) link, or `None`
    /// when no lset is registered or the lset reports no alarm.
    ///
    /// The lset's `alarm_severity` is the gated severity (see
    /// [`crate::server::database::LinkSet::alarm_severity`]); when it
    /// is `Some`, the `stat` is `LINK_ALARM` and the message comes
    /// from `alarm_message`.
    async fn external_link_alarm(&self, name: &str) -> Option<LinkAlarm> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset until one reports
            // a severity (mirrors `resolve_external_pv`'s bare path).
            let registry = self.inner.link_sets.read().await;
            for s in registry.schemes() {
                if let Some(lset) = registry.get(&s) {
                    if let Some(sev) = lset.alarm_severity(name) {
                        return Some(LinkAlarm {
                            // BRIDGE-FR-3: prefer the remote STAT for MSS;
                            // fall back to LINK_ALARM when the lset has none.
                            stat: lset
                                .alarm_status(name)
                                .map(|s| s as u16)
                                .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
                            sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
                            amsg: lset.alarm_message(name).unwrap_or_default(),
                        });
                    }
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        let sev = lset.alarm_severity(body)?;
        Some(LinkAlarm {
            // BRIDGE-FR-3: remote STAT for MSS, else LINK_ALARM.
            stat: lset
                .alarm_status(body)
                .map(|s| s as u16)
                .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
            sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
            amsg: lset.alarm_message(body).unwrap_or_default(),
        })
    }

    /// Remote display / control / valueAlarm metadata for an external
    /// (`pva://` / `ca://`) link, resolved through the registered
    /// lset's [`LinkSet::link_metadata`] hook (BR-R24).
    ///
    /// This is the DB-link-API entry point that exposes the linked PV
    /// metadata pvxs's pvalink lset surfaces through its
    /// `pvaGetDBFtype` / `pvaGetElements` / `pvaGetControlLimits` /
    /// `pvaGetGraphicLimits` / `pvaGetAlarmLimits` / `pvaGetPrecision`
    /// / `pvaGetUnits` getters
    /// (`pvxs/ioc/pvalink_lset.cpp:700`). Scheme dispatch mirrors
    /// [`Self::external_link_alarm`]: an explicit `pva://` / `ca://`
    /// prefix selects the lset directly, a bare name tries every
    /// registered lset until one reports metadata.
    ///
    /// `None` when no lset is registered for the scheme or the lset
    /// has no cached value for the link (not yet connected).
    pub async fn external_link_metadata(
        &self,
        name: &str,
    ) -> Option<crate::server::database::LinkMetadata> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            let registry = self.inner.link_sets.read().await;
            for s in registry.schemes() {
                if let Some(lset) = registry.get(&s) {
                    if let Some(meta) = lset.link_metadata(name) {
                        return Some(meta);
                    }
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        lset.link_metadata(body)
    }

    /// C `dbGetLink` PP rule: if a DB input link is `ProcessPassive`
    /// and its source record is `Passive`-scanned, process the source
    /// record before its value is read so the reader sees a freshly
    /// computed value. No-op for non-PP links or non-passive sources.
    ///
    /// Shared by `read_link_value_soft` (single-INP path) and the
    /// multi-input fetch loop (`INPA..INPL` for calc/sel/sub/aSub) so
    /// both paths get the identical C-correct PP-processing behavior.
    ///
    /// The caller's `visited` set and `depth` are threaded through into
    /// the source's processing cycle — NOT a fresh set / depth 0. This
    /// is required for the cycle guard to span the PP hop: in C,
    /// `calcRecord.c::process` sets `prec->pact = TRUE` *before*
    /// `fetch_values()` (calcRecord.c:119-120), so when a PP input link
    /// re-enters `dbProcess` on a record already mid-fetch, the
    /// `if (precord->pact) goto all_done;` guard (dbAccess.c:537-557)
    /// terminates the cycle after one bounce. The Rust port sets its
    /// PACT `AtomicBool` only on `AsyncPending` *after* `record.process()`
    /// returns, so it cannot catch a record mid-link-fetch. Threading
    /// the caller's `visited` set makes the existing `visited.insert`
    /// cycle guard (`process_record_with_links_inner`, processing.rs)
    /// fire instead — an A↔B `PP` cycle bails when the second hop tries
    /// to re-insert a name already on the chain. The FLNK path threads
    /// `visited`/`depth` the same way (processing.rs FLNK dispatch).
    pub(crate) async fn process_passive_db_source(
        &self,
        db: &crate::server::record::DbLink,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        if db.policy != crate::server::record::LinkProcessPolicy::ProcessPassive {
            return;
        }
        if let Some(src) = self.get_record(&db.record).await {
            let is_passive =
                src.read().await.common.scan == crate::server::record::ScanType::Passive;
            if is_passive {
                // MR-R5: recursive INP-link source processing within
                // one chain — gate held by the foreign entry record.
                let _ = self
                    .process_record_with_links_recursive(&db.record, visited, depth + 1)
                    .await;
            }
        }
    }

    /// Read a value from a parsed link for INP (only reads DB links when soft channel).
    ///
    /// `visited` / `depth` are the caller's processing-chain state — a PP
    /// input link's source is processed *within* that same chain so the
    /// `visited` cycle guard spans the PP hop (see
    /// [`Self::process_passive_db_source`]).
    pub async fn read_link_value_soft(
        &self,
        link: &crate::server::record::ParsedLink,
        is_soft: bool,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) if is_soft => {
                // PP: process source record if Passive before reading
                self.process_passive_db_source(db, visited, depth).await;
                let pv_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                self.get_pv(&pv_name).await.ok()
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
                if is_soft =>
            {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva link carries a PV name");
                self.resolve_external_pv(name).await
            }
            // lnkCalc evaluates regardless of `is_soft` — the input
            // PVs may themselves be local DB targets (which need the
            // soft path) or remote CA/PVA, but the calc evaluation
            // is uniform either way.
            crate::server::record::ParsedLink::Calc(calc) => self.evaluate_calc_link(calc).await,
            _ => None,
        }
    }

    /// Write a value through a DbLink, optionally processing the target if PP and Passive.
    ///
    /// `src_putf` carries the source record's `PUTF` bit so the target inherits
    /// it the same way C `dbDbLink.c::processTarget` propagates it (lines 470-498):
    ///
    /// - target not pact: `target.putf = src_putf` (normal propagation),
    /// - target pact AND `src_putf` AND target not on current process chain:
    ///   `target.rpro = true`, `target.putf = false` so the in-flight cycle
    ///   reprocesses on completion attributing the put to the originator,
    /// - otherwise: no PUTF change (target is either being processed
    ///   recursively by us, or wasn't triggered by a dbPutField).
    ///
    /// Without this, a CA WRITE_NOTIFY landing on an upstream calc/seq/dfanout
    /// that fanned out via DB OUT links would see `target.putf = 0` on every
    /// downstream record — breaking dbNotify completion attribution and any
    /// device-support code that uses PUTF to distinguish operator-driven from
    /// scan-driven processing.
    pub(crate) async fn write_db_link_value(
        &self,
        link: &crate::server::record::DbLink,
        value: EpicsValue,
        src_putf: bool,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        let target_name = if link.field == "VAL" {
            link.record.clone()
        } else {
            format!("{}.{}", link.record, link.field)
        };
        // MR-R5: an OUT-link write-back is an internal step of the
        // processing chain that already holds the entry record's
        // advisory write gate (`dbScanLock` analogue). It must use the
        // `_already_locked` write so it does not re-acquire a gate: a
        // self-referencing OUT link (`SELF PP`) would otherwise
        // dead-lock on the entry record's own non-reentrant gate. C
        // `dbDbPutValue` writes the OUT-link target under the same
        // lock set the chain already owns.
        let _ = self.put_pv_already_locked(&target_name, value).await;

        if link.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
            // Alias-aware lookup: the link's target may be the alias
            // form. `process_record_with_links` itself also resolves
            // aliases at entry, so passing `link.record` raw is safe.
            if let Some(target_rec) = self.get_record(&link.record).await {
                // Apply C `processTarget` PUTF propagation rules before
                // dispatching the target's process cycle.
                let (target_scan, should_process) = {
                    let mut tg = target_rec.write().await;
                    let pact = tg.is_processing();
                    let on_chain = visited.contains(&link.record);
                    if !pact {
                        tg.common.putf = src_putf;
                    } else if src_putf && !on_chain {
                        tg.common.rpro = true;
                        tg.common.putf = false;
                    }
                    (tg.common.scan, !pact)
                };
                if should_process && target_scan == ScanType::Passive {
                    // MR-R5: recursive OUT-link target processing within
                    // one chain — gate held by the foreign entry record.
                    let _ = self
                        .process_record_with_links_recursive(&link.record, visited, depth + 1)
                        .await;
                }
            }
        }
    }

    /// Write a value to an external (`ca://` / `pva://`) OUT link
    /// through the registered [`LinkSet`].
    ///
    /// This is the OUTPUT-side twin of [`Self::resolve_external_pv`]:
    /// the input side dispatches a `ParsedLink::Ca`/`Pva` read through
    /// `lset.get_value`, this dispatches a record's OUT-link write
    /// through `lset.put_value`. Mirrors C `dbLink.c::dbPutLink`
    /// (dbLink.c:434-448), which routes every link write — DB or CA —
    /// through `plink->lset->putValue` and raises a link alarm
    /// (`setLinkAlarm`) on failure.
    ///
    /// `name` may be a fully scheme-prefixed string (`pva://X`,
    /// `ca://X`) or the bare body (the form stored in
    /// `ParsedLink::Ca`/`Pva` after `record/link.rs` strips the
    /// scheme). For a bare name every registered lset is tried in
    /// turn — the first whose `put_value` succeeds wins.
    ///
    /// Returns `Ok(())` on a successful remote write, `Err(reason)`
    /// when no lset is registered for the scheme or the lset rejects
    /// the write (the caller folds that into a LINK alarm — it must
    /// never panic).
    pub(crate) async fn write_external_pv(
        &self,
        name: &str,
        value: EpicsValue,
    ) -> Result<(), String> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset in turn, first
            // accepting write wins (mirrors `resolve_external_pv`'s
            // bare-name path).
            let registry = self.inner.link_sets.read().await;
            let schemes = registry.schemes();
            if schemes.is_empty() {
                return Err(format!("no link set registered for external link '{name}'"));
            }
            let mut last_err = String::new();
            for s in schemes {
                if let Some(lset) = registry.get(&s) {
                    match lset.put_value(name, value.clone()) {
                        Ok(()) => return Ok(()),
                        Err(e) => last_err = e,
                    }
                }
            }
            return Err(last_err);
        };
        let lset = self
            .inner
            .link_sets
            .read()
            .await
            .get(scheme)
            .ok_or_else(|| format!("no '{scheme}' link set registered for '{name}'"))?;
        lset.put_value(body, value)
    }

    /// Write a value through a parsed OUT link, dispatching DB links
    /// to [`Self::write_db_link_value`] and external (`ca://`/`pva://`)
    /// links to [`Self::write_external_pv`].
    ///
    /// This is the OUTPUT-side counterpart of [`Self::read_link_value`]'s
    /// scheme dispatch: the OUT-link write stage in `processing.rs`
    /// must route a `ParsedLink::Ca`/`Pva` through the link set, not
    /// only handle `ParsedLink::Db`. An external link with no
    /// registered lset fails gracefully — the error is logged and the
    /// record is left to its alarm state, never a panic.
    ///
    /// `Constant`/`Hw`/`Calc`/`None` OUT links are not writable
    /// targets and are silently skipped (C `dbPutLink` returns
    /// `S_db_noLSET` for a link with no lset — the same no-op).
    pub(crate) async fn write_out_link_value(
        &self,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
        src_putf: bool,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                self.write_db_link_value(db, value, src_putf, visited, depth)
                    .await;
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva link carries a PV name");
                if let Err(e) = self.write_external_pv(name, value).await {
                    eprintln!("OUT-link write to external PV '{name}' failed: {e}");
                }
            }
            // Constant / Hw / Calc / None are not writable OUT-link
            // targets — no-op (C `dbPutLink` → `S_db_noLSET`).
            _ => {}
        }
    }

    /// Read a record String field, defaulting to empty.
    fn field_str(instance: &RecordInstance, field: &str) -> String {
        match instance.record.get_field(field) {
            Some(EpicsValue::String(s)) => s,
            _ => String::new(),
        }
    }

    /// Read a record numeric field as `i16`, defaulting to 0.
    fn field_i16(instance: &RecordInstance, field: &str) -> i16 {
        instance
            .record
            .get_field(field)
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0) as i16
    }

    /// Apply a SELM-resolved out-of-range alarm to the record.
    ///
    /// C raises this alarm inside `process()` (before `recGblResetAlarms`)
    /// via `recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM)`. The Rust
    /// multi-output dispatch runs after the record's own alarm reset,
    /// so we apply the severity directly to `common.sevr/stat`, refresh
    /// the live STAT/SEVR fields, and post the monitor — matching the
    /// observable end state (record reads INVALID/SOFT_ALARM, a
    /// `DBE_ALARM` subscriber on STAT/SEVR is notified).
    async fn apply_selm_alarm(
        rec: &Arc<RwLock<RecordInstance>>,
        alarm: Option<(u16, AlarmSeverity)>,
    ) {
        let Some((stat, sevr)) = alarm else {
            return;
        };
        let posted = {
            let mut inst = rec.write().await;
            // Raise-only, mirroring recGblSetSevr.
            if (sevr as u16) > (inst.common.sevr as u16) {
                inst.common.sevr = sevr;
                inst.common.stat = stat;
                true
            } else {
                false
            }
        };
        if posted {
            let inst = rec.read().await;
            inst.notify_field("SEVR", crate::server::recgbl::EventMask::ALARM);
            inst.notify_field("STAT", crate::server::recgbl::EventMask::VALUE);
        }
    }

    /// Multi-output dispatch for fanout, dfanout, seq record types.
    ///
    /// The per-record payload is a typed [`MultiOut`] — seq / sseq
    /// groups are kept as struct fields, NOT `\0`-packed strings
    /// (the pre-fix encoding could mis-split a link string that
    /// happened to contain an embedded NUL).
    pub(crate) async fn dispatch_multi_output(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        // Snapshot the source record's PUTF bit so every write_db_link_value
        // call below can propagate it to its target — C `dbDbLink.c::
        // processTarget` invariant (see write_db_link_value doc).
        let src_putf = rec.read().await.common.putf;

        // Resolve the SELL link into SELN before SELN is read below.
        // C `fanoutRecord.c:103`, `dfanoutRecord.c:126`,
        // `seqRecord.c:152` all call
        // `dbGetLink(&prec->sell, DBR_USHORT, &prec->seln, 0, 0)` at
        // the top of `process()`, every cycle. Only the `sel` record's
        // NVL->SELN binding was previously wired; fanout/dfanout/seq/
        // sseq never read the SELL link, so a SELL pointing at another
        // record's value field never updated SELN — the selection was
        // frozen at whatever SELN was initialised to.
        {
            let sell = {
                let instance = rec.read().await;
                match instance.record.record_type() {
                    "fanout" | "dfanout" | "seq" | "sseq" => {
                        Some(Self::field_str(&instance, "SELL"))
                    }
                    _ => None,
                }
            };
            if let Some(sell) = sell {
                if !sell.is_empty() {
                    if let crate::server::record::ParsedLink::Db(ref link) =
                        crate::server::record::parse_link_v2(&sell)
                    {
                        let pv_name = if link.field == "VAL" {
                            link.record.clone()
                        } else {
                            format!("{}.{}", link.record, link.field)
                        };
                        if let Ok(val) = self.get_pv(&pv_name).await {
                            // DBR_USHORT — clamp to the unsigned 16-bit
                            // range the C `epicsUInt16 seln` field holds,
                            // then store into the record's SELN field.
                            let seln = val.to_f64().unwrap_or(0.0);
                            let seln = seln.clamp(0.0, 65535.0) as u16 as i16;
                            let mut instance = rec.write().await;
                            let _ = instance.record.put_field("SELN", EpicsValue::Short(seln));
                        }
                    }
                }
            }
        }

        let dispatch_info: Option<(SelmResult, MultiOut, Option<EpicsValue>)> = {
            let instance = rec.read().await;
            match instance.record.record_type() {
                "fanout" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_i16(&instance, "SELN");
                    let offs = Self::field_i16(&instance, "OFFS");
                    let shft = Self::field_i16(&instance, "SHFT");
                    // C parity (fanoutRecord.c:39): 16 forward links
                    // LNK0..LNKF. LNK0 is the natural first slot.
                    let links: Vec<String> = [
                        "LNK0", "LNK1", "LNK2", "LNK3", "LNK4", "LNK5", "LNK6", "LNK7", "LNK8",
                        "LNK9", "LNKA", "LNKB", "LNKC", "LNKD", "LNKE", "LNKF",
                    ]
                    .iter()
                    .map(|f| Self::field_str(&instance, f))
                    .collect();
                    // SELM resolution with OFFS/SHFT bias (fanoutRecord.c).
                    let sel = select_link_indices_ex(
                        SelmKind::FanoutSeq,
                        selm,
                        seln,
                        offs,
                        shft,
                        links.len(),
                    );
                    Some((sel, MultiOut::Fanout(links), None))
                }
                "dfanout" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_i16(&instance, "SELN");
                    // IVOA / IVOV — invalid output handling, mirrors
                    // epics-base PR #688. When the record's SEVR is
                    // INVALID, IVOA selects: 0 = continue (use VAL as
                    // before), 1 = don't drive (suppress all OUT*),
                    // 2 = set outputs to IVOV.
                    let raw_val = instance.record.val();
                    let val =
                        if instance.common.sevr == crate::server::record::AlarmSeverity::Invalid {
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
                                1 => None, // suppress drive
                                2 => instance.record.get_field("IVOV").or(raw_val),
                                _ => raw_val, // 0 or unknown — Continue
                            }
                        } else {
                            raw_val
                        };
                    let links: Vec<String> = [
                        "OUTA", "OUTB", "OUTC", "OUTD", "OUTE", "OUTF", "OUTG", "OUTH", "OUTI",
                        "OUTJ", "OUTK", "OUTL", "OUTM", "OUTN", "OUTO", "OUTP",
                    ]
                    .iter()
                    .map(|f| Self::field_str(&instance, f))
                    .collect();
                    // dfanout Specified is 1-based; Mask has no SHFT
                    // (dfanoutRecord.c:307-339).
                    let sel =
                        select_link_indices_ex(SelmKind::Dfanout, selm, seln, 0, 0, links.len());
                    Some((sel, MultiOut::Dfanout(links), val))
                }
                "seq" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_i16(&instance, "SELN");
                    let offs = Self::field_i16(&instance, "OFFS");
                    let shft = Self::field_i16(&instance, "SHFT");
                    // C parity (seqRecord.c:86): 16 link groups 0..F,
                    // each DOLn / DOn (value storage) / DLYn / LNKn.
                    let dol_names = [
                        "DOL0", "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8",
                        "DOL9", "DOLA", "DOLB", "DOLC", "DOLD", "DOLE", "DOLF",
                    ];
                    let lnk_names = [
                        "LNK0", "LNK1", "LNK2", "LNK3", "LNK4", "LNK5", "LNK6", "LNK7", "LNK8",
                        "LNK9", "LNKA", "LNKB", "LNKC", "LNKD", "LNKE", "LNKF",
                    ];
                    let dly_names = [
                        "DLY0", "DLY1", "DLY2", "DLY3", "DLY4", "DLY5", "DLY6", "DLY7", "DLY8",
                        "DLY9", "DLYA", "DLYB", "DLYC", "DLYD", "DLYE", "DLYF",
                    ];
                    let do_names = [
                        "DO0", "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9",
                        "DOA", "DOB", "DOC", "DOD", "DOE", "DOF",
                    ];
                    let groups: Vec<SeqGroup> = (0..16)
                        .map(|i| SeqGroup {
                            dol: Self::field_str(&instance, dol_names[i]),
                            lnk: Self::field_str(&instance, lnk_names[i]),
                            dly: instance
                                .record
                                .get_field(dly_names[i])
                                .and_then(|v| v.to_f64())
                                .unwrap_or(0.0),
                            dov: instance
                                .record
                                .get_field(do_names[i])
                                .and_then(|v| v.to_f64())
                                .unwrap_or(0.0),
                        })
                        .collect();
                    let sel = select_link_indices_ex(
                        SelmKind::FanoutSeq,
                        selm,
                        seln,
                        offs,
                        shft,
                        groups.len(),
                    );
                    Some((sel, MultiOut::Seq(groups), None))
                }
                "sseq" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_i16(&instance, "SELN");
                    // sseq keeps the legacy 10-group 1-based layout —
                    // DOL1..DOLA / LNK1..LNKA with DO/STR value storage.
                    // synApps `sseqRecord.dbd` has NO `OFFS`/`SHFT`
                    // fields: `SELN` is the 1-based step number, so the
                    // SELM=Specified base is `SELN - 1` and SELM=Mask
                    // has no shift — exactly `SelmKind::Dfanout`. Using
                    // `SelmKind::FanoutSeq` (0-based `SELN + OFFS`)
                    // mis-selected every Specified/Mask step by one and
                    // diverged from `SseqRecord::should_execute_step`.
                    let dol_names = [
                        "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8", "DOL9",
                        "DOLA",
                    ];
                    let lnk_names = [
                        "LNK1", "LNK2", "LNK3", "LNK4", "LNK5", "LNK6", "LNK7", "LNK8", "LNK9",
                        "LNKA",
                    ];
                    let do_names = [
                        "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9", "DOA",
                    ];
                    let str_names = [
                        "STR1", "STR2", "STR3", "STR4", "STR5", "STR6", "STR7", "STR8", "STR9",
                        "STRA",
                    ];
                    // synApps `sseqRecord` carries a per-step DLYn
                    // (DLY1..DLYA, 1-based) — C `sseqRecord.c` schedules
                    // each selected step after its delay via
                    // `callbackRequestDelayed`, exactly as the base
                    // `seqRecord` does for its DLY0..DLYF groups.
                    let dly_names = [
                        "DLY1", "DLY2", "DLY3", "DLY4", "DLY5", "DLY6", "DLY7", "DLY8", "DLY9",
                        "DLYA",
                    ];
                    let groups: Vec<SseqGroup> = (0..10)
                        .map(|i| SseqGroup {
                            dol: Self::field_str(&instance, dol_names[i]),
                            lnk: Self::field_str(&instance, lnk_names[i]),
                            do_val: instance
                                .record
                                .get_field(do_names[i])
                                .and_then(|v| v.to_f64())
                                .unwrap_or(0.0),
                            str_val: Self::field_str(&instance, str_names[i]),
                            dly: instance
                                .record
                                .get_field(dly_names[i])
                                .and_then(|v| v.to_f64())
                                .unwrap_or(0.0),
                        })
                        .collect();
                    let sel =
                        select_link_indices_ex(SelmKind::Dfanout, selm, seln, 0, 0, groups.len());
                    Some((sel, MultiOut::Sseq(groups), None))
                }
                _ => None,
            }
        };

        let (sel, payload, val) = match dispatch_info {
            Some(info) => info,
            None => return,
        };
        debug_assert!(sel.indices.iter().all(|&i| i < payload.len()));
        // Single-owner invariant: every record type that produces a
        // `MultiOut` payload here MUST be listed in
        // `multi_output_dispatch_owned` so the generic
        // `multi_output_links` block in `processing.rs` skips it. If
        // this fires, the two lists have diverged and the skipped
        // type would be dispatched twice per cycle.
        debug_assert!(multi_output_dispatch_owned(
            rec.read().await.record.record_type()
        ));

        // C raises SOFT_ALARM/INVALID_ALARM when SELN/OFFS/SHFT resolve
        // out of range (fanoutRecord.c:116, dfanoutRecord.c:317,
        // seqRecord.c:157). Apply it before dispatching the (empty)
        // selection.
        Self::apply_selm_alarm(rec, sel.alarm).await;
        let indices = sel.indices;

        match payload {
            MultiOut::Fanout(links) => {
                for idx in indices {
                    let link_str = &links[idx];
                    if link_str.is_empty() {
                        continue;
                    }
                    let parsed = crate::server::record::parse_link_v2(link_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        // C `fanoutRecord.c:110/121/138` dispatches each
                        // selected LNKn via `dbScanFwdLink` →
                        // `dbDbScanFwdLink` → `dbScanPassive`
                        // (`dbDbLink.c:425-432`), which processes the
                        // target ONLY when its SCAN is Passive
                        // (`if (pto->scan != 0) return 0;`). A LNKn
                        // pointing at a Periodic/Event/I/O-Intr record
                        // must NOT be re-processed by the fanout — that
                        // record runs on its own scan. `dbScanPassive`
                        // then calls `processTarget`, which propagates
                        // PUTF (and sets RPRO on a busy target) exactly
                        // like the explicit FLNK path — so mirror that
                        // gate here instead of the previous
                        // unconditional `process_record_with_links`.
                        if let Some(target_rec) = self.get_record(&db.record).await {
                            let (target_scan, should_process) = {
                                let mut tg = target_rec.write().await;
                                let pact = tg.is_processing();
                                let on_chain = visited.contains(&db.record);
                                if !pact {
                                    tg.common.putf = src_putf;
                                } else if src_putf && !on_chain {
                                    tg.common.rpro = true;
                                    tg.common.putf = false;
                                }
                                (tg.common.scan, !pact)
                            };
                            if should_process && target_scan == ScanType::Passive {
                                // MR-R5: recursive link-target processing
                                // within one chain — gate held by the
                                // foreign entry record.
                                let _ = self
                                    .process_record_with_links_recursive(
                                        &db.record,
                                        visited,
                                        depth + 1,
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
            MultiOut::Dfanout(links) => {
                if let Some(ref val) = val {
                    for idx in indices {
                        let link_str = &links[idx];
                        if link_str.is_empty() {
                            continue;
                        }
                        let parsed = crate::server::record::parse_link_v2(link_str);
                        match parsed {
                            crate::server::record::ParsedLink::Db(ref db) => {
                                // C `dfanoutRecord.c:323` drives each OUTn
                                // via `dbPutLink`, whose `DBF_OUTLINK`
                                // target is processed by `dbDbPutLink`
                                // only when the link carries an explicit
                                // `PP` modifier (`dbDbLink.c:415` —
                                // `pvlMask & ln`). The C default for an
                                // out-link with no modifier is NPP: the
                                // value is written but the target is NOT
                                // processed. `parse_link_v2` defaults a
                                // bare link to `ProcessPassive`, so
                                // without this correction a bare `OUTn`
                                // would re-process the target — and a
                                // Soft-Channel ai target's `convert()`
                                // would then clobber the value just
                                // written. Honour C: process the dfanout
                                // OUTn target only on an explicit `PP`
                                // token.
                                let explicit_pp = link_has_explicit_pp(link_str);
                                let mut db = db.clone();
                                if !explicit_pp
                                    && db.policy
                                        == crate::server::record::LinkProcessPolicy::ProcessPassive
                                {
                                    db.policy = crate::server::record::LinkProcessPolicy::NoProcess;
                                }
                                self.write_db_link_value(
                                    &db,
                                    val.clone(),
                                    src_putf,
                                    visited,
                                    depth,
                                )
                                .await;
                            }
                            // External `ca://`/`pva://` OUTn — C
                            // `dbPutLink` routes a CA-link write through
                            // the link set's `putValue` identically to a
                            // DB link (dbLink.c:434-448). PP has no
                            // meaning for an external write (the remote
                            // record processes on its own IOC), so route
                            // straight through the link set.
                            crate::server::record::ParsedLink::Ca(_)
                            | crate::server::record::ParsedLink::Pva(_) => {
                                let name = parsed
                                    .external_pv_name()
                                    .expect("Ca/Pva link carries a PV name");
                                if let Err(e) = self.write_external_pv(name, val.clone()).await {
                                    eprintln!(
                                        "dfanout OUT-link write to external PV '{name}' failed: {e}"
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            MultiOut::Seq(groups) => {
                for idx in indices {
                    let grp = &groups[idx];
                    if grp.lnk.is_empty() {
                        continue;
                    }
                    // Per-group DLYn staggering — C `seqRecord.c`
                    // schedules each group after its delay. Groups
                    // process sequentially in index order, each after
                    // its own delay (callbackRequestDelayed chain).
                    if grp.dly > 0.0 {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(grp.dly)).await;
                    }
                    // Value: read from DOLn link, else the stored DOn
                    // value (linkGrp.dov) — C uses DOn as the value
                    // when DOLn is a constant/empty link.
                    let value = if !grp.dol.is_empty() {
                        let dol_parsed = crate::server::record::parse_link_v2(&grp.dol);
                        self.read_link_value(&dol_parsed, visited, depth).await
                    } else {
                        Some(EpicsValue::Double(grp.dov))
                    };
                    if let Some(value) = value {
                        // C `seqRecord.c:264` drives each LNKn via
                        // `dbPutLink`, whose `DBF_OUTLINK` target is
                        // processed by `dbDbPutValue` (`dbDbLink.c:388`)
                        // only when the link carries an explicit `PP`
                        // modifier. A bare `LNKn` is NPP — the value is
                        // written but the target is NOT processed.
                        // `parse_output_link_v2` applies that
                        // OUT-link-correct NPP default (the dfanout arm
                        // above open-codes the same downgrade).
                        // LNKn may be a local DB link or an external
                        // `ca://`/`pva://` link — C `dbPutLink` routes
                        // both through the link set's `putValue`
                        // (dbLink.c:434-448).
                        let lnk_parsed = crate::server::record::parse_output_link_v2(&grp.lnk);
                        self.write_out_link_value(&lnk_parsed, value, src_putf, visited, depth)
                            .await;
                    }
                }
            }
            MultiOut::Sseq(groups) => {
                for idx in indices {
                    let grp = &groups[idx];
                    if grp.lnk.is_empty() {
                        continue;
                    }
                    // Per-step DLYn staggering — C `sseqRecord.c`
                    // schedules each selected step after its delay
                    // (`callbackRequestDelayed`). Steps process
                    // sequentially in index order, each after its own
                    // delay — identical to the `MultiOut::Seq` arm.
                    if grp.dly > 0.0 {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(grp.dly)).await;
                    }
                    // Determine value: read from DOL link, or use DO/STR field
                    let value = if !grp.dol.is_empty() {
                        let dol_parsed = crate::server::record::parse_link_v2(&grp.dol);
                        self.read_link_value(&dol_parsed, visited, depth).await
                    } else if !grp.str_val.is_empty() {
                        Some(EpicsValue::String(grp.str_val.clone()))
                    } else {
                        Some(EpicsValue::Double(grp.do_val))
                    };
                    if let Some(value) = value {
                        // sseq `LNKn` is `DBF_OUTLINK` driven via
                        // `dbPutLink` → `dbDbPutValue` (`dbDbLink.c:388`):
                        // a bare `LNKn` is NPP. `parse_output_link_v2`
                        // applies the OUT-link-correct NPP default. An
                        // external `ca://`/`pva://` `LNKn` is routed
                        // through the link set's `putValue`
                        // (dbLink.c:434-448).
                        let lnk_parsed = crate::server::record::parse_output_link_v2(&grp.lnk);
                        self.write_out_link_value(&lnk_parsed, value, src_putf, visited, depth)
                            .await;
                    }
                }
            }
        }
    }

    /// Post the software event named by an `event` record's `VAL`.
    ///
    /// Mirrors C `eventRecord.c:120` `postEvent(prec->epvt)` — every
    /// `process()` of an event record posts its event, waking the
    /// `SCAN="Event"` records whose `EVNT` resolves to that name.
    /// No-op for any other record type, or when `VAL` is empty /
    /// resolves to event 0 (`eventNameToHandle` returns NULL).
    pub(crate) async fn dispatch_event_record(&self, rec: &Arc<RwLock<RecordInstance>>) {
        let event_name = {
            let instance = rec.read().await;
            if instance.record.record_type() != "event" {
                return;
            }
            match instance.record.get_field("VAL") {
                Some(EpicsValue::String(s)) => s,
                _ => return,
            }
        };
        if event_name.trim().is_empty() {
            return;
        }
        // C `postEvent` queues callbacks on the scan ring buffer —
        // the event-scanned records run on a separate callback thread,
        // NOT recursively inside this process cycle. Spawn the routed
        // post so a chain of event records cannot recurse unboundedly
        // and the current cycle's FLNK/CP dispatch is not blocked.
        let db = self.clone();
        crate::runtime::task::spawn(async move {
            db.post_event_named(&event_name).await;
        });
    }

    /// Register a CP link: when source_record changes, process target_record.
    ///
    /// Both names are normalised to canonical form so the cp_links
    /// map's key/value always match the canonical record name that
    /// `dispatch_cp_targets` uses for lookup. Without this, a user
    /// who wrote `INP="ALIAS_NAME CP"` in their .db file would
    /// register the CP edge under the alias key and then never see
    /// the target processed (the source record's canonical-name
    /// dispatch would miss).
    pub async fn register_cp_link(&self, source_record: &str, target_record: &str) {
        let source = self
            .resolve_alias(source_record)
            .await
            .unwrap_or_else(|| source_record.to_string());
        let target = self
            .resolve_alias(target_record)
            .await
            .unwrap_or_else(|| target_record.to_string());
        let mut cp = self.inner.cp_links.write().await;
        let targets = cp.entry(source).or_default();
        if !targets.contains(&target) {
            targets.push(target);
        }
    }

    /// Get target records that should be processed when source_record changes (CP links).
    pub async fn get_cp_targets(&self, source_record: &str) -> Vec<String> {
        self.inner
            .cp_links
            .read()
            .await
            .get(source_record)
            .cloned()
            .unwrap_or_default()
    }

    /// Scan all records for CP input links and register them.
    pub async fn setup_cp_links(&self) {
        let names = self.all_record_names().await;
        let mut links_to_register: Vec<(String, String)> = Vec::new();

        for target_name in &names {
            if let Some(rec_arc) = self.get_record(target_name).await {
                let instance = rec_arc.read().await;
                // Check common INP link
                let inp_str = &instance.common.inp;
                if !inp_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(inp_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
                        }
                    }
                }
                // Check multi-input links (INPA..INPL for calc/calcout/sel/sub)
                for (lf, _vf) in instance.record.multi_input_links() {
                    if let Some(EpicsValue::String(link_str)) = instance.record.get_field(lf) {
                        if !link_str.is_empty() {
                            let parsed = crate::server::record::parse_link_v2(&link_str);
                            if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                                if db.policy
                                    == crate::server::record::LinkProcessPolicy::ChannelProcess
                                {
                                    links_to_register
                                        .push((db.record.clone(), target_name.clone()));
                                }
                            }
                        }
                    }
                }
                // Check additional input link fields that may use CP:
                // DOL (ao/bo/longout/mbbo), DOL0-DOLF (seq — 16
                // groups), DOL1-DOLA (sseq — legacy 10 groups),
                // NVL (sel), SELL (sseq), SDIS (common), SGNL (histogram)
                const CP_INPUT_LINK_FIELDS: &[&str] = &[
                    "DOL", "DOL0", "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8",
                    "DOL9", "DOLA", "DOLB", "DOLC", "DOLD", "DOLE", "DOLF", "NVL", "SELL", "SGNL",
                ];
                for field_name in CP_INPUT_LINK_FIELDS {
                    if let Some(EpicsValue::String(link_str)) =
                        instance.record.get_field(field_name)
                    {
                        if !link_str.is_empty() {
                            let parsed = crate::server::record::parse_link_v2(&link_str);
                            if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                                if db.policy
                                    == crate::server::record::LinkProcessPolicy::ChannelProcess
                                {
                                    links_to_register
                                        .push((db.record.clone(), target_name.clone()));
                                }
                            }
                        }
                    }
                }
                // Check TSEL in common fields
                let tsel_str = &instance.common.tsel;
                if !tsel_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(tsel_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
                        }
                    }
                }
                // Check SDIS in common fields
                let sdis_str = &instance.common.sdis;
                if !sdis_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(sdis_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
                        }
                    }
                }
            }
        }

        let count = links_to_register.len();
        for (source, target) in links_to_register {
            self.register_cp_link(&source, &target).await;
        }
        if count > 0 {
            eprintln!("iocInit: {count} CP link subscriptions");
        }
    }
}
