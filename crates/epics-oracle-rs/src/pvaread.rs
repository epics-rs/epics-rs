//! The PVA **read** phase: every channel of the enumerated surface, read on
//! both sides, on two separate contracts.
//!
//! # The denominator, and why it is the CA one
//!
//! QSRV2 does not project per record type, and it does not keep a namespace of
//! its own. `SingleSource::onSearch` hands the channel name straight to base
//! (`pvxs/ioc/singlesource.cpp:469`):
//!
//! ```text
//! for (auto& pv: searchOperation) {
//!     if (!dbChannelTest(pv.name())) pv.claim();
//! }
//! ```
//!
//! So the PVA namespace **is** base's `dbChannel` namespace — `RECORD` (which
//! resolves to `.VAL`) and `RECORD.FIELD` — which is exactly the set
//! [`crate::surface`] already enumerates from the `.dbd` for the CA phases.
//! The PVA denominator is therefore not a new number to invent: it is the same
//! `record.FIELD` set, measured against a different ground truth
//! (`softIocPVX`) through a different instrument (`pvxget`/`pvxinfo`).
//!
//! `DBF_NOACCESS` fields stay excluded, as they are for CA, and here the
//! exclusion is if anything sharper: `fromDbrType(DBR_NOACCESS)` is
//! `TypeCode::Null`, pvxs cannot build an NT of `Null`, and `onCreate`
//! consequently refuses the channel — measured: `Server 127.0.0.1:36683 refuses
//! channel to 'ORACLE:AI.MLOK' : Refused to create Channel`.
//!
//! # Two contracts per channel, kept apart on purpose
//!
//! `pvxget`'s default output is **Delta** format: it prints only the fields the
//! reply *marks*. So a single `pvxget` diff cannot tell two different defects
//! apart — a field the port marks and QSRV2 does not reads exactly like a field
//! QSRV2 does not declare. Both are real, and both are present in the port, but
//! they have different fixes. So each channel carries two contracts:
//!
//! - [`PvaSurface::DeclaredType`] — `pvxinfo`, the declared type with no value.
//! - [`PvaSurface::ValueMarking`] — `pvxget`, the value and what the reply marked.
//!
//! A type gap can then never hide inside a value diff, and a marking difference
//! is never attributed to the type.
//!
//! **A channel is measured only when both contracts ran on both sides.** Either
//! one failing to run makes the channel [`Verdict::Errored`] — never agreement,
//! and never coverage.
//!
//! # The third thing each channel is checked against: the `.dbd`
//!
//! C-vs-port alone says *that* two blobs of text differ, not *which side is
//! wrong*. Where the `.dbd` entails the NT shape ([`crate::ntshape`]), it is
//! derived up front and each side is checked against it independently. A wrong
//! shape is then attributable: [`PvaSurface::PortShapeVsDbd`] indicts the port,
//! while [`PvaSurface::GroundTruthShapeVsDbd`] indicts **this harness's own
//! derivation** — pvxs is ground truth, so if it disagrees with the prediction,
//! the prediction is what is wrong.
//!
//! That is not decoration; it has already fired and paid for itself. The
//! derivation predicted `NTEnum` for `mbbo.VAL` from its `DBF_ENUM` declaration,
//! and both sides answered `NTScalar{uint16_t}` — because `mbbo`'s `cvt_dbaddr`
//! overrides the declared type in C the `.dbd` does not describe. Reported as a
//! bare C-vs-port text diff, that would have been an unattributable defect
//! against a port that was right; as a named `GroundTruthShapeVsDbd` finding it
//! pointed straight at the harness. [`NtShape::expected`] now declines to predict
//! for the fields the `.dbd` marks `special(SPC_DBADDR)`, and the two shape
//! contracts are simply not raised for them — the two cross-side contracts still
//! are, so nothing goes unmeasured.
//!
//! # Strict text comparison, on purpose
//!
//! The two sides' output is compared **byte for byte** after trimming trailing
//! whitespace. No field is normalized away first — not the timestamp, not the
//! alarm. A harness that pre-normalizes decides in advance which differences are
//! allowed to exist, and the whole point here is to find out which ones do.
//!
//! The single exception is declared with its evidence, per that same rule: the
//! `pvxinfo` header (`<pv> from 127.0.0.1:<port>`) is the block separator and
//! never enters the compared text, because the two sides' ports are assigned by
//! this harness and [`crate::ioc::PvaPair::boot`] refuses to run if they match.
//! See [`crate::pvatool`].
//!
//! # Not measured yet
//!
//! QSRV2's **group** PVs (`dbLoadGroup` JSON) are a separately *configured*
//! surface rather than a derived one: they exist only where a group definition
//! declares them, so no `.dbd` enumerates them and this denominator does not
//! cover them. That is a real gap, named here and in the report so a clean run
//! of this phase is never read as "PVA agrees" — it is a future phase.

use std::path::{Path, PathBuf};

use crate::allowlist::{Allowlist, MatchContext};
use crate::catool::ToolError;
use crate::diff::Verdict;
use crate::ioc::{Ioc, PvaPair, PvxTools, Side};
use crate::ntshape::NtShape;
use crate::pvatool::{PvaTools, Readings};
use crate::report::{Counts, Denominator, StaleRow};
use crate::surface::{Coverage, Surface};

/// The named contracts a PVA channel can differ on.
///
/// Distinct from [`crate::diff::Surface`] rather than bolted onto it: those
/// strings are the CA allowlist's data contract (`surface = [...]` in
/// `expected-deviations.toml`), and PVA has no allowlist — see
/// [`adjudicate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PvaSurface {
    /// `pvxinfo`: the declared type, pvxs vs the port. No value involved.
    DeclaredType,
    /// `pvxget`: the value, and which fields the reply marked.
    ValueMarking,
    /// The **port's** declared shape vs the shape the `.dbd` entails.
    PortShapeVsDbd,
    /// **pvxs's** declared shape vs the shape the `.dbd` entails. A difference
    /// here is a finding against this harness, not against the port.
    GroundTruthShapeVsDbd,
}

impl PvaSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeclaredType => "declared_type",
            Self::ValueMarking => "value_marking",
            Self::PortShapeVsDbd => "port_shape_vs_dbd",
            Self::GroundTruthShapeVsDbd => "ground_truth_shape_vs_dbd",
        }
    }

    /// Every surface, so a report can tabulate them without a hand-kept list
    /// that silently omits one.
    pub const ALL: [PvaSurface; 4] = [
        Self::DeclaredType,
        Self::ValueMarking,
        Self::PortShapeVsDbd,
        Self::GroundTruthShapeVsDbd,
    ];
}

/// One concrete disagreement on one contract.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PvaDifference {
    pub surface: PvaSurface,
    /// The reference: what pvxs said for a cross-side contract, or the shape the
    /// `.dbd` entails for a shape contract.
    pub reference: String,
    /// What was compared against it: the port's reading, or the shape that side
    /// declared.
    pub observed: String,
}

/// Everything one side reported about one channel. Each contract is
/// independently optional, so one failing does not throw away the other.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PvaObservation {
    /// `pvxinfo` body — the declared type, without the value.
    pub declared_type: Option<String>,
    /// `pvxget` body — the value and the reply's marking.
    pub value: Option<String>,
    /// Anything that prevented a reading. Non-empty => the channel is ERRORED.
    pub errors: Vec<ToolError>,
}

impl PvaObservation {
    /// Did this side produce **both** contracts?
    ///
    /// Not `errors.is_empty()`: a side that reported no error but also no
    /// reading has still not been measured, and the two must not be confused.
    pub fn is_complete(&self) -> bool {
        self.errors.is_empty() && self.declared_type.is_some() && self.value.is_some()
    }

    /// The NT shape this side declared, parsed from its `pvxinfo` body.
    pub fn shape(&self) -> Option<NtShape> {
        NtShape::observed(self.declared_type.as_deref()?)
    }
}

/// One channel of the surface, read on both sides.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvaCase {
    pub record_type: String,
    pub field: String,
    /// The PVA channel name — `RECORD.FIELD` over QSRV2.
    pub pv: String,
    pub verdict: Verdict,
    /// The shape the `.dbd` entails for this channel. `None` only where no
    /// shape is derivable, which the denominator already excludes.
    pub expected_shape: Option<NtShape>,
    pub c_side: PvaObservation,
    pub rust_side: PvaObservation,
    pub differences: Vec<PvaDifference>,
    /// CBUG ids that justified the differences (empty unless EXPECTED
    /// DEVIATION). Mirrors [`crate::report::CaseResult::allowlisted`].
    #[serde(default)]
    pub allowlisted: Vec<String>,
    /// Why the channel could not be measured. Non-empty iff ERRORED.
    pub errors: Vec<ToolError>,
    /// The `.db` that reproduces it.
    pub db: String,
}

impl PvaCase {
    pub fn id(&self) -> String {
        format!("{}.{}", self.record_type, self.field)
    }
}

/// The PVA phase's report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvaReport {
    pub denominator: Denominator,
    pub channel_coverage: Coverage,
    pub counts: Counts,
    /// Enabled allowlist rows whose scope this run observed but which never
    /// fired: the deviation they describe stopped happening. A finding, exactly
    /// as on the CA side.
    #[serde(default)]
    pub stale_allowlist_rows: Vec<StaleRow>,
    /// Rows this run never put in a position to fire. Coverage, not a finding.
    #[serde(default)]
    pub unexercised_allowlist_rows: Vec<StaleRow>,
    #[serde(default)]
    pub fired_allowlist_rows: Vec<String>,
    pub cases: Vec<PvaCase>,
}

impl PvaReport {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    pub fn defects(&self) -> impl Iterator<Item = &PvaCase> {
        self.cases.iter().filter(|c| c.verdict == Verdict::Defect)
    }

    /// How many DEFECT cases differ on a given contract. The number the phase
    /// exists to separate: a type gap and a marking gap are different defects.
    pub fn defects_on(&self, s: PvaSurface) -> usize {
        self.defects()
            .filter(|c| c.differences.iter().any(|d| d.surface == s))
            .count()
    }

    /// The human summary. Leads with the numbers that decide whether the run
    /// means anything, and never rounds an error into a pass.
    pub fn human(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let d = &self.denominator;
        let c = &self.counts;

        s.push_str(
            "=== PVA DIFFERENTIAL ORACLE: pvxs QSRV2 (softIocPVX) vs oracle-ioc --pva ===\n\n",
        );

        s.push_str("DENOMINATOR (from the .dbd, not hand-listed)\n");
        let _ = writeln!(s, "  spec                  : {}", d.dbd);
        let _ = writeln!(s, "  record types in dbd   : {}", d.record_types_in_dbd);
        let _ = writeln!(
            s,
            "  ...implemented by port: {} (measured by booting each)",
            d.record_types_covered.len()
        );
        if !d.record_types_unimplemented.is_empty() {
            let _ = writeln!(
                s,
                "  ...NOT implemented    : {} — {}  (unmeasurable: nothing to diff against)",
                d.record_types_unimplemented.len(),
                d.record_types_unimplemented.join(", ")
            );
        }
        let _ = writeln!(
            s,
            "  PVA channels          : {}   <-- THE DENOMINATOR (== the CA one: QSRV2 serves\n  \
             {:22}    base's dbChannel namespace, singlesource.cpp:469)",
            d.observable_fields, ""
        );
        let _ = writeln!(
            s,
            "  excluded (DBF_NOACCESS): {} (pvxs refuses the channel: NT of Null)\n",
            d.excluded_noaccess_fields
        );

        let cov = &self.channel_coverage;
        s.push_str("COVERAGE\n");
        let _ = writeln!(
            s,
            "  channels measured on BOTH sides, BOTH contracts: {}/{} = {:.1}%",
            cov.measured,
            cov.enumerated,
            cov.percent()
        );
        let _ = writeln!(
            s,
            "  channels that errored (NOT coverage)           : {}\n",
            cov.errored
        );

        s.push_str("CASES (one per channel; both contracts must run or it is an ERROR)\n");
        let _ = writeln!(s, "  ran                : {}", c.ran);
        let _ = writeln!(s, "  agreed             : {}", c.agreed);
        let _ = writeln!(
            s,
            "  expected deviation : {}  (allowlisted against doc/upstream-c-bugs.md)",
            c.expected_deviation
        );
        let _ = writeln!(s, "  DEFECT             : {}", c.defect);
        let _ = writeln!(
            s,
            "  ERROR              : {}  (could not run — never counted as agreement)",
            c.errored
        );
        match c.check() {
            Ok(()) => s.push_str("  (buckets reconcile with `ran`)\n\n"),
            Err(e) => {
                let _ = writeln!(s, "  !!! {e}\n");
            }
        }

        // The separation this phase exists for. A channel can differ on more
        // than one contract, so these do not sum to the DEFECT count -- said
        // plainly rather than left to be misread as a partition.
        s.push_str(
            "DEFECTS BY CONTRACT (a channel may differ on more than one, so these\n\
                    do NOT sum to the DEFECT count)\n",
        );
        for surface in PvaSurface::ALL {
            let n = self.defects_on(surface);
            let note = match surface {
                PvaSurface::GroundTruthShapeVsDbd if n > 0 => {
                    "   <-- indicts THIS HARNESS's derivation, not the port"
                }
                _ => "",
            };
            let _ = writeln!(s, "  {:<26}{}{}", surface.as_str(), n, note);
        }
        s.push('\n');

        if !self.fired_allowlist_rows.is_empty()
            || !self.stale_allowlist_rows.is_empty()
            || !self.unexercised_allowlist_rows.is_empty()
        {
            s.push_str("ALLOWLIST (doc/upstream-c-bugs.md, transcribed)\n");
            if !self.fired_allowlist_rows.is_empty() {
                let mut fired: Vec<&String> = self.fired_allowlist_rows.iter().collect();
                fired.sort();
                let _ = writeln!(
                    s,
                    "  fired (justified deviations seen): {}",
                    fired
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !self.stale_allowlist_rows.is_empty() {
                let _ = writeln!(
                    s,
                    "  STALE (scope observed, never fired — the deviation stopped): {}",
                    self.stale_allowlist_rows.len()
                );
                for r in &self.stale_allowlist_rows {
                    let _ = writeln!(s, "    {} — {}", r.id, r.why);
                }
            }
            if !self.unexercised_allowlist_rows.is_empty() {
                let _ = writeln!(
                    s,
                    "  unexercised (scope not driven — coverage, not a finding): {}",
                    self.unexercised_allowlist_rows.len()
                );
            }
            s.push('\n');
        }

        let defects: Vec<_> = self.defects().collect();
        if !defects.is_empty() {
            let _ = writeln!(s, "DEFECTS ({})", defects.len());
            for case in defects.iter().take(20) {
                let _ = writeln!(s, "\n  [{}]  {}", case.id(), case.pv);
                for diff in &case.differences {
                    let _ = writeln!(s, "    {} :", diff.surface.as_str());
                    for line in first_differing_lines(&diff.reference, &diff.observed) {
                        let _ = writeln!(s, "      {line}");
                    }
                }
            }
            if defects.len() > 20 {
                let _ = writeln!(s, "\n  ... and {} more (see --json)", defects.len() - 20);
            }
            s.push('\n');
        }

        let errs: Vec<_> = self
            .cases
            .iter()
            .filter(|c| c.verdict == Verdict::Errored)
            .collect();
        if !errs.is_empty() {
            let _ = writeln!(
                s,
                "ERRORS ({}) — channels that could NOT be measured",
                errs.len()
            );
            // Grouped by cause: a record type the ground truth cannot load
            // errors every one of its fields, and printing them all would bury
            // the one fact that explains them.
            let mut by_msg: std::collections::BTreeMap<String, usize> = Default::default();
            for e in &errs {
                let m = e
                    .errors
                    .first()
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "(unknown)".into());
                *by_msg.entry(m).or_default() += 1;
            }
            for (msg, n) in by_msg.iter().take(20) {
                let _ = writeln!(s, "  {n:>5}x  {}", truncate(msg, 110));
            }
            s.push('\n');
        }

        s.push_str(
            "NOT MEASURED: QSRV2 group PVs (dbLoadGroup JSON) are a separately configured\n\
             surface — no .dbd enumerates them, so this denominator does not cover them.\n\
             A clean run here says record.FIELD channels agree, not that PVA agrees.\n",
        );
        s
    }
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        return s;
    }
    let t: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{t}…")
}

/// The first few differing lines, side by side. Enough to act on without
/// dumping two whole NT structures into the terminal.
pub(crate) fn first_differing_lines(c: &str, r: &str) -> Vec<String> {
    const MAX: usize = 6;
    let mut out = Vec::new();
    let (cl, rl): (Vec<&str>, Vec<&str>) = (c.lines().collect(), r.lines().collect());
    for i in 0..cl.len().max(rl.len()) {
        let (a, b) = (cl.get(i).copied(), rl.get(i).copied());
        if a == b {
            continue;
        }
        out.push(format!("ref : {}", a.unwrap_or("<absent>")));
        out.push(format!("obs : {}", b.unwrap_or("<absent>")));
        if out.len() >= MAX * 2 {
            out.push("      ... (truncated; see --json for the full text)".into());
            break;
        }
    }
    out
}

/// Everything a case needs to identify itself. Passed as one value so
/// [`adjudicate`] keeps a signature a caller can read.
#[derive(Debug, Clone)]
pub struct ChannelRef {
    pub record_type: String,
    pub field: String,
    pub pv: String,
    /// The shape the `.dbd` entails for this channel.
    pub expected_shape: Option<NtShape>,
    pub db: String,
}

/// Boot the pair per record type and measure every channel of the surface.
pub fn probe(
    tools: &PvxTools,
    workdir: &Path,
    surface: &Surface,
    record_types: &[String],
    allowlist: &mut Allowlist,
) -> Vec<PvaCase> {
    let mut cases = Vec::new();
    for (i, rt) in record_types.iter().enumerate() {
        let n = surface.fields_of(rt).count();
        eprintln!(
            "[{}/{}] pva read: {rt} ({n} channels)",
            i + 1,
            record_types.len()
        );
        cases.extend(probe_type(tools, workdir, surface, rt, allowlist));
    }
    cases
}

/// Assemble the report from the cases and the surface they were drawn from.
///
/// Coverage is computed here rather than by each caller, so "an ERROR is not
/// coverage" cannot come to mean two different things in two places.
pub fn report(
    dbd_path: &str,
    surface: &Surface,
    cases: Vec<PvaCase>,
    allowlist: &Allowlist,
) -> PvaReport {
    let measured = cases
        .iter()
        .filter(|c| c.verdict != Verdict::Errored)
        .count();
    let stale = |rows: Vec<&crate::allowlist::Deviation>| -> Vec<StaleRow> {
        rows.into_iter()
            .map(|d| StaleRow {
                id: d.id.clone(),
                why: d.why.clone(),
            })
            .collect()
    };
    PvaReport {
        denominator: Denominator {
            dbd: dbd_path.to_string(),
            record_types_in_dbd: surface.covered_types.len() + surface.unimplemented_types.len(),
            record_types_covered: surface.covered_types.clone(),
            record_types_unimplemented: surface.unimplemented_types.clone(),
            observable_fields: surface.denominator(),
            excluded_noaccess_fields: surface.excluded_noaccess,
        },
        channel_coverage: Coverage {
            enumerated: surface.denominator(),
            // Only channels actually visited count. A --record-types filter
            // shrinks what was measured but NOT the denominator, so a partial
            // run honestly reports partial coverage.
            measured,
            errored: cases.len().saturating_sub(measured),
        },
        counts: Counts::tally_verdicts(cases.iter().map(|c| c.verdict)),
        stale_allowlist_rows: stale(allowlist.stale_rows()),
        unexercised_allowlist_rows: stale(allowlist.unexercised_rows()),
        fired_allowlist_rows: allowlist.fired_rows().iter().cloned().collect(),
        cases,
    }
}

fn write_db(workdir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let p = workdir.join(format!("{name}.db"));
    std::fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
    Ok(p)
}

/// One record type: boot the pair, read every channel of it on both sides.
///
/// No mutation happens, so every channel of the type is reachable in a single
/// boot and the cases are order-independent.
fn probe_type(
    tools: &PvxTools,
    workdir: &Path,
    surface: &Surface,
    record_type: &str,
    allowlist: &mut Allowlist,
) -> Vec<PvaCase> {
    let rec = format!("ORACLE:{}", record_type.to_uppercase());
    let db_text = crate::record_stmt(record_type, &rec);

    let refs: Vec<ChannelRef> = surface
        .fields_of(record_type)
        .map(|f| ChannelRef {
            record_type: record_type.to_string(),
            field: f.field.name.clone(),
            pv: f.pv(&rec),
            expected_shape: NtShape::expected(&f.field),
            db: db_text.clone(),
        })
        .collect();
    if refs.is_empty() {
        return Vec::new();
    }
    let pvs: Vec<String> = refs.iter().map(|r| r.pv.clone()).collect();

    let db = match write_db(workdir, &format!("pva_read_{record_type}"), &db_text) {
        Ok(p) => p,
        Err(e) => return errored_cases(&refs, &e),
    };
    // `PvaPair::boot` returns only once both sides are reachable AND each is
    // proven the sole server on its port, so a reading obtained below is
    // attributable by construction.
    let pair = match PvaPair::boot(tools, &db, &rec) {
        Ok(p) => p,
        // The pair would not boot: every channel of this type is an ERROR, and
        // not one of them is scored as agreement. The denominator does not
        // shrink just because we could not look.
        Err(e) => return errored_cases(&refs, &e.to_string()),
    };

    let c = PvaTools::new(tools, pair.c.port(), Side::C);
    let r = PvaTools::new(tools, pair.rust.port(), Side::Rust);

    // The two sides are separate servers on separate ports with no shared
    // state, so reading them concurrently changes nothing either one observes
    // -- it only stops each from waiting on the other. Within a side the two
    // contracts are independent reads of already-settled state.
    let (obs_c, obs_r) = std::thread::scope(|s| {
        let hc = s.spawn(|| observe(&c, &pvs));
        let hr = s.spawn(|| observe(&r, &pvs));
        (
            hc.join().expect("C read lane panicked"),
            hr.join().expect("Rust read lane panicked"),
        )
    });

    refs.iter()
        .enumerate()
        .map(|(i, cr)| adjudicate(cr, &obs_c[i], &obs_r[i], allowlist))
        .collect()
}

/// Read both contracts for every PV on one side.
fn observe(t: &PvaTools, pvs: &[String]) -> Vec<PvaObservation> {
    let (types, values) = std::thread::scope(|s| {
        let ht = s.spawn(|| t.pvxinfo_batch(pvs));
        let hv = s.spawn(|| t.pvxget_batch(pvs));
        (
            ht.join().expect("pvxinfo lane panicked"),
            hv.join().expect("pvxget lane panicked"),
        )
    });
    merge(types, values)
}

/// Fold one side's two batched readings into a per-PV observation.
fn merge(types: Readings, values: Readings) -> Vec<PvaObservation> {
    types
        .into_iter()
        .zip(values)
        .map(|(t, v)| {
            let mut o = PvaObservation::default();
            match t {
                Ok(x) => o.declared_type = Some(x),
                Err(e) => o.errors.push(e),
            }
            match v {
                Ok(x) => o.value = Some(x),
                Err(e) => o.errors.push(e),
            }
            o
        })
        .collect()
}

/// The verdict for one channel, from the two sides' observations.
///
/// The order of the checks is the policy:
/// 1. **Either contract missing on either side => ERROR.** Checked first, so a
///    channel that could not be measured can never be scored as agreement. Two
///    sides that both failed have NOT agreed — nothing was measured, so there is
///    nothing to agree about.
/// 2. No differences => AGREED.
/// 3. Anything left => DEFECT.
///
/// A difference is EXPECTED DEVIATION only when a NOT-REPRODUCED row in
/// `doc/upstream-c-bugs.md` (transcribed into `expected-deviations.toml`)
/// justifies it — the same contract the CA phases hold. The first such PVA row
/// is CBUG-G1 (pvxs drops `display.precision` for a field that NULLs
/// `get_graphic_double`; the port declines to reproduce it). A case is
/// EXPECTED DEVIATION only if EVERY difference on it is justified — one
/// unjustified diff makes the whole case a DEFECT, so a real bug cannot be
/// laundered by a justified one sharing the channel.
pub fn adjudicate(
    ch: &ChannelRef,
    c: &PvaObservation,
    r: &PvaObservation,
    allowlist: &mut Allowlist,
) -> PvaCase {
    let mut errors: Vec<ToolError> = Vec::new();
    errors.extend(c.errors.iter().cloned());
    errors.extend(r.errors.iter().cloned());

    let base = PvaCase {
        record_type: ch.record_type.clone(),
        field: ch.field.clone(),
        pv: ch.pv.clone(),
        verdict: Verdict::Errored,
        expected_shape: ch.expected_shape.clone(),
        c_side: c.clone(),
        rust_side: r.clone(),
        differences: Vec::new(),
        allowlisted: Vec::new(),
        errors,
        db: ch.db.clone(),
    };

    if !c.is_complete() || !r.is_complete() {
        return base;
    }

    let ctx = MatchContext {
        record_type: &ch.record_type,
        field: &ch.field,
        class: None,
    };
    // Tell the allowlist what this case looked at BEFORE the agreed early-out,
    // so a row whose deviation stopped happening reads as stale (a finding), not
    // as unexercised (coverage). A read case compares every surface `compare`
    // can emit.
    let compared: Vec<&str> = surfaces_compared(ch);
    allowlist.note_compared_pva(&ctx, &compared);

    let differences = compare(ch, c, r);
    if differences.is_empty() {
        return PvaCase {
            verdict: Verdict::Agreed,
            ..base
        };
    }

    let hits: Vec<Option<String>> = differences
        .iter()
        .map(|d| allowlist.match_pva_diff(&ctx, d.surface.as_str(), &d.reference, &d.observed))
        .collect();
    let all_justified = hits.iter().all(Option::is_some);
    let allowlisted: Vec<String> = hits.into_iter().flatten().collect();

    PvaCase {
        verdict: if all_justified {
            Verdict::ExpectedDeviation
        } else {
            Verdict::Defect
        },
        differences,
        allowlisted,
        ..base
    }
}

/// The surfaces [`compare`] emits for this channel — the same set, so
/// "what did the run look at" cannot drift from "what did it diff".
fn surfaces_compared(ch: &ChannelRef) -> Vec<&'static str> {
    let mut s = vec![
        PvaSurface::DeclaredType.as_str(),
        PvaSurface::ValueMarking.as_str(),
    ];
    if ch.expected_shape.is_some() {
        s.push(PvaSurface::GroundTruthShapeVsDbd.as_str());
        s.push(PvaSurface::PortShapeVsDbd.as_str());
    }
    s
}

/// Compare the two sides on both contracts, and each side against the `.dbd`.
///
/// Only called once both sides are complete, so an absence can never be
/// reported here as a difference — absences are the ERROR path's business, and
/// scoring "pvxs said X, the port said nothing" as a difference would fabricate
/// a finding.
fn compare(ch: &ChannelRef, c: &PvaObservation, r: &PvaObservation) -> Vec<PvaDifference> {
    let mut out = Vec::new();
    let mut push = |surface, reference: String, observed: String| {
        if reference != observed {
            out.push(PvaDifference {
                surface,
                reference,
                observed,
            });
        }
    };

    let (ct, rt) = (
        c.declared_type.clone().unwrap_or_default(),
        r.declared_type.clone().unwrap_or_default(),
    );
    push(
        PvaSurface::DeclaredType,
        ct.trim_end().to_string(),
        rt.trim_end().to_string(),
    );

    let (cv, rv) = (
        c.value.clone().unwrap_or_default(),
        r.value.clone().unwrap_or_default(),
    );
    push(
        PvaSurface::ValueMarking,
        cv.trim_end().to_string(),
        rv.trim_end().to_string(),
    );

    // Each side against the shape the .dbd entails. A shape that will not parse
    // is reported as such rather than skipped: "we could not read the shape" is
    // not "the shape is right".
    if let Some(expected) = &ch.expected_shape {
        let render = |o: &PvaObservation| {
            o.shape()
                .map(|s| s.render())
                .unwrap_or_else(|| "<no parseable NT shape>".to_string())
        };
        push(
            PvaSurface::GroundTruthShapeVsDbd,
            expected.render(),
            render(c),
        );
        push(PvaSurface::PortShapeVsDbd, expected.render(), render(r));
    }

    out
}

/// A boot failure must produce one ERROR case per channel it prevented from
/// being measured — not one aggregate error, and above all not silence.
fn errored_cases(refs: &[ChannelRef], msg: &str) -> Vec<PvaCase> {
    refs.iter()
        .map(|ch| PvaCase {
            record_type: ch.record_type.clone(),
            field: ch.field.clone(),
            pv: ch.pv.clone(),
            verdict: Verdict::Errored,
            expected_shape: ch.expected_shape.clone(),
            c_side: PvaObservation::default(),
            rust_side: PvaObservation::default(),
            differences: Vec::new(),
            allowlisted: Vec::new(),
            // A boot failure is not attributable to one side by construction,
            // so it is recorded against both -- never dropped, never guessed.
            errors: vec![
                ToolError {
                    side: Side::C,
                    tool: "boot".into(),
                    message: msg.to_string(),
                },
                ToolError {
                    side: Side::Rust,
                    tool: "boot".into(),
                    message: msg.to_string(),
                },
            ],
            db: ch.db.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbd::{DbfType, FieldDef};

    fn terr(side: Side, tool: &str) -> ToolError {
        ToolError {
            side,
            tool: tool.into(),
            message: "Timeout".into(),
        }
    }

    fn chan() -> ChannelRef {
        let val = FieldDef {
            name: "VAL".into(),
            dbf: DbfType::Double,
            size: None,
            special: None,
            menu: None,
            initial: None,
            pp: false,
            asl: None,
        };
        ChannelRef {
            record_type: "ai".into(),
            field: val.name.clone(),
            pv: "ORACLE:AI.VAL".into(),
            expected_shape: NtShape::expected(&val),
            db: "record(ai, \"ORACLE:AI\") {}".into(),
        }
    }

    /// A side that declared the expected shape and read a value.
    fn good(value: &str) -> PvaObservation {
        PvaObservation {
            declared_type: Some("struct \"epics:nt/NTScalar:1.0\" {\n    double value\n}".into()),
            value: Some(value.into()),
            errors: vec![],
        }
    }

    /// Adjudicate against an empty allowlist — the pre-allowlist behaviour, so
    /// every difference is a DEFECT. Tests that exercise the allowlist build one
    /// explicitly (see [`cbug_g1_precision_add_is_expected_deviation`]).
    fn adj(ch: &ChannelRef, c: &PvaObservation, r: &PvaObservation) -> PvaCase {
        adjudicate(ch, c, r, &mut Allowlist::empty())
    }

    #[test]
    fn identical_readings_on_both_contracts_agree() {
        let c = adj(
            &chan(),
            &good("value double = 1"),
            &good("value double = 1"),
        );
        assert_eq!(c.verdict, Verdict::Agreed);
        assert!(c.errors.is_empty());
        assert!(c.differences.is_empty());
    }

    /// The separation this phase exists for: a value difference must land on
    /// the value contract and leave the type contract clean.
    #[test]
    fn a_value_difference_lands_only_on_the_value_contract() {
        let c = adj(
            &chan(),
            &good("value double = 1"),
            &good("value double = 2"),
        );
        assert_eq!(c.verdict, Verdict::Defect);
        let s: Vec<_> = c.differences.iter().map(|d| d.surface).collect();
        assert_eq!(s, [PvaSurface::ValueMarking], "type must not be implicated");
    }

    /// ...and the converse: a type difference must not be reported as a value
    /// difference. In Delta text these two look alike, which is exactly why
    /// they are measured by different tools.
    #[test]
    fn a_type_difference_lands_only_on_the_type_contract() {
        let mut rust = good("value double = 1");
        rust.declared_type = Some(
            "struct \"epics:nt/NTScalar:1.0\" {\n    double value\n    int32_t extra\n}".into(),
        );
        let c = adj(&chan(), &good("value double = 1"), &rust);
        assert_eq!(c.verdict, Verdict::Defect);
        let s: Vec<_> = c.differences.iter().map(|d| d.surface).collect();
        assert_eq!(
            s,
            [PvaSurface::DeclaredType],
            "value must not be implicated"
        );
    }

    /// A wrong shape is attributed to the PORT when only the port has it.
    #[test]
    fn a_wrong_port_shape_is_attributed_to_the_port() {
        let mut rust = good("value double = 1");
        // The port declares int32_t where the .dbd entails double.
        rust.declared_type =
            Some("struct \"epics:nt/NTScalar:1.0\" {\n    int32_t value\n}".into());
        let c = adj(&chan(), &good("value double = 1"), &rust);
        assert_eq!(c.verdict, Verdict::Defect);
        let s: Vec<_> = c.differences.iter().map(|d| d.surface).collect();
        assert!(s.contains(&PvaSurface::PortShapeVsDbd), "got {s:?}");
        assert!(
            !s.contains(&PvaSurface::GroundTruthShapeVsDbd),
            "pvxs matched the derivation, so the harness is not implicated: {s:?}"
        );
    }

    /// ...and to the HARNESS when pvxs is the one that disagrees. pvxs is
    /// ground truth: if it does not match the prediction, the prediction is
    /// wrong, and reporting that as a port defect would be a fabricated finding.
    #[test]
    fn a_ground_truth_shape_mismatch_indicts_the_harness_not_the_port() {
        let mut c_side = good("value double = 1");
        c_side.declared_type =
            Some("struct \"epics:nt/NTScalar:1.0\" {\n    int32_t value\n}".into());
        let mut rust = good("value double = 1");
        rust.declared_type = c_side.declared_type.clone();

        let case = adj(&chan(), &c_side, &rust);
        let s: Vec<_> = case.differences.iter().map(|d| d.surface).collect();
        // Both sides agree with each other, so no cross-side contract fires...
        assert!(!s.contains(&PvaSurface::DeclaredType));
        assert!(!s.contains(&PvaSurface::ValueMarking));
        // ...but both disagree with the .dbd, which names the harness.
        assert!(s.contains(&PvaSurface::GroundTruthShapeVsDbd), "got {s:?}");
        assert!(s.contains(&PvaSurface::PortShapeVsDbd), "got {s:?}");
    }

    /// Trailing whitespace is not a behavioural difference; nothing else is
    /// normalized.
    #[test]
    fn only_trailing_whitespace_is_normalized() {
        let c = adj(&chan(), &good("v = 1\n"), &good("v = 1"));
        assert_eq!(c.verdict, Verdict::Agreed);
        // ...but an interior difference still lands, even a whitespace one.
        let c = adj(&chan(), &good("v =  1"), &good("v = 1"));
        assert_eq!(c.verdict, Verdict::Defect);
    }

    /// **The rule the whole harness rests on**, at the contract level: a
    /// channel whose type read succeeded but whose value read failed was NOT
    /// measured, even though half of it was.
    #[test]
    fn a_channel_is_errored_when_either_contract_did_not_run() {
        let mut half = good("value double = 1");
        half.value = None;
        half.errors.push(terr(Side::Rust, "pvxget"));
        let c = adj(&chan(), &good("value double = 1"), &half);
        assert_eq!(
            c.verdict,
            Verdict::Errored,
            "one contract missing must not score as agreement on the other",
        );

        let mut half = good("value double = 1");
        half.declared_type = None;
        half.errors.push(terr(Side::Rust, "pvxinfo"));
        let c = adj(&chan(), &good("value double = 1"), &half);
        assert_eq!(c.verdict, Verdict::Errored);
    }

    /// A side reporting neither a reading nor an error has still not been
    /// measured. `is_complete` must not be a synonym for "no errors".
    #[test]
    fn a_silent_side_with_no_reading_is_not_complete() {
        let empty = PvaObservation::default();
        assert!(
            !empty.is_complete(),
            "no reading and no error is not a measurement"
        );
        let c = adj(&chan(), &good("v"), &empty);
        assert_eq!(c.verdict, Verdict::Errored);
    }

    /// Two sides that both failed have NOT agreed.
    #[test]
    fn both_sides_failing_is_an_error_never_agreement() {
        let fail = |side| PvaObservation {
            errors: vec![terr(side, "pvxget"), terr(side, "pvxinfo")],
            ..Default::default()
        };
        let c = adj(&chan(), &fail(Side::C), &fail(Side::Rust));
        assert_eq!(
            c.verdict,
            Verdict::Errored,
            "two failed reads must not score as agreement",
        );
        assert_eq!(c.errors.len(), 4, "every side's every failure is reported");
    }

    /// A boot failure errors every channel of the type — the denominator does
    /// not shrink because we could not look.
    #[test]
    fn a_boot_failure_errors_every_channel_it_prevented_and_names_both_sides() {
        let refs = vec![chan(), chan()];
        let cases = errored_cases(&refs, "softIocPVX exited during boot");
        assert_eq!(cases.len(), 2);
        for c in &cases {
            assert_eq!(c.verdict, Verdict::Errored);
            assert_eq!(c.errors.len(), 2, "attributed to neither side alone");
            assert!(c.errors.iter().any(|e| e.side == Side::C));
            assert!(c.errors.iter().any(|e| e.side == Side::Rust));
        }
    }

    /// One tool failing must not throw away the other's reading — the case is
    /// still ERRORED, but the half that was read is kept for diagnosis.
    #[test]
    fn merge_keeps_the_contract_that_did_read() {
        let obs = merge(
            vec![Ok("type text".into())],
            vec![Err(terr(Side::Rust, "pvxget"))],
        );
        assert_eq!(obs[0].declared_type.as_deref(), Some("type text"));
        assert!(obs[0].value.is_none());
        assert_eq!(obs[0].errors.len(), 1);
        assert!(!obs[0].is_complete());
    }

    #[test]
    fn counts_reconcile_over_pva_verdicts() {
        let counts = Counts::tally_verdicts([
            Verdict::Agreed,
            Verdict::Defect,
            Verdict::Errored,
            Verdict::Agreed,
        ]);
        assert_eq!(counts.ran, 4);
        assert_eq!(counts.agreed, 2);
        assert_eq!(counts.defect, 1);
        assert_eq!(counts.errored, 1);
        counts.check().expect("buckets must reconcile");
    }

    /// An errored channel is not coverage, and the denominator is the surface's
    /// — not the number of cases that happened to run.
    #[test]
    fn errors_are_not_coverage_and_the_denominator_is_the_surface() {
        let dbd = crate::dbd::Dbd::parse(
            "recordtype(ai) {\n    field(VAL, DBF_DOUBLE) { pp(TRUE) }\n    field(NAME, DBF_STRING) { size(61) }\n}\n",
        )
        .unwrap();
        let supported = ["ai".to_string()].into_iter().collect();
        let surface = Surface::build(&dbd, &supported);

        let ok = adj(&chan(), &good("v"), &good("v"));
        let bad = errored_cases(&[chan()], "boom");
        let rep = report(
            "test.dbd",
            &surface,
            vec![ok, bad[0].clone()],
            &Allowlist::empty(),
        );

        assert_eq!(rep.denominator.observable_fields, 2, "from the .dbd");
        assert_eq!(rep.channel_coverage.enumerated, 2);
        assert_eq!(
            rep.channel_coverage.measured, 1,
            "the errored one is not coverage"
        );
        assert_eq!(rep.channel_coverage.errored, 1);
        assert!((rep.channel_coverage.percent() - 50.0).abs() < 1e-9);
        rep.counts.check().expect("buckets reconcile");
    }

    #[test]
    fn the_human_report_states_coverage_and_never_hides_errors() {
        let dbd = crate::dbd::Dbd::parse(
            "recordtype(ai) {\n    field(VAL, DBF_DOUBLE) { pp(TRUE) }\n}\n",
        )
        .unwrap();
        let surface = Surface::build(&dbd, &["ai".to_string()].into_iter().collect());
        let bad = errored_cases(&[chan()], "softIocPVX exited during boot");
        let rep = report("softIoc.dbd", &surface, bad, &Allowlist::empty());
        let h = rep.human();
        assert!(h.contains("THE DENOMINATOR"), "states the denominator");
        assert!(
            h.contains("0/1 = 0.0%"),
            "a run that measured nothing says so"
        );
        assert!(h.contains("ERROR"));
        assert!(
            h.contains("DEFECTS BY CONTRACT"),
            "type and marking stay apart"
        );
        assert!(
            h.contains("group PVs"),
            "names the surface it does NOT cover"
        );
    }

    /// The CBUG-G1 allowlist row: a bo channel whose only marking difference is
    /// the port adding a `display.precision` line pvxs omits is EXPECTED
    /// DEVIATION, not DEFECT — and it fires the row so it is not stale.
    fn g1_allowlist() -> Allowlist {
        Allowlist::parse(
            "schema = 1\n\
             [[deviation]]\n\
             id = \"CBUG-G1\"\n\
             bucket = \"NOT-REPRODUCED\"\n\
             record_types = [\"bo\"]\n\
             surface = [\"value_marking\"]\n\
             port_adds_leaves = [\"display.precision\"]\n\
             why = \"port serves precision pvxs drops\"\n",
        )
        .expect("valid allowlist")
    }

    fn bo_chan() -> ChannelRef {
        ChannelRef {
            record_type: "bo".into(),
            field: "HIGH".into(),
            pv: "ORACLE:BO.HIGH".into(),
            expected_shape: None,
            db: "record(bo, \"ORACLE:BO\") {}".into(),
        }
    }

    #[test]
    fn cbug_g1_precision_add_is_expected_deviation() {
        // Same leaves, in different orders, plus one display.precision line the
        // port adds — exactly CBUG-G1's shape.
        let c = good("value = 0\ncontrol.limitHigh double = 100000");
        let r = good("control.limitHigh double = 100000\ndisplay.precision int32_t = 2\nvalue = 0");
        let mut al = g1_allowlist();
        let case = adjudicate(&bo_chan(), &c, &r, &mut al);
        assert_eq!(case.verdict, Verdict::ExpectedDeviation);
        assert_eq!(case.allowlisted, vec!["CBUG-G1".to_string()]);
        assert!(al.fired_rows().contains("CBUG-G1"));
        assert!(al.stale_rows().is_empty(), "a fired row is not stale");
    }

    #[test]
    fn cbug_g1_does_not_launder_a_second_marking_difference() {
        // The port adds display.precision AND disagrees on control.limitHigh.
        // The precision add is justified, the limit change is not, so the whole
        // case must stay a DEFECT — a real diff cannot ride in on the row.
        let c = good("value = 0\ncontrol.limitHigh double = 100000");
        let r = good("value = 0\ncontrol.limitHigh double = 999\ndisplay.precision int32_t = 2");
        let mut al = g1_allowlist();
        let case = adjudicate(&bo_chan(), &c, &r, &mut al);
        assert_eq!(case.verdict, Verdict::Defect);
    }

    #[test]
    fn cbug_g1_scope_stops_at_the_named_types() {
        // The identical precision-add shape on a type NOT in the row's scope is
        // still a DEFECT — the row does not generalise to every record type.
        let c = good("value = 0");
        let r = good("value = 0\ndisplay.precision int32_t = 2");
        let mut al = g1_allowlist();
        let ai = ChannelRef {
            record_type: "ai".into(),
            ..bo_chan()
        };
        let case = adjudicate(&ai, &c, &r, &mut al);
        assert_eq!(case.verdict, Verdict::Defect);
    }
}
