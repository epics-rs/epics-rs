use std::collections::HashSet;
use std::sync::Arc;

use crate::runtime::sync::RwLock;
use crate::server::record::{AlarmSeverity, NotifyWaitSet, RecordInstance, ScanType};
use crate::types::EpicsValue;

use super::processing::join_put_notify;
use super::{LinkPutOp, PvDatabase, SelmKind, SelmResult, dbr_ushort_cast, select_link_indices_ex};

/// Record-specific input link fields that may carry a CP/CPP modifier:
/// DOL (ao/bo/longout/mbbo), DOL0-DOLF (seq — 16 groups), DOL1-DOLA
/// (sseq — legacy 10 groups), NVL (sel), SELL (sseq), SGNL (histogram).
///
/// Shared by [`PvDatabase::record_link_fields`] (the single owner of
/// "which fields on a record are links") and consumed transitively by
/// both [`PvDatabase::setup_cp_links`] (CA CP/CPP) and the pvalink
/// install scan (PVA CP/CPP), so the two enumerations cannot diverge.
pub(crate) const CP_INPUT_LINK_FIELDS: &[&str] = &[
    "DOL", "DOL0", "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8", "DOL9", "DOLA",
    "DOLB", "DOLC", "DOLD", "DOLE", "DOLF", "NVL", "SELL", "SGNL",
];

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

/// Apply C `recGblInheritSevrMsg` (recGbl.c:263-281) for one MS-class
/// link: fold the link source's alarm (`src`) into the destination
/// record's PENDING alarm (`dest`) per the maximize-severity mode `ms`.
///
/// * **NMS** — no propagation.
/// * **MS**  — raise dest severity to `src.sevr` under `LINK_ALARM`
///   (NOT the source's stat); no message.
/// * **MSI** — same as MS, but only when the source is at `INVALID`.
/// * **MSS** — copy the source's stat + severity + amsg (the only mode
///   that propagates the message).
///
/// Shared by the INPUT-link read path (`processing.rs`, where `dest` is
/// the record reading its inputs) and the DB OUT-link write path
/// ([`Database::write_db_link_value`], C `dbDbPutValue` →
/// `recGblInheritSevrMsg`, dbDbLink.c:382-383, where `dest` is the
/// OUT-link target). One implementation keeps the two sides from
/// diverging — an earlier INPUT-side variant wrongly treated MS like
/// MSS (propagating source stat + amsg through plain MS).
pub(crate) fn inherit_sevr_msg(
    dest: &mut crate::server::record::CommonFields,
    ms: crate::server::record::MonitorSwitch,
    src: &LinkAlarm,
) {
    use crate::server::recgbl::{alarm_status, rec_gbl_set_sevr, rec_gbl_set_sevr_msg};
    use crate::server::record::{AlarmSeverity, MonitorSwitch};
    match ms {
        MonitorSwitch::Maximize => {
            rec_gbl_set_sevr(dest, alarm_status::LINK_ALARM, src.sevr);
        }
        MonitorSwitch::MaximizeIfInvalid => {
            if src.sevr == AlarmSeverity::Invalid {
                rec_gbl_set_sevr(dest, alarm_status::LINK_ALARM, src.sevr);
            }
        }
        MonitorSwitch::MaximizeStatus => {
            rec_gbl_set_sevr_msg(dest, src.stat, src.sevr, src.amsg.clone());
        }
        MonitorSwitch::NoMaximize => {} // NMS: do not propagate
    }
}

/// The source record's per-cycle state propagated to a DB OUT-link
/// target, captured once from the source and threaded to every OUT-link
/// write so all targets in one cycle see the same snapshot:
///
/// * `putf` / `notify` — the PUTF bit and put-notify wait-set that C
///   `processTarget` carries to each target (dbDbLink.c:460-474).
/// * `alarm` — the committed alarm that C `recGblInheritSevrMsg` folds
///   into the dest per the link's MS-class switch (dbDbLink.c:382-383).
///
/// Bundled so the OUT-link write path threads one snapshot instead of
/// three positional arguments. The FLNK process-trigger path keeps the
/// lighter `PutNotifyCtx` (a forward link propagates no value and thus
/// no alarm).
#[derive(Clone, Copy)]
pub(crate) struct OutLinkSrc<'a> {
    pub putf: bool,
    pub notify: Option<&'a Arc<NotifyWaitSet>>,
    pub alarm: &'a LinkAlarm,
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

/// Typed multi-output payload — replaces the legacy `\0`-packed
/// `Vec<String>` so a link string containing an embedded NUL can
/// never mis-split (parity review 04-L3).
///
/// `sseq` is NOT a variant here: a `sseq` record drives its per-step
/// `LNKn` writes itself, in `SseqRecord::process()`, through the async
/// PACT machine (C `sseqRecord.c::processCallback`) — not via this
/// all-at-once dispatch.
pub(crate) enum MultiOut {
    /// fanout — 16 forward-link strings (LNK0..LNKF).
    Fanout(Vec<String>),
    /// dfanout — 16 output-link strings (OUTA..OUTP).
    Dfanout(Vec<String>),
    /// seq — 16 link groups (0..F).
    Seq(Vec<SeqGroup>),
}

impl MultiOut {
    /// Number of link slots — the `count` passed to the SELM selector.
    fn len(&self) -> usize {
        match self {
            MultiOut::Fanout(v) => v.len(),
            MultiOut::Dfanout(v) => v.len(),
            MultiOut::Seq(v) => v.len(),
        }
    }
}

/// Record types whose multi-output link groups are dispatched by
/// [`PvDatabase::dispatch_multi_output`].
///
/// SINGLE-OWNER INVARIANT — each of these record types' output links
/// (fanout `LNKn`, dfanout `OUTn`, seq `LNKn`) is dispatched (value
/// written + target forward-link processed) **exactly once per process
/// cycle, by `dispatch_multi_output` and by nothing else**.
///
/// `dispatch_multi_output` is the sole owner because it is the only
/// path that performs the full C-record model: SELL→SELN resolution,
/// SELM/OFFS/SHFT selection, per-group DOLn input fetch, and per-group
/// DLYn delay.
///
/// `sseq` is deliberately NOT listed: its `LNKn` writes are owned by
/// `SseqRecord::process()` (the async PACT machine, C
/// `sseqRecord.c::processCallback`), not by this dispatch. `sseq` also
/// does not implement `Record::multi_output_links`, so the generic
/// block skips it for that reason — there is no second dispatcher to
/// gate against.
///
/// MUST NOT: the generic `multi_output_links` block in `processing.rs`
/// (run unconditionally for every record after `dispatch_multi_output`)
/// must skip any record type listed here. `multi_output_dispatch_owned`
/// is consulted by that block (see `run_forward_link_tail_with_putf`
/// §4.6) so a double-dispatch is structurally impossible, not merely
/// removed at one call site.
pub(crate) fn multi_output_dispatch_owned(record_type: &str) -> bool {
    matches!(record_type, "fanout" | "dfanout" | "seq")
}

impl PvDatabase {
    /// Read a `Db`-variant link's value honoring C `dbInitLink`'s
    /// locality rule (`dbLink.c:118-130`): a PV link whose target record
    /// exists in this IOC reads from the local database; a non-local
    /// target is a CA link — `dbDbInitLink` fails to resolve it locally
    /// and falls through to `dbCaAddLinkCallbackOpt`, so its value comes
    /// from the external resolver. Pre-fix every `Db` arm read only the
    /// local DB (`get_pv`) and returned `None` for a non-local target, so
    /// a plain `INP="other:pv"` (no modifier) and a re-parsed multi-input
    /// (`INPA`..`INPL`) / `DOL` link never read the remote value.
    ///
    /// This is the single owner of that rule for the value-read path, so
    /// it holds uniformly — for every link field and regardless of an
    /// explicit `CP`/`CPP`/`CA` modifier — not only the
    /// `INP`/`OUT`/`TSEL`/`SDIS` parse caches the iocInit CP scan
    /// rewrites (the per-cycle re-parsed links have no cache to rewrite,
    /// so an init-time conversion can never reach them). The external
    /// read routes through the lset's lazy-open path: the first read
    /// opens the CA link and returns `None` until the monitor connects,
    /// then serves the cached value — exactly C `dbCaGetLink`.
    async fn read_db_link_value(&self, db: &crate::server::record::DbLink) -> Option<EpicsValue> {
        self.read_target_value(&db.record, &db.field).await
    }

    /// Read a `(record, field)` link target's value, dispatching by C
    /// `dbInitLink` locality (`dbLink.c:118-130`): a target present in
    /// this IOC reads from the local database (`get_pv`); a non-local
    /// target is a CA link, resolved through the external path
    /// (`dbCaGetLink`). The single owner of that locality decision for
    /// the value-read path — shared by [`Self::read_db_link_value`] (the
    /// record's own `Db` link) and the lnkCalc input loop
    /// ([`Self::evaluate_calc_link`]), whose `A..L` inputs are each their
    /// own `dbInitLink` link and so become CA links when non-local.
    async fn read_target_value(&self, record: &str, field: &str) -> Option<EpicsValue> {
        let pv_name = if field == "VAL" {
            record.to_string()
        } else {
            format!("{record}.{field}")
        };
        if self.has_name_no_resolve(record).await {
            self.get_pv(&pv_name).await.ok()
        } else {
            self.resolve_external_pv(&pv_name).await
        }
    }

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
            // Resolve through the per-link identity key (not bare `j.pv`)
            // so two same-PV structured links keep distinct configs —
            // matches the boundary key the write/scan/alarm paths use via
            // `external_pv_name` (pvxs per-link `pvaLinkConfig`,
            // ioc/pvalink.h:65).
            crate::server::record::ParsedLink::PvaJson(j) => {
                self.resolve_external_pv(&j.link_identity_key()).await
            }
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) => {
                // PP: process source record if Passive before reading.
                // Threads the caller's `visited`/`depth` so an A↔B PP
                // cycle terminates at the existing cycle guard instead
                // of recursing with a fresh set.
                self.process_passive_db_source(db, visited, depth).await;
                self.read_db_link_value(db).await
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

    /// Read a link's current value WITHOUT processing a Passive source —
    /// the parity of C `dbGetLink` (`dbLink.c:325` → `dbTryGetLink` →
    /// `lset->getValue`), which fetches the value for *any* link type
    /// (DB / CA / PVA / constant / lnkCalc) and never processes the
    /// target record.
    ///
    /// Distinct from [`Self::read_link_value`], which threads
    /// `visited`/`depth` to PP-process a Passive DB source before reading
    /// (the INPUT-link path). `dbGetLink` does no such processing, so the
    /// DB arm here reads with a plain `get_pv` exactly as the pre-fix
    /// control-link sites did.
    ///
    /// Used by the control links that C reads via `dbGetLink` every
    /// process cycle — `SDIS`→`disa`, `SIML`→`simm`, `SELL`→`seln`, and
    /// `TSEL`'s `TSE` load. The pre-fix sites open-coded an
    /// `if let ParsedLink::Db` read, so a control link sourced over
    /// CA/PVA or given as a constant was silently ignored.
    pub(crate) async fn read_link_value_no_process(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::None => None,
            crate::server::record::ParsedLink::Ca(ca) => self.resolve_external_pv(&ca.pv).await,
            crate::server::record::ParsedLink::Pva(name) => self.resolve_external_pv(name).await,
            // Per-link identity key, as in `read_link_value` above.
            crate::server::record::ParsedLink::PvaJson(j) => {
                self.resolve_external_pv(&j.link_identity_key()).await
            }
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) => self.read_db_link_value(db).await,
            // Hardware links carry no generic readable value.
            crate::server::record::ParsedLink::Hw(_) => None,
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
            // Each lnkCalc input is its own `dbInitLink` link, so a
            // non-local input record is a CA link — read it through the
            // locality owner, not a local-only `get_pv`. `arg` is a bare
            // record name or `record.FIELD`; split on the last `.`.
            let (record, field) = match arg.rsplit_once('.') {
                Some((r, f)) => (r, f),
                None => (arg.as_str(), "VAL"),
            };
            let v = self.read_target_value(record, field).await?;
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
                if self.has_name_no_resolve(record_name).await {
                    let rec = self.get_record(record_name).await?;
                    let inst = rec.read().await;
                    Some(inst.common.time)
                } else {
                    // Non-local time source → CA link; pull the remote
                    // `.TIME` through the external resolver (C
                    // `dbGetTimeStamp` on a CA link), same as the
                    // non-local TSEL `.TIME` adoption.
                    let (secs, ns, _utag) = self
                        .external_link_time(&format!("ca://{record_name}"))
                        .await?;
                    let secs = secs.max(0) as u64;
                    let ns = (ns.max(0) as u32).min(999_999_999);
                    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, ns))
                }
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
                // C `dbInitLink` locality (`dbLink.c:118-130`): a target
                // record present in this IOC is a DB link read from the
                // local database; a non-local target is a CA link, so its
                // value and raw remote alarm come from the external
                // resolver — identical to the `Ca`/`Pva` arm below.
                if !self.has_name_no_resolve(&db.record).await {
                    return (
                        self.resolve_external_pv(&pv_name).await,
                        self.external_link_alarm(&pv_name).await,
                    );
                }
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
            // CA: the `MS`/`NMS`/`MSI`/`MSS` modifier is
            // now carried in the `CaLink`, so the resolver returns the
            // *raw* remote alarm and record processing applies the gate
            // using `link.monitor_switch()`. Either way this fn just
            // reads the raw/gated alarm; the switch pairing happens in
            // `processing.rs`. Without this, a connected external link
            // carrying a remote MINOR/MAJOR severity never folded into
            // the owning record's LINK_ALARM (B2).
            crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
            | crate::server::record::ParsedLink::Ca(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                let value = self.resolve_external_pv(&name).await;
                let alarm = self.external_link_alarm(&name).await;
                (value, alarm)
            }
            _ => (None, None),
        }
    }

    /// Latched upstream timestamp from the lset, when the
    /// link is configured with `time=true`. The lset gates internally
    /// (returning `None` for links without the `time` option), so a
    /// `Some` here is the authoritative remote timestamp the
    /// processing path should adopt into the owning record's
    /// `common.time` and `common.utag`. Mirrors pvxs
    /// `pvalink_lset.cpp:427`.
    ///
    /// Returns `(seconds_since_epoch, nanoseconds, userTag)` exactly as
    /// the lset reports them; the caller folds the time into the
    /// record's `SystemTime` via `UNIX_EPOCH + Duration::new(...)` and
    /// adopts the `userTag` into `common.utag`. The `userTag` is the
    /// remote `timeStamp.userTag` widened without sign extension, or `0`
    /// when the source carries none.
    /// Every registered lset, as a snapshot.
    ///
    /// The bare-name link paths try each lset in turn, and every lset call is
    /// now an await. Snapshotting the registry (rather than iterating it under
    /// its read guard) keeps the registry lock off the await path, so an lset
    /// that registers or unregisters another while resolving cannot deadlock
    /// against the caller.
    async fn registered_link_sets(&self) -> Vec<crate::server::database::DynLinkSet> {
        let registry = self.inner.link_sets.read().await;
        registry
            .schemes()
            .iter()
            .filter_map(|s| registry.get(s))
            .collect()
    }

    pub(crate) async fn external_link_time(&self, name: &str) -> Option<(i64, i32, u64)> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset.
            for lset in self.registered_link_sets().await {
                if let Some(ts) = lset.time_stamp(name).await {
                    return Some(ts);
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        lset.time_stamp(body).await
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
            for lset in self.registered_link_sets().await {
                if let Some(sev) = lset.alarm_severity(name).await {
                    return Some(LinkAlarm {
                        // prefer the remote STAT for MSS;
                        // fall back to LINK_ALARM when the lset has none.
                        stat: lset
                            .alarm_status(name)
                            .await
                            .map(|s| s as u16)
                            .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
                        sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
                        amsg: lset.alarm_message(name).await.unwrap_or_default(),
                    });
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        let sev = lset.alarm_severity(body).await?;
        Some(LinkAlarm {
            // remote STAT for MSS, else LINK_ALARM.
            stat: lset
                .alarm_status(body)
                .await
                .map(|s| s as u16)
                .unwrap_or(crate::server::recgbl::alarm_status::LINK_ALARM),
            sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
            amsg: lset.alarm_message(body).await.unwrap_or_default(),
        })
    }

    /// Ungated remote alarm snapshot for an external (`pva://` /
    /// `ca://`) link — the DB-link inspection counterpart of
    /// [`Self::external_link_alarm`].
    ///
    /// Where `external_link_alarm` returns the **gated** maximize-severity
    /// contribution folded into the owning record's `LINK_ALARM` (pvxs
    /// `pvaGetValue` applying the `MS`/`NMS`/`MSI` gate,
    /// `pvalink_lset.cpp:424-431`), this returns the **ungated** remote
    /// `(severity, status, message)` snapshot pvxs exposes through
    /// `dbGetAlarm` / `dbGetAlarmMsg` (`pvaGetAlarmMsg`,
    /// `pvalink_lset.cpp:542-575`). A default `NMS` link reports its
    /// remote severity here even though it leaves the owning record
    /// unraised.
    ///
    /// `None` when no lset is registered for the scheme, the link is not
    /// connected, or the lset does not track remote alarms. Scheme
    /// dispatch mirrors [`Self::external_link_alarm`].
    pub async fn external_link_alarm_snapshot(
        &self,
        name: &str,
    ) -> Option<crate::server::database::RemoteAlarm> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            for lset in self.registered_link_sets().await {
                if let Some(snap) = lset.remote_alarm(name).await {
                    return Some(snap);
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        lset.remote_alarm(body).await
    }

    /// Remote display / control / valueAlarm metadata for an external
    /// (`pva://` / `ca://`) link, resolved through the registered
    /// lset's [`LinkSet::link_metadata`] hook.
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
            for lset in self.registered_link_sets().await {
                if let Some(meta) = lset.link_metadata(name).await {
                    return Some(meta);
                }
            }
            return None;
        };
        let lset = self.inner.link_sets.read().await.get(scheme)?;
        lset.link_metadata(body).await
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
                // recursive INP-link source processing within
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
                self.read_db_link_value(db).await
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_)
                if is_soft =>
            {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                self.resolve_external_pv(&name).await
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
    /// Returns `true` when the write failed — C `dbPutLink` status `!= 0`:
    /// the local `dbPut` was rejected (conversion error, missing field) or
    /// the non-local external write returned `Err`. Callers that mirror a
    /// record's `dbPutLink` status (e.g. dfanout `push_values` raising
    /// `LINK_ALARM/MAJOR`, dfanoutRecord.c:312) fold this into the source's
    /// alarm; the fanout/seq dispatch paths ignore it.
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
        src: OutLinkSrc<'_>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> bool {
        let target_name = if link.field == "VAL" {
            link.record.clone()
        } else {
            format!("{}.{}", link.record, link.field)
        };
        // C `dbInitLink` locality (`dbLink.c:118-130`): a target record
        // not present in this IOC is a CA link, so its write is routed
        // through the external put path (`dbCaPutLink`), not a local
        // `dbPut`. The alarm-inheritance / PUTF / `processTarget`
        // machinery below is the local-DB `dbDbPutValue` body
        // (dbDbLink.c:372-393), which `dbCaPutLink` performs none of —
        // so a non-local target returns right after the remote write.
        // OUTPUT-side twin of the `read_db_link_value` locality fallback.
        if !self.has_name_no_resolve(&link.record).await {
            let op = Self::external_put_op(src.notify);
            if let Err(e) = self.write_external_pv(&target_name, value, op).await {
                eprintln!("OUT-link write to external PV '{target_name}' failed: {e}");
                return true;
            }
            return false;
        }
        // an OUT-link write-back is an internal step of the
        // processing chain that already holds the entry record's
        // advisory write gate (`dbScanLock` analogue). It must use the
        // `_already_locked` write so it does not re-acquire a gate: a
        // self-referencing OUT link (`SELF PP`) would otherwise
        // dead-lock on the entry record's own non-reentrant gate. C
        // `dbDbPutValue` writes the OUT-link target under the same
        // lock set the chain already owns.
        let put_result = self.put_pv_already_locked(&target_name, value).await;

        // C `dbDbPutValue` (dbDbLink.c:382-383) folds the SOURCE
        // record's alarm into the destination via `recGblInheritSevrMsg`,
        // AFTER the `dbPut` and BEFORE `processTarget`. In this port the
        // source's per-cycle alarm has already been committed by
        // `rec_gbl_reset_alarms` before the OUT link dispatches, so
        // `src_alarm` carries the source's *committed* stat/sevr/amsg —
        // the same values C reads from the still-pending nsta/nsev/namsg
        // at its (earlier) synchronous `writeValue` point. The inherited
        // severity lands in the dest's PENDING nsev/nsta(/namsg for MSS);
        // the dest commits it on its next `rec_gbl_reset_alarms` — its
        // own process cycle, reached below for a `.PROC`/`PP` link, or a
        // later independent scan otherwise. NMS (the common case) skips
        // the dest lookup/lock entirely.
        if link.monitor_switch != crate::server::record::MonitorSwitch::NoMaximize {
            if let Some(target_rec) = self.get_record(&link.record).await {
                let mut tg = target_rec.write().await;
                inherit_sevr_msg(&mut tg.common, link.monitor_switch, src.alarm);
            }
        }

        // C `dbDbPutValue` (dbDbLink.c:384-385) returns the put status
        // immediately after the alarm inheritance and BEFORE the
        // `.PROC`/`PP` `processTarget` branch: only a successful write
        // reaches target processing. A failed OUT-link write (missing
        // field, record put rejection) must therefore NOT trigger the
        // target's process cycle, which would otherwise run the target
        // on its stale field value and diverge from C on side effects,
        // FLNK, alarms, and put-notify completion ordering. The alarm
        // inheritance above already ran (C folds it regardless of
        // status), matching the C ordering exactly.
        //
        // An empty array into a scalar field is NOT such a failure: C
        // `dbPut` accepts it, raises LINK/INVALID on the destination and
        // returns 0 (`dbAccess.c:1370-1372`), so a `PP` link still
        // processes its target — see `field_io::PutRequest`.
        if put_result.is_err() {
            return true;
        }

        // C `dbDbPutValue` (`dbDbLink.c:387-390`) processes the target
        // when the destination field is `.PROC` **or** the link carries
        // `pvlOptPP` (an explicit ` PP` token → `ProcessPassive`). The
        // `.PROC` arm is independent of the PP flag, so it is checked
        // here in the write path rather than encoded as a parse-time
        // policy: a modifier-less link now defaults to `NoProcess`
        // uniformly (INPUT and OUTPUT alike), and writing a value into a
        // record's `.PROC` field still forces a process.
        if link.field == "PROC"
            || link.policy == crate::server::record::LinkProcessPolicy::ProcessPassive
        {
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
                    let scan = tg.common.scan;
                    if !pact {
                        tg.common.putf = src.putf;
                        // C `dbNotifyAdd` (dbDbLink.c:460) lives inside
                        // `processTarget`, which `dbDbPutValue` reaches
                        // for a `.PROC` write or a `PP` link to a passive
                        // target (dbDbLink.c:387-389). This Rust port only
                        // processes the target on the passive branch
                        // below, so gate the join on the same condition:
                        // a target that will not be processed must NOT
                        // join, or it would `enter` the wait-set without
                        // ever `leave`ing it and hang the completion.
                        if scan == ScanType::Passive {
                            join_put_notify(&mut tg, src.notify);
                        }
                    } else if src.putf && !on_chain {
                        tg.common.rpro = true;
                        tg.common.putf = false;
                    }
                    (scan, !pact)
                };
                if should_process && target_scan == ScanType::Passive {
                    // recursive OUT-link target processing within
                    // one chain — gate held by the foreign entry record.
                    let _ = self
                        .process_record_with_links_recursive(&link.record, visited, depth + 1)
                        .await;
                }
            }
        }
        // Successful local write (C `dbPutLink` status 0).
        false
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
    ///
    /// `op` carries the delivery semantics the lset must honour:
    /// [`LinkPutOp::Async`] when the write is part of a put-notify /
    /// blocking-put chain (mirrors C `dbPutLinkAsync` / pvxs
    /// `pvaPutValueAsync`), [`LinkPutOp::Plain`] otherwise.
    pub(crate) async fn write_external_pv(
        &self,
        name: &str,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> Result<(), String> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset in turn, first
            // accepting write wins (mirrors `resolve_external_pv`'s
            // bare-name path).
            let lsets = self.registered_link_sets().await;
            if lsets.is_empty() {
                return Err(format!("no link set registered for external link '{name}'"));
            }
            let mut last_err = String::new();
            for lset in lsets {
                match lset.put_value(name, value.clone(), op).await {
                    Ok(()) => {
                        // Production drain of any retry-queued OUT
                        // writes on this lset now that a write has
                        // reached it (the channel may have just
                        // reconnected). pvxs replays the queued
                        // put from record processing, not test code
                        // (`pvalink_channel.cpp:220-263`).
                        lset.flush_puts().await;
                        return Ok(());
                    }
                    Err(e) => last_err = e,
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
        let result = lset.put_value(body, value, op).await;
        if result.is_ok() {
            lset.flush_puts().await;
        }
        result
    }

    /// Fire a forward link (FLNK) whose target is an external
    /// (`pva://` / `ca://`) PV — the FWD-link counterpart of
    /// [`Self::write_external_pv`]. Resolves the scheme (or tries every
    /// registered lset for a bare name, first to accept wins, exactly as
    /// the OUT-write path does) and delegates to [`LinkSet::scan_forward`].
    ///
    /// Mirrors C `dbScanFwdLink` → `plink->lset->scanForward`
    /// (`dbLink.c:475-480`): the database hands the forward link to the
    /// link set, which (for pvalink) runs `pvaScanForward`. Returns the
    /// lset's `Err` unchanged so the caller can raise LINK/INVALID on the
    /// owning record (pvxs `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)`).
    pub(crate) async fn scan_forward_external_pv(&self, name: &str) -> Result<(), String> {
        let (scheme, body) = if let Some(rest) = name.strip_prefix("pva://") {
            ("pva", rest)
        } else if let Some(rest) = name.strip_prefix("ca://") {
            ("ca", rest)
        } else {
            // Bare name — try every registered lset in turn, first
            // accepting the forward wins (mirrors `write_external_pv`'s
            // bare-name path so FWD and OUT route a bare target alike).
            let lsets = self.registered_link_sets().await;
            if lsets.is_empty() {
                return Err(format!("no link set registered for forward link '{name}'"));
            }
            let mut last_err = String::new();
            for lset in lsets {
                match lset.scan_forward(name).await {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = e,
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
        lset.scan_forward(body).await
    }

    /// Map a record's put-completion wait-set to the external-link put
    /// op. A write that originates inside a put-notify / blocking-put
    /// chain (the source record carries a completion wait-set) is
    /// delivered as [`LinkPutOp::Async`] (the pvxs `pvaPutValueAsync` /
    /// C `dbPutLinkAsync` path); a plain record-processing OUT write is
    /// [`LinkPutOp::Plain`]. Single owner of the notify→op mapping so
    /// the external-OUT dispatch sites cannot diverge.
    fn external_put_op(src_notify: Option<&Arc<NotifyWaitSet>>) -> LinkPutOp {
        if src_notify.is_some() {
            LinkPutOp::Async
        } else {
            LinkPutOp::Plain
        }
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
    /// C `devaCalcoutSoft.c::write_acalcout` (65-88) buffer choice for a
    /// multi-output pair that carries a scalar companion
    /// ([`crate::server::record::Record::multi_output_scalar_companion`]):
    /// resolve the TARGET element count and drive the scalar field when
    /// the effective count is 1, the array field otherwise.
    ///
    /// C's resolution, mirrored branch for branch:
    /// - source clamp `i = (nuse > 0) ? nuse : nelm` (:82-83) — already
    ///   the served array length (`array_field_value` serves
    ///   `num_elements()` elements), so a 1-element source picks the
    ///   scalar regardless of the target;
    /// - local DB target — `dbNameToAddr` `no_elements` (:78-79): the
    ///   target field's array-ness (`EpicsValue::is_array`, the port's
    ///   documented `no_elements > 1` stand-in). An unresolvable name
    ///   leaves `nelm = 1` (:68), picking the scalar;
    /// - external target (explicit `ca://`/`pva://` OR a DB-parsed name
    ///   not in this IOC, which C classifies as a CA link) —
    ///   `dbCaGetNelements` (:75-76): the lset's cached element count
    ///   ([`LinkSet::link_metadata`]); a disconnected link reports none
    ///   and leaves `nelm = 1`, exactly as C's failed `dbCaGetNelements`
    ///   leaves the initializer.
    ///
    /// The target-side `min(target, source)` truncation for counts > 1
    /// is the put path's own clamp (`dbAccess.c:1359` analogue in
    /// `field_io.rs`); only the `nelm == 1 ⇒ &scalar` pick needs to
    /// happen here, before the value is committed to the link write.
    pub(crate) async fn multi_out_buffer_choice(
        &self,
        link: &crate::server::record::ParsedLink,
        array_val: EpicsValue,
        scalar_companion: Option<EpicsValue>,
    ) -> EpicsValue {
        let Some(scalar) = scalar_companion else {
            return array_val;
        };
        // Source clamp: `i = (nuse>0 ? nuse : nelm); if (i < nelm) nelm = i;`
        // — a single-element source picks the scalar buffer for any target.
        if array_val.count() <= 1 {
            return scalar;
        }
        // C dbCaGetNelements analogue: the lset's cached element count,
        // defaulting to 1 when disconnected/uncached (C leaves the
        // initializer untouched when dbCaGetNelements fails).
        let external_nelements = |name: String| async move {
            self.external_link_metadata(&name)
                .await
                .and_then(|m| m.element_count)
                .unwrap_or(1)
        };
        let target_is_single = match link {
            crate::server::record::ParsedLink::Db(db) => {
                let target_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                if self.has_name_no_resolve(&db.record).await {
                    // C dbNameToAddr → no_elements: scalar field ⇒ 1. A
                    // resolvable record with an unknown field keeps C's
                    // failed-dbNameToAddr default (nelm = 1).
                    !self
                        .get_pv(&target_name)
                        .await
                        .map(|v| v.is_array())
                        .unwrap_or(false)
                } else {
                    // Non-local ⇒ CA link in C (`dbInitLink` locality).
                    external_nelements(target_name).await <= 1
                }
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                external_nelements(name.to_string()).await <= 1
            }
            // Constant / Hw / Calc / None: not a writable target — the
            // write path no-ops, the choice is irrelevant.
            _ => return array_val,
        };
        if target_is_single { scalar } else { array_val }
    }

    pub(crate) async fn write_out_link_value(
        &self,
        link: &crate::server::record::ParsedLink,
        value: EpicsValue,
        src: OutLinkSrc<'_>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        match link {
            crate::server::record::ParsedLink::Db(db) => {
                self.write_db_link_value(db, value, src, visited, depth)
                    .await;
            }
            crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_)
            | crate::server::record::ParsedLink::PvaJson(_) => {
                let name = link
                    .external_pv_name()
                    .expect("Ca/Pva/PvaJson link carries a PV name");
                let op = Self::external_put_op(src.notify);
                if let Err(e) = self.write_external_pv(&name, value, op).await {
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
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
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

    /// Read a record numeric field as `u16`, defaulting to 0.
    ///
    /// Used for `DBF_USHORT` fields such as `SELN`: the native unsigned
    /// carrier returns its value directly, while any other DBR type is
    /// truncated through the `i64` integer view (C `dbPut` cast), so a
    /// value ≥ 32768 round-trips instead of saturating at `i16::MAX`.
    fn field_u16(instance: &RecordInstance, field: &str) -> u16 {
        match instance.record.get_field(field) {
            Some(EpicsValue::UShort(v)) => v,
            Some(other) => other.as_int_i64().unwrap_or(0) as u16,
            None => 0,
        }
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
            // Write guard: a value-class post advances the record's
            // already-published state (`RecordInstance::record_value_post`),
            // so posting is a `&mut` operation.
            let mut inst = rec.write().await;
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
    ///
    /// `dfanout_pending_sevr` selects the dispatch phase and carries the
    /// severity the dfanout `push_values` decision needs:
    ///
    /// * `Some(nsev)` — the **dfanout pre-commit** phase. C
    ///   `dfanoutRecord.c:128-146` runs `push_values` between `checkAlarms`
    ///   and `recGblResetAlarms`, gating the push on the *pending* `nsev`
    ///   (`if nsev < INVALID push else switch(ivoa)`), and a failed
    ///   `dbPutLink` raises `LINK_ALARM/MAJOR` into that same `nsev`. The
    ///   caller runs this before the alarm commit and folds the returned
    ///   failure flag into the record's `nsev`, so the write alarm lands in
    ///   THIS cycle's committed SEVR and its VAL monitor post.
    /// * `None` — the **fanout/seq tail** phase (post-commit forward-link
    ///   tail). These drive no value and raise no write alarm.
    ///
    /// A dfanout reached with `None`, or a fanout/seq reached with `Some`,
    /// is skipped: each type dispatches in exactly one phase. Returns the
    /// single pending alarm `(stat, sevr)` C `push_values` would raise this
    /// cycle — `LINK_ALARM/MAJOR` for a failed OUT write or
    /// `SOFT_ALARM/INVALID` for a Specified `seln` out of range — for the
    /// caller to fold into `nsev` before the alarm commit. Always `None` for
    /// fanout/seq (they raise their own range alarm directly, post-commit).
    pub(crate) async fn dispatch_multi_output(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
        dfanout_pending_sevr: Option<AlarmSeverity>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> Option<(u16, AlarmSeverity)> {
        // Phase gate: dfanout drives its OUT links pre-commit (so a write
        // failure folds into the same cycle's SEVR), fanout/seq in the
        // forward-link tail. Skip the type that does not belong to the
        // calling phase so each dispatches exactly once per cycle.
        let is_dfanout = rec.read().await.record.record_type() == "dfanout";
        if is_dfanout != dfanout_pending_sevr.is_some() {
            return None;
        }

        // Snapshot the source record's PUTF bit + put-notify wait-set so
        // every write_db_link_value call below propagates them to its
        // target — C `dbDbLink.c::processTarget` PUTF and `dbNotifyAdd`
        // wait-set invariants (see write_db_link_value doc). The
        // committed alarm travels the same way for `recGblInheritSevrMsg`
        // MS-class propagation into each OUT-link target.
        let (src_putf, src_notify, src_alarm) = {
            let guard = rec.read().await;
            (
                guard.common.putf,
                guard.notify.clone(),
                LinkAlarm {
                    stat: guard.common.stat,
                    sevr: guard.common.sevr,
                    amsg: guard.common.amsg.clone(),
                },
            )
        };
        // One snapshot threaded to every OUT-link write below.
        let out_src = OutLinkSrc {
            putf: src_putf,
            notify: src_notify.as_ref(),
            alarm: &src_alarm,
        };

        // Resolve the SELL link into SELN before SELN is read below.
        // C `fanoutRecord.c:103` and `dfanoutRecord.c:126` call
        // `dbGetLink(&prec->sell, DBR_USHORT, &prec->seln, 0, 0)` at the
        // top of `process()`, *before* the SELM switch, so SELN is
        // refreshed from SELL every cycle regardless of SELM.
        // `seqRecord.c:148` places the identical `dbGetLink` *inside* the
        // `else` of `if (prec->selm == seqSELM_All)`, so an All-mode seq
        // never reads SELL — SELN stays frozen at its last value and posts
        // no spurious change (the post-process `if (seln != oldn)` monitor
        // is then a no-op). Match that per-record placement: fanout/dfanout
        // always, seq only when SELM != All. Only the `sel` record's
        // NVL->SELN binding was previously wired; fanout/dfanout/seq never
        // read the SELL link, so a SELL pointing at another record's value
        // field never updated SELN — the selection was frozen at whatever
        // SELN was initialised to. `sseq` reads its own SELL→SELN in
        // `SseqRecord::pre_input_link_actions` (the async machine owns the
        // whole cycle), so it is not handled here.
        {
            let sell = {
                let instance = rec.read().await;
                match instance.record.record_type() {
                    "fanout" | "dfanout" => Some(Self::field_str(&instance, "SELL")),
                    // seqSELM_All == 0 (seqRecord.dbd menu(seqSELM) order
                    // All/Specified/Mask). C skips the SELL→SELN read in
                    // All mode, so the port must too.
                    "seq" if Self::field_i16(&instance, "SELM") != 0 => {
                        Some(Self::field_str(&instance, "SELL"))
                    }
                    _ => None,
                }
            };
            if let Some(sell) = sell {
                if !sell.is_empty() {
                    // C reads SELL via `dbGetLink(&prec->sell, DBR_USHORT,
                    // &prec->seln, 0, 0)` for any link type; the pre-fix
                    // port only read a `ParsedLink::Db` SELL, so a SELL
                    // sourced over CA/PVA or given as a constant never
                    // updated SELN.
                    let parsed = crate::server::record::parse_link_v2(&sell);
                    if let Some(val) = self.read_link_value_no_process(&parsed).await {
                        // C reads SELL with `dbGetLink(&prec->sell,
                        // DBR_USHORT, &prec->seln, 0, 0)`; the dbConvert GET
                        // macro stores `*pdst = (epicsUInt16) *psrc`
                        // (`dbConvert.c:63-70`) — a C cast (truncate-then-
                        // wrap mod 2^16), NOT a clamp. `SELL=-1` therefore
                        // yields `SELN=65535` (Specified → out-of-range
                        // INVALID, Mask → all-bits), where the old clamp
                        // produced `0` (drove link 0 / empty mask). SELN is
                        // a `DBF_USHORT` (u16) field; `select_link_indices_ex`
                        // consumes it as `u16`.
                        let seln = dbr_ushort_cast(&val);
                        let mut instance = rec.write().await;
                        let _ = instance.record.put_field("SELN", EpicsValue::UShort(seln));
                    }
                }
            }
        }

        let dispatch_info: Option<(SelmResult, MultiOut, Option<EpicsValue>)> = {
            let instance = rec.read().await;
            match instance.record.record_type() {
                "fanout" => {
                    let selm = Self::field_i16(&instance, "SELM");
                    let seln = Self::field_u16(&instance, "SELN");
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
                    let seln = Self::field_u16(&instance, "SELN");
                    // IVOA / IVOV — invalid output handling, mirrors
                    // epics-base PR #688. When the record's pending
                    // severity is INVALID, IVOA selects: 0 = continue (use
                    // VAL as before), 1 = don't drive (suppress all OUT*),
                    // 2 = set outputs to IVOV.
                    //
                    // C `dfanoutRecord.c:128` gates the push on the
                    // *pending* `nsev` (`if (prec->nsev < INVALID_ALARM)
                    // push_values(); else switch(ivoa)`), evaluated after
                    // `checkAlarms` but before `recGblResetAlarms` commits
                    // it. This dispatch runs in that same pre-commit window,
                    // so the gate reads the caller-supplied pending
                    // severity, not the still-stale committed `common.sevr`.
                    let push_sevr = dfanout_pending_sevr
                        .expect("dfanout dispatch carries the pending severity (phase gate)");
                    let raw_val = instance.record.val();
                    let val = if push_sevr == crate::server::record::AlarmSeverity::Invalid {
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
                    let seln = Self::field_u16(&instance, "SELN");
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
                _ => None,
            }
        };

        let (sel, payload, val) = match dispatch_info {
            Some(info) => info,
            None => return None,
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
        // seqRecord.c:157). fanout/seq dispatch in the post-commit tail, so
        // they raise it directly into the committed SEVR and post SEVR/STAT
        // immediately (apply_selm_alarm). dfanout dispatches PRE-commit, so
        // its out-of-range alarm must instead fold into the pending nsev the
        // caller commits THIS cycle — captured here and returned below, not
        // raised+posted now (a direct SEVR raise would be clobbered by the
        // caller's `recGblResetAlarms`).
        let dfanout_selm_alarm = sel.alarm;
        if !is_dfanout {
            Self::apply_selm_alarm(rec, sel.alarm).await;
        }
        let indices = sel.indices;
        // C `dfanoutRecord.c` push_values raises LINK_ALARM/MAJOR per failed
        // dbPutLink; accumulated across the selected OUT links below.
        let mut link_failed = false;

        match payload {
            MultiOut::Fanout(links) => {
                for idx in indices {
                    let link_str = &links[idx];
                    if link_str.is_empty() {
                        continue;
                    }
                    // fanout `LNK1`..`LNKF` are `DBF_FWDLINK`
                    // (`fanoutRecord.dbd.pod:144`), so C masks their modifiers
                    // to `pvlOptCA` alone (`dbStaticLib.c:2390`).
                    let parsed = crate::server::record::parse_forward_link_v2(link_str);
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
                                // recursive link-target processing
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
                        // C `dfanoutRecord.c:323` drives each OUTn via
                        // `dbPutLink` → `dbDbPutValue`: an `DBF_OUTLINK`
                        // target is processed only when the link carries
                        // an explicit ` PP` token or the destination is
                        // `.PROC` (`dbDbLink.c:387-390`). A modifier-less
                        // OUTn is NPP — the value is written but the
                        // target is NOT re-processed (otherwise a
                        // Soft-Channel ai target's `convert()` would
                        // clobber the value just written). The NPP
                        // default and the `.PROC`/`PP` processing rule
                        // are both honoured by `parse_output_link_v2`
                        // (uniform NoProcess default) +
                        // `write_db_link_value` (write-path target
                        // processing), so no per-call downgrade is needed.
                        let parsed = crate::server::record::parse_output_link_v2(link_str);
                        match parsed {
                            crate::server::record::ParsedLink::Db(ref db) => {
                                // C `dfanoutRecord.c:311-312`: a failed
                                // dbPutLink raises LINK_ALARM/MAJOR.
                                if self
                                    .write_db_link_value(db, val.clone(), out_src, visited, depth)
                                    .await
                                {
                                    link_failed = true;
                                }
                            }
                            // External `ca://`/`pva://` OUTn — C
                            // `dbPutLink` routes a CA-link write through
                            // the link set's `putValue` identically to a
                            // DB link (dbLink.c:434-448). PP has no
                            // meaning for an external write (the remote
                            // record processes on its own IOC), so route
                            // straight through the link set.
                            crate::server::record::ParsedLink::Ca(_)
                            | crate::server::record::ParsedLink::Pva(_)
                            | crate::server::record::ParsedLink::PvaJson(_) => {
                                let name = parsed
                                    .external_pv_name()
                                    .expect("Ca/Pva/PvaJson link carries a PV name");
                                let op = Self::external_put_op(src_notify.as_ref());
                                if let Err(e) = self.write_external_pv(&name, val.clone(), op).await
                                {
                                    eprintln!(
                                        "dfanout OUT-link write to external PV '{name}' failed: {e}"
                                    );
                                    // C `dfanoutRecord.c:312` routes a CA OUT
                                    // link through dbPutLink identically — a
                                    // disconnected/rejected remote write
                                    // returns nonzero → LINK_ALARM/MAJOR.
                                    link_failed = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            MultiOut::Seq(groups) => {
                // DOn value-storage field names (`linkGrp.dov`),
                // index-aligned with the LNKn/DOLn groups.
                const DO_NAMES: [&str; 16] = [
                    "DO0", "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9", "DOA",
                    "DOB", "DOC", "DOD", "DOE", "DOF",
                ];
                // C `dbLinkIsConstant` is true for empty and numeric-literal
                // links; a real link is a DB/CA/PVA PV reference.
                let is_real = |s: &str| {
                    !s.is_empty()
                        && !matches!(
                            crate::server::record::parse_link_v2(s),
                            crate::server::record::ParsedLink::Constant(_)
                        )
                };
                let rec_name = rec.read().await.name.clone();
                for idx in indices {
                    let grp = &groups[idx];
                    // C `seqRecord.c:182-189`: a group is processed iff its
                    // LNKn OR DOLn is a real (non-constant) link. When both
                    // are constant/empty the group is skipped entirely — so
                    // a DOL-only group (real DOLn, empty LNKn) still reads
                    // back DOn and posts it, even though nothing is driven.
                    let lnk_real = is_real(&grp.lnk);
                    let dol_real = is_real(&grp.dol);
                    if !lnk_real && !dol_real {
                        continue;
                    }
                    // Per-group DLYn staggering — C `seqRecord.c`
                    // schedules each group after its delay. Groups
                    // process sequentially in index order, each after
                    // its own delay (callbackRequestDelayed chain).
                    if grp.dly > 0.0 {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(grp.dly)).await;
                    }
                    // C `seqRecord.c:259` `dbGetLink(&dol, DBR_DOUBLE,
                    // &dov)`: read DOLn into the DOn value field. A
                    // constant/empty DOL — or a failed read — leaves DOn at
                    // its previous value.
                    let new_dov = if dol_real {
                        let dol_parsed = crate::server::record::parse_link_v2(&grp.dol);
                        self.read_link_value(&dol_parsed, visited, depth)
                            .await
                            .and_then(|v| v.to_f64())
                            .unwrap_or(grp.dov)
                    } else {
                        grp.dov
                    };
                    // C `seqRecord.c:264` drives LNKn via `dbPutLink`
                    // (`DBR_DOUBLE, &dov`), whose `DBF_OUTLINK` target is
                    // processed by `dbDbPutValue` (`dbDbLink.c:388`) only
                    // when the link carries an explicit `PP` modifier. A
                    // bare `LNKn` is NPP — the value is written but the
                    // target is NOT processed. `parse_output_link_v2`
                    // applies that NPP default (the dfanout arm above
                    // open-codes the same downgrade). LNKn may be a local DB
                    // link or an external `ca://`/`pva://` link — C
                    // `dbPutLink` routes both through the link set's
                    // `putValue` (dbLink.c:434-448). Only a real LNK does
                    // anything; a constant LNK is a no-op.
                    if lnk_real {
                        let lnk_parsed = crate::server::record::parse_output_link_v2(&grp.lnk);
                        self.write_out_link_value(
                            &lnk_parsed,
                            EpicsValue::Double(new_dov),
                            out_src,
                            visited,
                            depth,
                        )
                        .await;
                    }
                    // C `seqRecord.c:266-268`: store DOn and post a
                    // DBE_VALUE|DBE_LOG monitor only when it changed. The
                    // field already holds the old value (snapshot read), so
                    // `post_fields` (write + post) is needed only on change.
                    if new_dov != grp.dov {
                        let _ = self
                            .post_fields(
                                &rec_name,
                                vec![(DO_NAMES[idx].to_string(), EpicsValue::Double(new_dov))],
                            )
                            .await;
                    }
                }
            }
        }

        // dfanout (pre-commit phase): return the single pending alarm C
        // `push_values` would have raised this cycle for the caller to fold
        // into `nsev` before `recGblResetAlarms`. C raises at most one:
        // SOFT_ALARM/INVALID for a Specified `seln` out of range (line 317,
        // no push) OR LINK_ALARM/MAJOR for a failed dbPutLink (line
        // 312/324/333). They are mutually exclusive in C's control flow; if
        // both were somehow set, fold the higher severity (recGblSetSevr is
        // raise-only). fanout/seq already applied their range alarm directly
        // above and drive no value, so they return None.
        if is_dfanout {
            let link_alarm = if link_failed {
                Some((
                    crate::server::recgbl::alarm_status::LINK_ALARM,
                    AlarmSeverity::Major,
                ))
            } else {
                None
            };
            return match (dfanout_selm_alarm, link_alarm) {
                (Some(a), Some(b)) => Some(if (a.1 as u16) >= (b.1 as u16) { a } else { b }),
                (a, b) => a.or(b),
            };
        }
        None
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
                Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
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
    /// `passive_only` is `true` for a CPP edge (process the target only when
    /// its `SCAN` is Passive) and `false` for CP (always process). When the
    /// same source→target edge is registered from both a CP and a CPP link,
    /// CP dominates: the merged edge keeps `passive_only == false`, matching
    /// C, where an unconditional CP `CA_DBPROCESS` overrides any CPP gate on
    /// the same record.
    pub async fn register_cp_link(
        &self,
        source_record: &str,
        target_record: &str,
        passive_only: bool,
    ) {
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
        if let Some(existing) = targets.iter_mut().find(|t| t.record == target) {
            existing.passive_only = existing.passive_only && passive_only;
        } else {
            targets.push(super::CpTarget {
                record: target,
                passive_only,
            });
        }
    }

    /// Get target edges to process when `source_record` changes (CP/CPP links).
    pub async fn get_cp_targets(&self, source_record: &str) -> Vec<super::CpTarget> {
        self.inner
            .cp_links
            .read()
            .await
            .get(source_record)
            .cloned()
            .unwrap_or_default()
    }

    /// Register an EXTERNAL CP/CPP link: when the remote PV `external_pv`
    /// (a cross-IOC CA/PVA source, e.g. `OTHER:PV` from
    /// `INP="OTHER:PV CP"`) changes, process `target_record`.
    ///
    /// Twin of [`Self::register_cp_link`] for cross-IOC sources. The key is
    /// the **scheme-stripped external PV name** — it is not a local record,
    /// so it is NOT alias-resolved. It MUST equal the name the calink/pvalink
    /// monitor dispatches under: that monitor is opened through the lset,
    /// which strips the `ca://` / `pva://` scheme first, so its `pv_name`
    /// (passed to [`Self::dispatch_external_cp_targets`]) is the bare PV.
    /// Stripping the same schemes here — the set [`Self::resolve_external_pv`]
    /// already knows — guarantees the registry key and the dispatch key can
    /// never diverge. The `target_record` (the local holder) IS alias-resolved
    /// so the dispatch processes the canonical record. CP dominates CPP on a
    /// merged edge, identical to the local path.
    pub async fn register_external_cp_link(
        &self,
        external_pv: &str,
        target_record: &str,
        passive_only: bool,
    ) {
        let key = external_pv
            .strip_prefix("ca://")
            .or_else(|| external_pv.strip_prefix("pva://"))
            .unwrap_or(external_pv);
        let target = self
            .resolve_alias(target_record)
            .await
            .unwrap_or_else(|| target_record.to_string());
        let mut cp = self.inner.external_cp_links.write().await;
        let targets = cp.entry(key.to_string()).or_default();
        if let Some(existing) = targets.iter_mut().find(|t| t.record == target) {
            existing.passive_only = existing.passive_only && passive_only;
        } else {
            targets.push(super::CpTarget {
                record: target,
                passive_only,
            });
        }
    }

    /// Get holder edges to process when the remote PV `external_pv` changes
    /// (external CA/PVA CP/CPP links).
    pub async fn get_external_cp_targets(&self, external_pv: &str) -> Vec<super::CpTarget> {
        self.inner
            .external_cp_links
            .read()
            .await
            .get(external_pv)
            .cloned()
            .unwrap_or_default()
    }

    /// Every external PV name that has at least one CP/CPP holder edge.
    /// The calink/pvalink integration warms (opens a monitor for) each of
    /// these at iocInit so the remote source has a live subscription whose
    /// callback can drive [`Self::dispatch_external_cp_targets`] — a Passive
    /// CP holder is otherwise never read and its monitor never opens.
    pub async fn external_cp_pv_names(&self) -> Vec<String> {
        self.inner
            .external_cp_links
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }

    /// Classify one parsed CP/CPP link, deciding the local-vs-external
    /// routing — this method is the single owner of that decision, so the
    /// CP trigger registry and the holder's value-read parse cache stay
    /// consistent.
    ///
    /// A link with no CP/CPP policy (`cp_passive_only() == None`), and
    /// every non-`Db`/`Ca` variant, is ignored.
    ///
    /// `Ca`: an external `ca://OTHER CP` holder always drives the cross-IOC
    /// path — C `dbCa.c` `eventCallback` adds `CA_DBPROCESS` for every CP link
    /// (`dbCa.c:993-994`). Note `OTHER CP CA` is NOT such a link: `CA` and `CP`
    /// are mutually exclusive process classes and `CA` is matched first
    /// (`dbStaticLib.c:2369-2373`), so it is a plain CA link with no CP policy.
    ///
    /// `Db`: C `dbInitLink` (`dbLink.c:118-130`) makes any link carrying a
    /// `CP`/`CPP`/`CA` option a CA link, and `dbDbInitLink` keeps it local
    /// only when the named record exists in this IOC. So a CP/CPP `Db`
    /// link whose target is NOT a local record (e.g. `INP="other:pv CP"`,
    /// no explicit `CA`) must be served externally — both the calink
    /// trigger monitor (`ext_links`) AND the holder's value read. The read
    /// half is the `convert_to_ca` entry: it carries the field name and an
    /// equivalent [`CaLink`] so the caller rewrites the holder's cached
    /// parse from `Db` to `Ca`, routing the read through the external
    /// resolver. A local target keeps the local fast-path (`db_links`).
    async fn classify_cp_link(
        &self,
        field: &str,
        parsed: crate::server::record::ParsedLink,
        target_name: &str,
        db_links: &mut Vec<(String, String, bool)>,
        ext_links: &mut Vec<(String, String, bool)>,
        convert_to_ca: &mut Vec<(String, crate::server::record::CaLink)>,
    ) {
        match parsed {
            crate::server::record::ParsedLink::Db(db) => {
                let Some(passive_only) = db.policy.cp_passive_only() else {
                    return;
                };
                if self.has_name_no_resolve(&db.record).await {
                    db_links.push((db.record, target_name.to_string(), passive_only));
                } else {
                    // Reconstruct the CA channel name verbatim: `record`
                    // for a default-`VAL` link, `record.FIELD` otherwise —
                    // the same string the `CA`-modifier path would store
                    // in `CaLink::pv`.
                    let pv = if db.field == "VAL" {
                        db.record.clone()
                    } else {
                        format!("{}.{}", db.record, db.field)
                    };
                    ext_links.push((pv.clone(), target_name.to_string(), passive_only));
                    convert_to_ca.push((
                        field.to_string(),
                        crate::server::record::CaLink {
                            pv,
                            monitor_switch: db.monitor_switch,
                            policy: db.policy,
                        },
                    ));
                }
            }
            crate::server::record::ParsedLink::Ca(ca) => {
                if let Some(passive_only) = ca.policy.cp_passive_only() {
                    ext_links.push((ca.pv, target_name.to_string(), passive_only));
                }
            }
            _ => {}
        }
    }

    /// Scan all records for CP/CPP input links and register them. Local
    /// `Db` links land in the local CP registry; external `Ca` links land
    /// in the external CP registry, whose holders are processed by the
    /// calink monitor callback.
    pub async fn setup_cp_links(&self) {
        let names = self.all_record_names().await;
        let mut db_links: Vec<(String, String, bool)> = Vec::new();
        let mut ext_links: Vec<(String, String, bool)> = Vec::new();

        for target_name in &names {
            // Enumerate this record's link-bearing fields through the
            // single shared owner (`record_link_fields`) so the CA CP/CPP
            // setup here and the pvalink install scan can never diverge on
            // which fields count as links. `classify_cp_link` keeps only
            // the CP/CPP-policy links, routes local `Db` vs external `Ca`,
            // and — for a CP/CPP `Db` link to a non-local target — emits a
            // `convert_to_ca` entry so the holder's value read is rewired
            // to the external resolver (C `dbLink.c:118-130`).
            let mut convert_to_ca: Vec<(String, crate::server::record::CaLink)> = Vec::new();
            for (field, _raw, parsed) in self.record_link_fields(target_name).await {
                self.classify_cp_link(
                    &field,
                    parsed,
                    target_name,
                    &mut db_links,
                    &mut ext_links,
                    &mut convert_to_ca,
                )
                .await;
            }
            // Apply the `Db`→`Ca` parse-cache rewrite for non-local CP/CPP
            // links so the holder reads its value through the external
            // resolver — consistent with the external CP trigger registered
            // above (both halves use the same locality decision made in
            // `classify_cp_link`). Only the common-field caches are
            // rewritten; INPA.. / DOL.. links are re-parsed from their raw
            // strings each process cycle and so are not covered here.
            if !convert_to_ca.is_empty() {
                if let Some(rec_arc) = self.get_record(target_name).await {
                    let mut inst = rec_arc.write().await;
                    for (field, calink) in convert_to_ca {
                        let link = crate::server::record::ParsedLink::Ca(calink);
                        match field.as_str() {
                            "INP" => inst.parsed_inp = link,
                            "OUT" => inst.parsed_out = link,
                            "TSEL" => inst.parsed_tsel = link,
                            "SDIS" => inst.parsed_sdis = link,
                            _ => {}
                        }
                    }
                }
            }
        }

        let db_count = db_links.len();
        for (source, target, passive_only) in db_links {
            self.register_cp_link(&source, &target, passive_only).await;
        }
        let ext_count = ext_links.len();
        for (external_pv, target, passive_only) in ext_links {
            self.register_external_cp_link(&external_pv, &target, passive_only)
                .await;
        }
        if db_count > 0 {
            eprintln!("iocInit: {db_count} CP link subscriptions");
        }
        // Warm external CP/CPP links. A Passive holder of an external CP
        // link is never scanned, so its link never opens lazily and the
        // calink monitor that drives `dispatch_external_cp_targets` is never
        // created (chicken-and-egg). Open each external CP PV now so its
        // monitor is live at iocInit — the C `dbCa.c` "add the link at init"
        // analogue (`dbCaAddLink`). `resolve_external_pv` routes through the
        // registered lset's lazy-open path (the same path a record read
        // uses); a no-op when no matching lset is installed (calink off).
        if ext_count > 0 {
            let ext_pvs = self.external_cp_pv_names().await;
            for pv in &ext_pvs {
                let _ = self.resolve_external_pv(pv).await;
            }
            eprintln!(
                "iocInit: {ext_count} external CP link subscriptions ({} PVs warmed)",
                ext_pvs.len()
            );
        }
    }
}

#[cfg(test)]
mod out_link_put_fail_tests {
    use super::{LinkAlarm, OutLinkSrc};
    use crate::server::database::PvDatabase;
    use crate::server::record::{AlarmSeverity, DbLink, LinkProcessPolicy, MonitorSwitch};
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;
    use std::collections::HashSet;

    /// C `dbDbPutValue` (dbDbLink.c:382-390) runs `dbPut`, folds the
    /// source alarm via `recGblInheritSevrMsg`, then `if (status) return
    /// status;` — only a *successful* write reaches the `.PROC`/`PP`
    /// `processTarget` branch. A failed OUT-link write must therefore NOT
    /// process the target. The failure here is a string that names no choice
    /// of a `DBF_MENU` field — C `dbPut` → `putStringMenu` → `S_db_badChoice`.
    ///
    /// Observable: a passive `calc` target with `CALC = "7"` evaluates to
    /// `VAL = 7` on process and stays at its `Default` `VAL = 0` when not
    /// processed. The PP OUT link below carries a write that fails, so the
    /// code returns before `processTarget` and `VAL` stays `0.0`; processing
    /// the target unconditionally would make it `7.0`.
    #[tokio::test]
    async fn pp_out_link_failed_write_does_not_process_target() {
        let db = PvDatabase::new();
        db.add_record("TGT", Box::new(CalcRecord::new("7")))
            .await
            .unwrap();

        // Precondition: CALC not yet evaluated, VAL at its Default 0.0.
        assert!(
            matches!(db.get_pv("TGT.VAL").await.unwrap(), EpicsValue::Double(v) if v == 0.0),
            "calc VAL must start at its Default 0.0 before any process"
        );

        let link = DbLink {
            record: "TGT".to_string(),
            field: "PINI".to_string(),
            policy: LinkProcessPolicy::ProcessPassive, // ` PP` token
            monitor_switch: MonitorSwitch::NoMaximize,
        };
        let alarm = LinkAlarm {
            stat: 0,
            sevr: AlarmSeverity::NoAlarm,
            amsg: String::new(),
        };
        let src = OutLinkSrc {
            putf: false,
            notify: None,
            alarm: &alarm,
        };
        let mut visited = HashSet::new();

        db.write_db_link_value(
            &link,
            EpicsValue::String("NOT_A_MENU_CHOICE".into()),
            src,
            &mut visited,
            0,
        )
        .await;

        // The failed write short-circuits before processTarget, so CALC was
        // never evaluated and VAL is still its Default 0.0.
        assert!(
            matches!(db.get_pv("TGT.VAL").await.unwrap(), EpicsValue::Double(v) if v == 0.0),
            "a failed OUT-link write must NOT process the PP target \
             (VAL must stay 0.0, not become 7.0)"
        );
    }

    /// R6-7 — an empty array into a scalar field is NOT a failed write. C
    /// `dbPut` (`dbAccess.c:1370-1372`) writes nothing, raises
    /// `LINK_ALARM`/`INVALID_ALARM` on the destination and returns 0, so
    /// `dbDbPutValue`'s `if (status) return status;` does not fire and a ` PP`
    /// link goes on to process its target.
    ///
    /// The port used to reject the put, which suppressed both the destination's
    /// alarm and its `PP` processing.
    #[tokio::test]
    async fn pp_out_link_empty_array_alarms_target_and_still_processes_it() {
        use crate::server::recgbl::alarm_status;

        let db = PvDatabase::new();
        db.add_record("ETGT", Box::new(CalcRecord::new("7")))
            .await
            .unwrap();

        let link = DbLink {
            record: "ETGT".to_string(),
            field: "VAL".to_string(),
            policy: LinkProcessPolicy::ProcessPassive, // ` PP` token
            monitor_switch: MonitorSwitch::NoMaximize,
        };
        let alarm = LinkAlarm {
            stat: 0,
            sevr: AlarmSeverity::NoAlarm,
            amsg: String::new(),
        };
        let src = OutLinkSrc {
            putf: false,
            notify: None,
            alarm: &alarm,
        };
        let mut visited = HashSet::new();

        db.write_db_link_value(&link, EpicsValue::DoubleArray(vec![]), src, &mut visited, 0)
            .await;

        // The put succeeded (status 0), so the PP target processed: CALC = "7".
        assert!(
            matches!(db.get_pv("ETGT.VAL").await.unwrap(), EpicsValue::Double(v) if v == 7.0),
            "an empty-array put is accepted by C, so the PP target must process"
        );
        // …and the destination carries LINK/INVALID, committed by that very
        // process cycle's `recGblResetAlarms`.
        let inst = db.get_record("ETGT").await.unwrap();
        let inst = inst.read().await;
        assert_eq!(inst.common.stat, alarm_status::LINK_ALARM);
        assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
    }
}

#[cfg(test)]
mod nonlocal_db_link_write_tests {
    use super::OutLinkSrc;
    use crate::server::database::{LinkPutOp, LinkSet, PvDatabase};
    use crate::server::record::{AlarmSeverity, DbLink, LinkProcessPolicy, MonitorSwitch};
    use crate::server::records::calc::CalcRecord;
    use crate::types::EpicsValue;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    /// A link set that records every `put_value` it receives so a test
    /// can assert a non-local OUT-link write reached the external (CA)
    /// put path instead of being dropped by a local `dbPut`.
    struct RecordingLset {
        puts: Arc<Mutex<Vec<(String, EpicsValue)>>>,
    }
    #[async_trait::async_trait]
    impl LinkSet for RecordingLset {
        async fn is_connected(&self, _: &str) -> bool {
            true
        }
        async fn get_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn put_value(
            &self,
            name: &str,
            value: EpicsValue,
            _op: LinkPutOp,
        ) -> Result<(), String> {
            self.puts.lock().unwrap().push((name.to_string(), value));
            Ok(())
        }
    }

    fn out_src(alarm: &super::LinkAlarm) -> OutLinkSrc<'_> {
        OutLinkSrc {
            putf: false,
            notify: None,
            alarm,
        }
    }

    fn no_alarm() -> super::LinkAlarm {
        super::LinkAlarm {
            stat: 0,
            sevr: AlarmSeverity::NoAlarm,
            amsg: String::new(),
        }
    }

    /// A plain OUT link whose target record is NOT local must write
    /// through the external put path — C `dbPutLink` routes a non-local
    /// (CA) link's write to `dbCaPutLink`, never a local `dbPut`. The
    /// pre-fix code called `put_pv_already_locked` on a name that does
    /// not exist locally; the write was silently dropped. The recording
    /// lset must now capture the value.
    #[tokio::test]
    async fn nonlocal_db_out_link_writes_through_external_put() {
        let db = PvDatabase::new();
        let puts = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set("ca", Arc::new(RecordingLset { puts: puts.clone() }))
            .await;

        // "OTHER:PV" is never added as a local record -> non-local.
        let link = DbLink {
            record: "OTHER:PV".to_string(),
            field: "VAL".to_string(),
            policy: LinkProcessPolicy::NoProcess,
            monitor_switch: MonitorSwitch::NoMaximize,
        };
        let alarm = no_alarm();
        let mut visited = HashSet::new();
        db.write_db_link_value(
            &link,
            EpicsValue::Double(42.0),
            out_src(&alarm),
            &mut visited,
            0,
        )
        .await;

        let captured = puts.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "non-local OUT-link write must reach the external put path exactly once"
        );
        assert_eq!(captured[0].0, "OTHER:PV");
        assert!(matches!(captured[0].1, EpicsValue::Double(v) if v == 42.0));
    }

    /// A local OUT-link write must NOT divert to the external put path —
    /// the locality dispatch only reroutes non-local targets. The local
    /// record receives the value; the lset records nothing.
    #[tokio::test]
    async fn local_db_out_link_writes_local_not_external() {
        let db = PvDatabase::new();
        let puts = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set("ca", Arc::new(RecordingLset { puts: puts.clone() }))
            .await;
        db.add_record("TGT", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();

        let link = DbLink {
            record: "TGT".to_string(),
            field: "VAL".to_string(),
            policy: LinkProcessPolicy::NoProcess,
            monitor_switch: MonitorSwitch::NoMaximize,
        };
        let alarm = no_alarm();
        let mut visited = HashSet::new();
        db.write_db_link_value(
            &link,
            EpicsValue::Double(7.0),
            out_src(&alarm),
            &mut visited,
            0,
        )
        .await;

        assert!(
            puts.lock().unwrap().is_empty(),
            "a local OUT-link write must not reach the external put path"
        );
        assert!(
            matches!(db.get_pv("TGT.VAL").await.unwrap(), EpicsValue::Double(v) if v == 7.0),
            "the local target must hold the written value"
        );
    }

    /// A link set that records every `scan_forward` it receives so a test
    /// can assert a non-DB FLNK reached the external forward path
    /// (C `dbScanFwdLink` → `lset->scanForward`) instead of being dropped
    /// by the DB-only `flnk_name` filter. `connected=false` models a
    /// disconnected link (pvxs `pvaScanForward`'s `!valid()` gate).
    struct ForwardingLset {
        forwards: Arc<Mutex<Vec<String>>>,
        connected: bool,
    }
    #[async_trait::async_trait]
    impl LinkSet for ForwardingLset {
        async fn is_connected(&self, _: &str) -> bool {
            self.connected
        }
        async fn get_value(&self, _: &str) -> Option<EpicsValue> {
            None
        }
        async fn scan_forward(&self, name: &str) -> Result<(), String> {
            self.forwards.lock().unwrap().push(name.to_string());
            if self.connected {
                Ok(())
            } else {
                Err("Disconn".into())
            }
        }
    }

    /// `scan_forward_external_pv` must resolve the scheme and delegate to
    /// the registered lset's `scan_forward` — the FWD-link twin of
    /// `write_external_pv` for OUT writes (C `dbScanFwdLink` →
    /// `lset->scanForward`).
    #[tokio::test]
    async fn external_forward_link_dispatches_through_scan_forward() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: true,
            }),
        )
        .await;

        db.scan_forward_external_pv("pva://OTHER:PROC")
            .await
            .expect("a connected forward must succeed");

        let captured = forwards.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], "OTHER:PROC");
    }

    /// End-to-end: a record whose FLNK targets an external `pva://` PV
    /// must fire the link set's `scan_forward` when it processes — the
    /// non-DB FLNK dispatch the DB-only `flnk_name` filter previously
    /// dropped. C `recGblFwdLink` runs `dbScanFwdLink` for every FLNK
    /// regardless of link kind.
    #[tokio::test]
    async fn record_processing_fires_external_forward_link() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: true,
            }),
        )
        .await;
        db.add_record("SRC", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        if let Some(rec) = db.get_record("SRC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("pva://TARGET".into()))
                .unwrap();
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("SRC", &mut visited, 0)
            .await
            .unwrap();

        let captured = forwards.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "an external pva:// FLNK must fire scan_forward exactly once"
        );
        assert_eq!(captured[0], "TARGET");
    }

    /// A disconnected non-retry external FLNK is NOT dropped silently: the
    /// lset returns Err and the owning record takes a *pending*
    /// LINK/INVALID alarm — pvxs `pvaScanForward`'s
    /// `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "Disconn")`, which
    /// writes `nsta`/`nsev`/`namsg` (promoted by the next
    /// `recGblResetAlarms`, matching the C late-set inside `recGblFwdLink`).
    #[tokio::test]
    async fn disconnected_external_forward_link_raises_pending_link_invalid() {
        let db = PvDatabase::new();
        let forwards = Arc::new(Mutex::new(Vec::new()));
        db.register_link_set(
            "pva",
            Arc::new(ForwardingLset {
                forwards: forwards.clone(),
                connected: false,
            }),
        )
        .await;
        db.add_record("SRC", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        if let Some(rec) = db.get_record("SRC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("pva://TARGET".into()))
                .unwrap();
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("SRC", &mut visited, 0)
            .await
            .unwrap();

        assert_eq!(
            forwards.lock().unwrap().len(),
            1,
            "scan_forward is still attempted on a disconnected link"
        );
        let rec = db.get_record("SRC").await.unwrap();
        let inst = rec.read().await;
        assert_eq!(inst.common.nsev, AlarmSeverity::Invalid);
        assert_eq!(
            inst.common.nsta,
            crate::server::recgbl::alarm_status::LINK_ALARM
        );
        assert_eq!(inst.common.namsg, "Disconn");
    }
}

#[cfg(test)]
mod cp_link_locality_tests {
    use crate::server::database::PvDatabase;
    use crate::server::record::ParsedLink;
    use crate::server::records::ai::AiRecord;

    /// A CP/CPP link with no explicit `CA` modifier whose target is NOT a
    /// local record must be served as an external (CA) link — both the
    /// trigger and the value read. C `dbInitLink` (`dbLink.c:118-130`)
    /// makes any `CP`/`CPP` link a CA link, and `dbDbInitLink` keeps it
    /// local only when the named record exists in this IOC.
    ///
    /// Here `INP="OTHER:PV CP"` parses to `ParsedLink::Db` (bare name, no
    /// `CA`), but `OTHER:PV` is not local. After `setup_cp_links` the
    /// holder's `parsed_inp` must be rewritten to `ParsedLink::Ca` (so the
    /// read routes through `resolve_external_pv`) and `OTHER:PV` must be
    /// registered as an external CP trigger.
    #[tokio::test]
    async fn cp_link_to_nonlocal_target_forced_external() {
        let db = PvDatabase::new();
        db.add_record("HOLDER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("HOLDER").await.unwrap();
            rec.write().await.common.inp = "OTHER:PV CP".to_string();
        }

        db.setup_cp_links().await;

        let inp = db
            .get_record("HOLDER")
            .await
            .unwrap()
            .read()
            .await
            .parsed_inp
            .clone();
        match inp {
            ParsedLink::Ca(ca) => assert_eq!(
                ca.pv, "OTHER:PV",
                "non-local CP link must carry the verbatim PV name"
            ),
            other => panic!("non-local CP link must be forced to Ca, got {other:?}"),
        }
        assert!(
            db.external_cp_pv_names()
                .await
                .contains(&"OTHER:PV".to_string()),
            "non-local CP link must be registered as an external CP trigger"
        );
    }

    /// A CP link whose target IS a local record keeps the local fast-path:
    /// `setup_cp_links` must NOT rewrite its `parsed_inp` to `Ca` and must
    /// NOT register it as an external CP link.
    #[tokio::test]
    async fn cp_link_to_local_target_stays_db() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("HOLDER", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        {
            let rec = db.get_record("HOLDER").await.unwrap();
            let mut inst = rec.write().await;
            inst.common.inp = "SRC CP".to_string();
            // Seed the parse cache as load would, so the assertion proves
            // setup_cp_links leaves a local CP link untouched.
            inst.parsed_inp = crate::server::record::parse_link_v2("SRC CP");
        }

        db.setup_cp_links().await;

        let inp = db
            .get_record("HOLDER")
            .await
            .unwrap()
            .read()
            .await
            .parsed_inp
            .clone();
        assert!(
            matches!(inp, ParsedLink::Db(_)),
            "local CP link must stay a Db link, got {inp:?}"
        );
        assert!(
            !db.external_cp_pv_names().await.contains(&"SRC".to_string()),
            "a local CP target must not be registered as an external CP link"
        );
    }
}

#[cfg(test)]
mod nonlocal_db_link_read_tests {
    use crate::server::database::PvDatabase;
    use crate::server::record::{ParsedLink, parse_link_v2};
    use crate::types::EpicsValue;
    use std::collections::HashSet;
    use std::sync::Arc;

    /// C `dbInitLink` (`dbLink.c:118-130`) makes a PV link whose target is
    /// not a local record a CA link — even with NO `CP`/`CPP`/`CA`
    /// modifier (`dbDbInitLink` fails to resolve it locally and falls
    /// through to `dbCaAddLinkCallbackOpt`). So a plain `INP="OTHER:PV"`
    /// to a non-local target must read the remote value, not return
    /// `None`. Pre-fix the `Db` read arm read only the local DB (`get_pv`)
    /// and dropped the non-local target.
    #[tokio::test]
    async fn plain_nonlocal_db_link_reads_via_external_resolver() {
        let db = PvDatabase::new();
        // Stand-in for the calink/pvalink lset: resolve OTHER:PV remotely.
        db.set_external_resolver(Arc::new(|name: &str| {
            let hit = name == "OTHER:PV";
            Box::pin(async move {
                if hit {
                    Some(EpicsValue::Double(42.0))
                } else {
                    None
                }
            })
        }))
        .await;

        let link = parse_link_v2("OTHER:PV");
        assert!(
            matches!(link, ParsedLink::Db(_)),
            "a bare non-scheme name parses to a Db link, got {link:?}"
        );

        let mut visited = HashSet::new();
        let v = db.read_link_value(&link, &mut visited, 0).await;
        assert_eq!(
            v,
            Some(EpicsValue::Double(42.0)),
            "a plain non-local Db link must resolve through the external resolver (C dbCaAddLink fallback)"
        );
    }

    /// A multi-input-style link re-parsed from its raw string each cycle
    /// (calc `INPA`..`INPL`, `DOL`) routes its value read through
    /// `read_link_with_alarm`; the same locality rule must hold there, so
    /// a non-local target reads the remote value and surfaces no
    /// local-record alarm.
    #[tokio::test]
    async fn nonlocal_db_link_value_and_alarm_via_external() {
        let db = PvDatabase::new();
        db.set_external_resolver(Arc::new(|name: &str| {
            let hit = name == "OTHER:PV";
            Box::pin(async move {
                if hit {
                    Some(EpicsValue::Double(5.0))
                } else {
                    None
                }
            })
        }))
        .await;

        let link = parse_link_v2("OTHER:PV");
        let (value, alarm) = db.read_link_with_alarm(&link).await;
        assert_eq!(
            value,
            Some(EpicsValue::Double(5.0)),
            "non-local Db link value must come from the external resolver"
        );
        // No lset is registered (only the legacy resolver), so the bare
        // external alarm path reports None — never a fabricated local
        // record alarm for a record that does not exist in this IOC.
        assert!(
            alarm.is_none(),
            "non-local Db link must not fabricate a local-record alarm, got {alarm:?}"
        );
    }

    /// Owner path: a Db link whose target IS a local record still reads
    /// the local database and never consults the external resolver.
    #[tokio::test]
    async fn local_db_link_reads_local_not_external() {
        let db = PvDatabase::new();
        db.add_pv("SRC", EpicsValue::Double(7.0)).await.unwrap();
        // A resolver returning a DIFFERENT value proves the local read
        // wins and the external path is not taken for a local target.
        db.set_external_resolver(Arc::new(|_name: &str| {
            Box::pin(async { Some(EpicsValue::Double(-1.0)) })
        }))
        .await;

        let link = parse_link_v2("SRC");
        let mut visited = HashSet::new();
        let v = db.read_link_value(&link, &mut visited, 0).await;
        assert_eq!(
            v,
            Some(EpicsValue::Double(7.0)),
            "a local Db link must read the local DB, not the external resolver"
        );
    }
}
