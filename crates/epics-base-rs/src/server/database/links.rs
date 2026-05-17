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
    pub(crate) async fn read_link_value(
        &self,
        link: &crate::server::record::ParsedLink,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::None => None,
            crate::server::record::ParsedLink::Ca(name)
            | crate::server::record::ParsedLink::Pva(name) => self.resolve_external_pv(name).await,
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) => {
                // PP: process source record if Passive before reading
                if db.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
                    if let Some(src) = self.get_record(&db.record).await {
                        let is_passive = src.read().await.common.scan
                            == crate::server::record::ScanType::Passive;
                        if is_passive {
                            let mut visited = std::collections::HashSet::new();
                            let _ = self
                                .process_record_with_links(&db.record, &mut visited, 0)
                                .await;
                        }
                    }
                }
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
            // cached snapshot, and the alarm severity comes from the
            // lset's `alarm_severity` accessor. The lset has already
            // applied the link's `MS`/`NMS`/`MSI` mode gate (that
            // modifier is stripped from the link string before
            // epics-base-rs parses it), so a returned `Some(sev)` is
            // propagated verbatim as a maximize-severity contribution.
            // Without this, a connected pva link carrying a remote
            // MINOR/MAJOR severity never folded into the owning
            // record's LINK_ALARM (B2).
            crate::server::record::ParsedLink::Pva(name)
            | crate::server::record::ParsedLink::Ca(name) => {
                let value = self.resolve_external_pv(name).await;
                let alarm = self.external_link_alarm(name).await;
                (value, alarm)
            }
            _ => (None, None),
        }
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
                            stat: crate::server::recgbl::alarm_status::LINK_ALARM,
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
            stat: crate::server::recgbl::alarm_status::LINK_ALARM,
            sevr: crate::server::record::AlarmSeverity::from_u16(sev as u16),
            amsg: lset.alarm_message(body).unwrap_or_default(),
        })
    }

    /// Read a value from a parsed link for INP (only reads DB links when soft channel).
    pub async fn read_link_value_soft(
        &self,
        link: &crate::server::record::ParsedLink,
        is_soft: bool,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) if is_soft => {
                // PP: process source record if Passive before reading
                if db.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
                    if let Some(src) = self.get_record(&db.record).await {
                        let is_passive = src.read().await.common.scan
                            == crate::server::record::ScanType::Passive;
                        if is_passive {
                            let mut visited = std::collections::HashSet::new();
                            let _ = self
                                .process_record_with_links(&db.record, &mut visited, 0)
                                .await;
                        }
                    }
                }
                let pv_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                self.get_pv(&pv_name).await.ok()
            }
            crate::server::record::ParsedLink::Ca(name)
            | crate::server::record::ParsedLink::Pva(name)
                if is_soft =>
            {
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
        let _ = self.put_pv(&target_name, value).await;

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
                    let _ = self
                        .process_record_with_links(&link.record, visited, depth + 1)
                        .await;
                }
            }
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
                    let sel = select_link_indices_ex(
                        SelmKind::Dfanout,
                        selm,
                        seln,
                        0,
                        0,
                        links.len(),
                    );
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
                    let offs = Self::field_i16(&instance, "OFFS");
                    let shft = Self::field_i16(&instance, "SHFT");
                    // sseq keeps the legacy 10-group 1-based layout —
                    // DOL1..DOLA / LNK1..LNKA with DO/STR value storage.
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
                        let _ = self
                            .process_record_with_links(&db.record, visited, depth + 1)
                            .await;
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
                        if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                            // C `dfanoutRecord.c:323` drives each OUTn via
                            // `dbPutLink`, whose `DBF_OUTLINK` target is
                            // processed by `dbDbPutLink` only when the link
                            // carries an explicit `PP` modifier
                            // (`dbDbLink.c:415` — `pvlMask & ln`). The C
                            // default for an out-link with no modifier is
                            // NPP: the value is written but the target is
                            // NOT processed. `parse_link_v2` defaults a
                            // bare link to `ProcessPassive`, so without
                            // this correction a bare `OUTn` would re-process
                            // the target — and a Soft-Channel ai target's
                            // `convert()` would then clobber the value just
                            // written. Honour C: process the dfanout OUTn
                            // target only on an explicit `PP` token.
                            let explicit_pp = link_has_explicit_pp(link_str);
                            let mut db = db.clone();
                            if !explicit_pp
                                && db.policy
                                    == crate::server::record::LinkProcessPolicy::ProcessPassive
                            {
                                db.policy = crate::server::record::LinkProcessPolicy::NoProcess;
                            }
                            self.write_db_link_value(&db, val.clone(), src_putf, visited, depth)
                                .await;
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
                        self.read_link_value(&dol_parsed).await
                    } else {
                        Some(EpicsValue::Double(grp.dov))
                    };
                    if let Some(value) = value {
                        let lnk_parsed = crate::server::record::parse_link_v2(&grp.lnk);
                        if let crate::server::record::ParsedLink::Db(ref db) = lnk_parsed {
                            self.write_db_link_value(db, value, src_putf, visited, depth)
                                .await;
                        }
                    }
                }
            }
            MultiOut::Sseq(groups) => {
                for idx in indices {
                    let grp = &groups[idx];
                    if grp.lnk.is_empty() {
                        continue;
                    }
                    // Determine value: read from DOL link, or use DO/STR field
                    let value = if !grp.dol.is_empty() {
                        let dol_parsed = crate::server::record::parse_link_v2(&grp.dol);
                        self.read_link_value(&dol_parsed).await
                    } else if !grp.str_val.is_empty() {
                        Some(EpicsValue::String(grp.str_val.clone()))
                    } else {
                        Some(EpicsValue::Double(grp.do_val))
                    };
                    if let Some(value) = value {
                        let lnk_parsed = crate::server::record::parse_link_v2(&grp.lnk);
                        if let crate::server::record::ParsedLink::Db(ref db) = lnk_parsed {
                            self.write_db_link_value(db, value, src_putf, visited, depth)
                                .await;
                        }
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
