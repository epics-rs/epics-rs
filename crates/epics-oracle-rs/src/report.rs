//! The output: machine-readable JSON, and a human summary that cannot lie.
//!
//! Two invariants are enforced here rather than merely intended:
//!
//! 1. **The counts add up.** `ran == agreed + expected_deviation + defect +
//!    errored`. [`Counts::check`] asserts it. A harness whose buckets do not
//!    reconcile is a harness that has silently dropped cases, which is the
//!    failure this whole exercise exists to eliminate.
//! 2. **Coverage counts only what was actually measured.** A case that errored
//!    is not coverage. `measured` excludes it, so a run where the IOC failed to
//!    boot reports *low coverage*, not high coverage with a footnote.

use crate::catool::ToolError;
use crate::diff::Observation;
use crate::diff::{Difference, Verdict};
use crate::surface::Coverage;

/// The minimal `.db` + operation sequence that reproduces one case.
///
/// Minimal *by construction*: each put case drives one operation against its
/// own single record, so there is nothing left to shrink. No search needed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Reproducer {
    pub db: String,
    pub ops: Vec<String>,
}

impl Reproducer {
    /// Print as something a human can paste into a shell.
    pub fn render(&self, indent: &str) -> String {
        let mut s = String::new();
        s.push_str(&format!("{indent}# --- repro.db ---\n"));
        for l in self.db.lines() {
            s.push_str(&format!("{indent}{l}\n"));
        }
        s.push_str(&format!(
            "{indent}# softIoc -S -d repro.db   (C, ground truth)\n"
        ));
        s.push_str(&format!(
            "{indent}# oracle-ioc --db repro.db (port under test)\n"
        ));
        for op in &self.ops {
            s.push_str(&format!("{indent}{op}\n"));
        }
        s
    }
}

/// Which probe produced a case.
///
/// Recorded by the producer, never inferred at a consumer. It exists because
/// [`CaseResult::class`] cannot answer the question: `class` is the boundary
/// *value* driven, and it is `None` for the read phase and the monitor phase
/// alike, so `class.is_none()` selects two different experiments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CasePhase {
    Read,
    Put,
    Monitor,
    Array,
}

impl CasePhase {
    /// The one spelling: the same word serde writes, so a JSON reader and a
    /// human reader name the same probe.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Put => "put",
            Self::Monitor => "monitor",
            Self::Array => "array",
        }
    }
}

/// One adjudicated case.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseResult {
    pub record_type: String,
    pub field: String,
    /// The probe this case came from. Not derivable from `class`.
    pub phase: CasePhase,
    /// The boundary value class driven, for the put and array phases. `None`
    /// says "no boundary value", NOT "read phase" — see [`CasePhase`].
    pub class: Option<String>,
    pub verdict: Verdict,
    pub differences: Vec<Difference>,
    /// CBUG ids that justified the differences (empty unless EXPECTED DEVIATION).
    pub allowlisted: Vec<String>,
    /// Why the case could not run. Non-empty iff verdict is ERRORED.
    pub errors: Vec<ToolError>,
    pub reproducer: Reproducer,
    pub c_side: Observation,
    pub rust_side: Observation,
}

impl CaseResult {
    /// What a human types back to re-find this case.
    ///
    /// It must name the probe. Four phases put cases into one report, and the
    /// read and monitor phases both drive `VAL` with no boundary class, so
    /// `record_type.field` alone rendered two different measurements of `ai.VAL`
    /// as one string in the DEFECTS list — the reader cannot tell which
    /// experiment the difference came from, and the two entries read as a
    /// duplicate rather than as two findings.
    pub fn id(&self) -> String {
        match &self.class {
            Some(c) => format!(
                "{}.{} {}[{}]",
                self.record_type,
                self.field,
                self.phase.as_str(),
                c
            ),
            None => format!(
                "{}.{} {}",
                self.record_type,
                self.field,
                self.phase.as_str()
            ),
        }
    }
}

/// Field coverage, over the **read** probe alone.
///
/// The read probe is the only phase that visits every field of the denominator
/// exactly once, so it is the only phase whose case count is commensurable with
/// that denominator. Selecting it by [`CasePhase::Read`] rather than by "has no
/// boundary class" is what keeps the fraction honest: one monitor case per
/// record type also carries no class, and counting those inflated `measured`
/// past the fields actually read — far worse, a monitor agreement then offset a
/// read ERROR and the measurement failure vanished from the number.
///
/// Phases that visit only part of the surface contribute nothing here and need
/// no special case: a run without a read phase simply has no read cases, and
/// honestly reports zero.
pub fn field_coverage(cases: &[CaseResult], enumerated: usize) -> Coverage {
    let read: Vec<&CaseResult> = cases
        .iter()
        .filter(|c| c.phase == CasePhase::Read && !c.field.is_empty())
        .collect();
    let measured = read
        .iter()
        .filter(|c| c.verdict != Verdict::Errored)
        .count();
    Coverage {
        enumerated,
        // Only fields actually visited count; a --record-types filter shrinks
        // what was measured but NOT the denominator, so a partial run honestly
        // reports partial coverage.
        measured,
        errored: read.len() - measured,
    }
}

/// The buckets. They must reconcile.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Counts {
    pub ran: usize,
    pub agreed: usize,
    pub expected_deviation: usize,
    pub defect: usize,
    /// Agreed on every surface, and neither side reported the put complete.
    /// See [`Verdict::NeitherCompleted`].
    pub neither_completed: usize,
    pub errored: usize,
}

impl Counts {
    pub fn tally(cases: &[CaseResult]) -> Self {
        Self::tally_verdicts(cases.iter().map(|c| c.verdict))
    }

    /// Tally bare verdicts. The single owner of the bucket rule: the CA phases
    /// reach it through [`Self::tally`] and the PVA phase
    /// ([`crate::pvaread`]) hands its own case shape's verdicts straight in,
    /// so "a case that could not run is ERRORED, never agreement" cannot come
    /// to mean two different things in two counters.
    pub fn tally_verdicts(verdicts: impl IntoIterator<Item = Verdict>) -> Self {
        let mut c = Counts::default();
        for v in verdicts {
            c.ran += 1;
            match v {
                Verdict::Agreed => c.agreed += 1,
                Verdict::ExpectedDeviation => c.expected_deviation += 1,
                Verdict::Defect => c.defect += 1,
                Verdict::NeitherCompleted => c.neither_completed += 1,
                Verdict::Errored => c.errored += 1,
            }
        }
        c
    }

    /// Every case landed in exactly one bucket. If this ever fails, the harness
    /// has lost cases and no number it prints can be trusted.
    pub fn check(&self) -> Result<(), String> {
        let sum = self.agreed
            + self.expected_deviation
            + self.defect
            + self.neither_completed
            + self.errored;
        if sum != self.ran {
            return Err(format!(
                "counts do not reconcile: ran={} but buckets sum to {sum}",
                self.ran
            ));
        }
        Ok(())
    }
}

/// Why this run must fail — one line per reason, empty iff the run is clean.
///
/// **The single owner of the exit code**, for every phase. The rule used to be
/// written out three times (`bin/oracle.rs`, once per phase) as
/// `defect == 0 && errored == 0`, and each copy read only the two counters it
/// happened to know about. Everything else the run reported — an unimplemented
/// record type, a stale allowlist row — was printed and then discarded by the
/// caller, so a finding the harness had correctly detected still exited 0.
///
/// A reason here is a *finding the run made*, not a bucket: the caller prints
/// the lines and exits non-zero if there are any, so adding a finding class
/// means adding it once, here.
///
/// - **DEFECT / ERROR** — the original two. "Could not measure" fails exactly
///   like "measured wrong"; that is the rule the audit loop lacked.
/// - **Unimplemented record type** — the port cannot load a type the `.dbd`
///   declares, so every field of it went unmeasured. It is in the denominator
///   (see [`crate::surface`]), so coverage already fell; the exit code must say
///   so too, or a record type going dark is a green run.
/// - **Stale allowlist row** — a justified deviation whose scope this run did
///   observe and which never fired, so the deviation stopped happening: either
///   the port regressed onto C's bug or C fixed it upstream. Both are findings
///   the harness detected correctly and the caller then discarded. Rows the run
///   never exercised are NOT here — see [`crate::allowlist::Allowlist::
///   unexercised_rows`]; failing on those would make every scoped run red.
pub fn run_failures(
    counts: &Counts,
    unimplemented_types: &[String],
    stale_rows: &[StaleRow],
) -> Vec<String> {
    let mut out = Vec::new();
    if counts.defect > 0 {
        out.push(format!("{} DEFECT case(s)", counts.defect));
    }
    if counts.errored > 0 {
        out.push(format!(
            "{} ERROR case(s) — could not measure, which is not a pass",
            counts.errored
        ));
    }
    if !unimplemented_types.is_empty() {
        out.push(format!(
            "{} record type(s) the port does not implement: {} — every field of \
             each went unmeasured",
            unimplemented_types.len(),
            unimplemented_types.join(", ")
        ));
    }
    if !stale_rows.is_empty() {
        let ids: Vec<&str> = stale_rows.iter().map(|r| r.id.as_str()).collect();
        out.push(format!(
            "{} STALE allowlist row(s): {} — the justified deviation stopped \
             happening where the run looked",
            stale_rows.len(),
            ids.join(", ")
        ));
    }
    out
}

/// The process exit code the run's findings require: 0 iff there are none.
///
/// The companion to [`run_failures`] and the reason the exit rule is a library
/// value rather than a branch inside `bin/oracle.rs`: an exit code that only
/// exists inside a binary cannot be asserted by a test, and this harness's
/// whole output is its exit code.
pub fn exit_status(failures: &[String]) -> u8 {
    u8::from(!failures.is_empty())
}

/// A stale allowlist row: a justified deviation that stopped happening.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleRow {
    pub id: String,
    pub why: String,
    /// The `--phase` values that can actually drive this row, derived from the
    /// row's own `surface` list. See [`StaleRow::of`].
    pub driven_by: Vec<&'static str>,
}

impl StaleRow {
    /// The one mint for a [`StaleRow`], so `driven_by` cannot disagree with the
    /// row it describes.
    ///
    /// The three surface vocabularies are disjoint and belong to different
    /// lanes: [`crate::diff::Surface`] is CA, [`crate::pvaread::PvaSurface`] is
    /// `--phase pvaread`, [`crate::pvamonitor::MonSurface`] is
    /// `--phase pvamonitor`. `Phase::PvaRead` and `Phase::PvaMonitor` are
    /// deliberately OUTSIDE `Phase::All` (different ground truth, different
    /// instrument, no allowlist), so a row whose every surface is a PVA one is
    /// unreachable from `--phase all` no matter how wide the record-type set
    /// is. Telling its reader to widen the run is advice that cannot work,
    /// which is why the phase is computed here and not written into the
    /// message.
    pub fn of(d: &crate::allowlist::Deviation) -> Self {
        let mut driven_by: Vec<&'static str> = Vec::new();
        let mut push = |p: &'static str| {
            if !driven_by.contains(&p) {
                driven_by.push(p);
            }
        };
        for name in &d.surface {
            let name = name.as_str();
            if crate::diff::Surface::ALL.iter().any(|s| s.as_str() == name) {
                push("--phase all");
            } else if crate::pvaread::PvaSurface::ALL
                .iter()
                .any(|s| s.as_str() == name)
            {
                push("--phase pvaread");
            } else if crate::pvamonitor::MonSurface::ALL
                .iter()
                .any(|s| s.as_str() == name)
            {
                push("--phase pvamonitor");
            }
        }
        Self {
            id: d.id.clone(),
            why: d.why.trim().to_string(),
            driven_by,
        }
    }

    /// How to name the driving phase in a human line. A row whose surfaces
    /// match no known vocabulary gets no invented advice.
    pub fn driven_by_phrase(&self) -> String {
        match self.driven_by.as_slice() {
            [] => "No phase in this harness drives its surfaces — check the row's \
                   `surface` list against the three surface vocabularies"
                .to_string(),
            [one] => format!("Drive it with `{one}`"),
            many => format!(
                "Drive it with {}",
                many.iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        }
    }
}

/// What the denominator actually was, so the coverage number can be audited.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Denominator {
    /// The spec the surface was enumerated from.
    pub dbd: String,
    pub record_types_in_dbd: usize,
    /// Record types the port implements (measured, not assumed).
    pub record_types_covered: Vec<String>,
    /// In the dbd but not implemented by the port: a real, named coverage gap.
    pub record_types_unimplemented: Vec<String>,
    /// CA-observable fields across the covered types. THE denominator.
    pub observable_fields: usize,
    /// DBF_NOACCESS declarations excluded (no client can reach them).
    pub excluded_noaccess_fields: usize,
    /// Record types the **monitor** phase left out of its drive denominator,
    /// each with the `.dbd` rule that removed it
    /// ([`crate::surface::val_status`]). Empty when this run drove no monitor
    /// phase — an exclusion that was never in scope is not an exclusion.
    #[serde(default)]
    pub excluded_undrivable_val: Vec<UndrivableVal>,
}

/// One record type the monitor phase could not stimulate, and the rule that
/// says so.
///
/// Named rather than counted: a drive denominator that reports only "10
/// excluded" cannot be audited back against the `.dbd`, which is the failure
/// this report exists to prevent.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UndrivableVal {
    pub record_type: String,
    /// The rule, from [`crate::surface::ValStatus::why`].
    pub why: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub denominator: Denominator,
    pub field_coverage: Coverage,
    pub counts: Counts,
    pub stale_allowlist_rows: Vec<StaleRow>,
    /// Rows this run never drove — no case in their scope ran, or none of their
    /// surfaces was compared. NOT findings: coverage. A `--phase read` run leaves
    /// every put-surface row here, and reporting those as stale would be inventing
    /// findings out of a narrowed scope.
    #[serde(default)]
    pub unexercised_allowlist_rows: Vec<StaleRow>,
    pub fired_allowlist_rows: Vec<String>,
    pub cases: Vec<CaseResult>,
}

impl Report {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serializes")
    }

    pub fn defects(&self) -> impl Iterator<Item = &CaseResult> {
        self.cases.iter().filter(|c| c.verdict == Verdict::Defect)
    }

    pub fn errors(&self) -> impl Iterator<Item = &CaseResult> {
        self.cases.iter().filter(|c| c.verdict == Verdict::Errored)
    }

    pub fn deviations(&self) -> impl Iterator<Item = &CaseResult> {
        self.cases
            .iter()
            .filter(|c| c.verdict == Verdict::ExpectedDeviation)
    }

    /// The human summary. Leads with the numbers that decide whether the run
    /// means anything, and never rounds an error into a pass.
    pub fn human(&self) -> String {
        let mut s = String::new();
        let d = &self.denominator;
        let c = &self.counts;

        s.push_str("=== DIFFERENTIAL ORACLE: C softIoc vs Rust IOC ===\n\n");

        s.push_str("DENOMINATOR (from the .dbd, not hand-listed)\n");
        s.push_str(&format!("  spec                  : {}\n", d.dbd));
        s.push_str(&format!(
            "  record types in dbd   : {}\n",
            d.record_types_in_dbd
        ));
        s.push_str(&format!(
            "  ...implemented by port: {} (measured by booting each)\n",
            d.record_types_covered.len()
        ));
        if !d.record_types_unimplemented.is_empty() {
            s.push_str(&format!(
                "  ...NOT implemented    : {} — {}  (unmeasurable: nothing to diff against)\n",
                d.record_types_unimplemented.len(),
                d.record_types_unimplemented.join(", ")
            ));
        }
        s.push_str(&format!(
            "  CA-observable fields  : {}   <-- THE DENOMINATOR\n",
            d.observable_fields
        ));
        s.push_str(&format!(
            "  excluded (DBF_NOACCESS): {} (no CA client can reach these)\n",
            d.excluded_noaccess_fields
        ));
        if !d.excluded_undrivable_val.is_empty() {
            s.push_str(&format!(
                "  monitor drive excluded : {}  (the drive is the stimulus, not the reading, so a\n\
                 {:26}VAL the .dbd says no client may write leaves that denominator)\n",
                d.excluded_undrivable_val.len(),
                ""
            ));
            for u in &d.excluded_undrivable_val {
                s.push_str(&format!("      {:14} {}\n", u.record_type, u.why));
            }
        }
        s.push('\n');

        let fc = &self.field_coverage;
        s.push_str("COVERAGE\n");
        s.push_str(&format!(
            "  fields measured on BOTH sides: {}/{} = {:.1}%\n",
            fc.measured,
            fc.enumerated,
            fc.percent()
        ));
        s.push_str(&format!(
            "  fields that errored (NOT coverage): {}\n\n",
            fc.errored
        ));

        s.push_str("CASES\n");
        s.push_str(&format!("  ran                : {}\n", c.ran));
        s.push_str(&format!("  agreed             : {}\n", c.agreed));
        s.push_str(&format!(
            "  expected deviation : {}  (allowlisted against expected-deviations.toml)\n",
            c.expected_deviation
        ));
        s.push_str(&format!("  DEFECT             : {}\n", c.defect));
        if c.neither_completed > 0 {
            s.push_str(&format!(
                "  no completion      : {}  (agreed on every surface; NEITHER side \
                 reported the put complete)\n",
                c.neither_completed
            ));
        }
        s.push_str(&format!(
            "  ERROR              : {}  (could not run — never counted as agreement)\n",
            c.errored
        ));
        match c.check() {
            Ok(()) => s.push_str("  (buckets reconcile with `ran`)\n\n"),
            Err(e) => s.push_str(&format!("  !!! {e}\n\n")),
        }

        if !self.stale_allowlist_rows.is_empty() {
            s.push_str("STALE ALLOWLIST ROWS (the deviation stopped happening — investigate)\n");
            for r in &self.stale_allowlist_rows {
                s.push_str(&format!(
                    "  {} — its scope WAS exercised and it still never fired. Either the port\n     \
                     regressed back onto C's bug, or C fixed it upstream. Both are findings.\n",
                    r.id
                ));
            }
            s.push('\n');
        }

        if !self.unexercised_allowlist_rows.is_empty() {
            s.push_str("UNEXERCISED ALLOWLIST ROWS (not findings — this run never drove them)\n");
            for r in &self.unexercised_allowlist_rows {
                s.push_str(&format!(
                    "  {} — no case in its scope ran, or none of its surfaces was compared.\n     \
                     Its silence measures nothing. {} to judge it.\n",
                    r.id,
                    r.driven_by_phrase()
                ));
            }
            s.push('\n');
        }

        let devs: Vec<_> = self.deviations().collect();
        if !devs.is_empty() {
            s.push_str("EXPECTED DEVIATIONS (port deliberately refuses C's bug)\n");
            for case in devs.iter().take(20) {
                s.push_str(&format!(
                    "  {} [{}]\n",
                    case.id(),
                    case.allowlisted.join(",")
                ));
            }
            s.push('\n');
        }

        let defects: Vec<_> = self.defects().collect();
        if !defects.is_empty() {
            s.push_str(&format!("DEFECTS ({})\n", defects.len()));
            for case in defects.iter().take(40) {
                s.push_str(&format!("\n  [{}]\n", case.id()));
                for diff in &case.differences {
                    s.push_str(&format!(
                        "    {:<14} C={:<28} port={}\n",
                        diff.surface.as_str(),
                        truncate(&diff.c, 28),
                        truncate(&diff.rust, 40)
                    ));
                }
                s.push_str(&case.reproducer.render("      "));
            }
            if defects.len() > 40 {
                s.push_str(&format!(
                    "\n  ... and {} more (see JSON)\n",
                    defects.len() - 40
                ));
            }
            s.push('\n');
        }

        s.push_str(&errors_section(
            "cases",
            self.errors().map(|e| e.errors.as_slice()).collect(),
        ));

        // The printed verdict comes from the same owner as the exit code, so a
        // run cannot exit non-zero while its report says it was clean — or,
        // worse, print "every case ran" while a whole record type went dark.
        let failures = run_failures(c, &d.record_types_unimplemented, &self.stale_allowlist_rows);
        if failures.is_empty() {
            s.push_str(
                "No defects, no errors, every record type implemented. \
                 Every case ran and was adjudicated.\n",
            );
        } else {
            s.push_str("RUN FAILED\n");
            for f in &failures {
                s.push_str(&format!("  - {f}\n"));
            }
        }
        s
    }
}

/// How much of one ERROR cause a human report prints.
///
/// A cause is the only explanation the report offers for every channel it
/// names — a boot failure stamps one onto every field of a record type — and
/// at 110 it was cut exactly where the cause lives, at the end: a boot reason
/// leads with the step that failed and closes with the IOC's own last words
/// ([`crate::ioc::OutputTail`]), so 110 characters bought the step and lost
/// the words.
///
/// Measured on this host, rendered as the section prints it: a
/// `verify_sole_server` failure quoting a whole `softIocPVX` tail is 314
/// characters (3 tail lines, 107 of them) and the `oracle-ioc --pva` side's is
/// 232 (1 line, 25); five of the C-side shape, which is what a `retry_pair`
/// exhaustion joins, is about 1,635. This holds all of them.
///
/// It stays a bound rather than becoming none, because a tool's stderr is not
/// bounded by anything this crate owns.
const CAUSE_CHARS: usize = 2000;

/// The ERRORS section of a human report: the causes, grouped, with every side
/// of a cause on its own line. Empty when nothing errored.
///
/// **The single owner of that rendering**, shared by the CA report and both
/// PVA ones, because writing the rule down once per report is exactly how it
/// drifted apart. Grouping is needed at all because a boot failure produces
/// one error per field, and printing all of them would bury the cause.
///
/// The key is the case's WHOLE error list, not its first entry. Keying on
/// `.first()` renders a two-sided failure as one-sided, which is how 464 `sub`
/// cases that timed out on BOTH sides read as C-only until the JSON was
/// opened — and the two PVA reports still keyed that way long after the CA one
/// stopped. Sidedness is the one thing this section exists to show, so it
/// cannot be a casualty of the key; each side is truncated on its own line,
/// because truncating the joined string would drop the second side all over
/// again.
pub(crate) fn errors_section(noun: &str, causes: Vec<&[ToolError]>) -> String {
    if causes.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "ERRORS ({}) — {noun} that could NOT be measured\n",
        causes.len()
    );
    let mut by_msg: std::collections::BTreeMap<Vec<String>, usize> = Default::default();
    for errors in &causes {
        let key: Vec<String> = if errors.is_empty() {
            vec!["(unknown)".into()]
        } else {
            errors
                .iter()
                .map(|x| truncate(&x.to_string(), CAUSE_CHARS))
                .collect()
        };
        *by_msg.entry(key).or_default() += 1;
    }
    for (msgs, n) in by_msg.iter().take(20) {
        for (i, msg) in msgs.iter().enumerate() {
            if i == 0 {
                s.push_str(&format!("  {n:>5}x  {msg}\n"));
            } else {
                s.push_str(&format!("          {msg}\n"));
            }
        }
    }
    s.push('\n');
    s
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        return s;
    }
    let t: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{t}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Surface;
    use crate::ioc::Side;

    fn case(v: Verdict) -> CaseResult {
        CaseResult {
            record_type: "ai".into(),
            field: "VAL".into(),
            phase: CasePhase::Read,
            class: None,
            verdict: v,
            differences: vec![],
            allowlisted: vec![],
            errors: match v {
                Verdict::Errored => vec![ToolError {
                    side: crate::ioc::Side::Rust,
                    tool: "caget".into(),
                    message: "timed out".into(),
                }],
                _ => vec![],
            },
            reproducer: Reproducer {
                db: "record(ai, \"X\") {}".into(),
                ops: vec!["caget X.VAL".into()],
            },
            c_side: Observation::default(),
            rust_side: Observation::default(),
        }
    }

    #[test]
    fn counts_reconcile_with_ran() {
        let cases = vec![
            case(Verdict::Agreed),
            case(Verdict::Agreed),
            case(Verdict::Defect),
            case(Verdict::ExpectedDeviation),
            case(Verdict::Errored),
        ];
        let c = Counts::tally(&cases);
        assert_eq!(c.ran, 5);
        assert_eq!(c.agreed, 2);
        assert_eq!(c.defect, 1);
        assert_eq!(c.expected_deviation, 1);
        assert_eq!(c.errored, 1);
        c.check().expect("buckets must reconcile");
    }

    /// A failure that hit BOTH sides must render as two-sided.
    ///
    /// Keying the ERRORS grouping on `errors.first()` printed only the C half of
    /// 464 `sub` cases whose put callback never completed on either side, and a
    /// reader of that report would have concluded the Rust IOC answered.
    #[test]
    fn a_two_sided_failure_renders_both_sides() {
        let mut both = case(Verdict::Errored);
        both.errors = vec![
            ToolError {
                side: crate::ioc::Side::C,
                tool: "caput".into(),
                message: "Write callback operation timed out".into(),
            },
            ToolError {
                side: crate::ioc::Side::Rust,
                tool: "caput".into(),
                message: "Write callback operation timed out".into(),
            },
        ];
        let cases = vec![both];
        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 1,
                record_types_covered: vec!["ai".into()],
                record_types_unimplemented: vec![],
                observable_fields: 1,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: field_coverage(&cases, 1),
            counts: Counts::tally(&cases),
            stale_allowlist_rows: vec![],
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases,
        };
        let h = rep.human();
        assert!(
            h.contains("[C] caput: Write callback operation timed out"),
            "the C side must show. got:\n{h}"
        );
        assert!(
            h.contains("[rust] caput: Write callback operation timed out"),
            "the rust side must show too, or the failure reads as one-sided. got:\n{h}"
        );
    }

    /// The guard that catches a harness silently dropping cases.
    #[test]
    fn a_lost_case_makes_the_counts_fail_to_reconcile() {
        let bad = Counts {
            ran: 10,
            agreed: 4,
            expected_deviation: 1,
            defect: 1,
            neither_completed: 0,
            errored: 1,
        };
        let err = bad.check().unwrap_err();
        assert!(err.contains("do not reconcile"), "got: {err}");
    }

    /// An errored case must never be reported as agreement, and must not be
    /// counted as coverage.
    /// A cause that belongs to NEITHER side names both sides.
    ///
    /// The formerly-bypassing path: both PVA reports keyed this grouping on
    /// `errors.first()` long after the CA one stopped, so a pair that never
    /// booted — one `ToolError` per side — printed as C-only, and a reader
    /// would have hunted a C-side fault that did not exist.
    #[test]
    fn a_cause_belonging_to_neither_side_names_both() {
        let errors =
            crate::ioc::BootError::neither("both sides drew one number").tool_errors("boot");
        let block = errors_section("channels", vec![errors.as_slice(); 56]);
        assert!(
            block.starts_with("ERRORS (56) — channels that could NOT be measured\n"),
            "{block}"
        );
        assert!(
            block.contains("[C] boot: both sides drew one number"),
            "{block}"
        );
        assert!(
            block.contains("[rust] boot: both sides drew one number"),
            "{block}"
        );
        // One group, counted once, with the second side on its own line.
        assert_eq!(block.matches("56x").count(), 1, "{block}");
    }

    /// Distinct causes stay distinct: grouping must not merge two faults.
    #[test]
    fn distinct_causes_are_counted_separately() {
        let a = vec![terr(Side::C, "pvxget", "Timeout")];
        let b = vec![terr(Side::Rust, "pvxinfo", "no such channel")];
        let block = errors_section("cases", vec![a.as_slice(), a.as_slice(), b.as_slice()]);
        assert!(block.contains("    2x  [C] pvxget: Timeout"), "{block}");
        assert!(
            block.contains("    1x  [rust] pvxinfo: no such channel"),
            "{block}"
        );
    }

    fn terr(side: Side, tool: &str, message: &str) -> ToolError {
        ToolError {
            side,
            tool: tool.into(),
            message: message.into(),
        }
    }

    #[test]
    fn errors_are_not_agreement_and_not_coverage() {
        let cases = vec![case(Verdict::Errored), case(Verdict::Agreed)];
        let c = Counts::tally(&cases);
        assert_eq!(c.agreed, 1, "the errored case must not inflate `agreed`");
        let cov = Coverage {
            enumerated: 2,
            measured: 1,
            errored: 1,
        };
        assert!((cov.percent() - 50.0).abs() < 1e-9, "50%, not 100%");
    }

    /// A monitor case must not be able to pay for a read case's measurement
    /// failure.
    ///
    /// Both phases leave `class` at `None`, so selecting the read probe by
    /// "no boundary class" swept the monitor cases into the field-coverage
    /// fraction. One field then failed to read while one monitor agreed, and
    /// the two cancelled: the instrument printed full coverage over a surface
    /// it had not managed to look at. The exit code still failed the run on the
    /// ERROR, which is exactly what made the coverage line the lie — a reader
    /// reconciling "why did this fail" against "100 % measured" is told the
    /// failure was somewhere the harness had already covered.
    #[test]
    fn a_monitor_agreement_cannot_mask_a_read_measurement_failure() {
        let mut read_err = case(Verdict::Errored);
        read_err.field = "HIHI".into();
        let mut mon_ok = case(Verdict::Agreed);
        mon_ok.phase = CasePhase::Monitor;
        let cases = vec![case(Verdict::Agreed), read_err, mon_ok];

        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 1,
                record_types_covered: vec!["ai".into()],
                record_types_unimplemented: vec![],
                observable_fields: 2,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: field_coverage(&cases, 2),
            counts: Counts::tally(&cases),
            stale_allowlist_rows: vec![],
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases,
        };
        let h = rep.human();
        assert!(
            h.contains("1/2 = 50.0%"),
            "one of two fields was read; the monitor case is not a field. got:\n{h}"
        );
        assert!(
            h.contains("fields that errored (NOT coverage): 1"),
            "the read failure must survive in the number. got:\n{h}"
        );
        assert!(h.contains("RUN FAILED"), "an ERROR case fails the run");
        assert_eq!(
            exit_status(&run_failures(&rep.counts, &[], &[])),
            1,
            "and the exit code says the same"
        );
    }

    /// Two probes measuring the same field must be two rows a reader can tell
    /// apart, in the human report and in the JSON alike.
    ///
    /// The read and monitor phases both drive `VAL` carrying no boundary class,
    /// so `ai.VAL` named both. A DEFECT found by the monitor probe and one found
    /// by the read probe then printed under the same identifier: the reader who
    /// pastes it back re-runs the wrong experiment, or reads two findings as one
    /// duplicated line.
    #[test]
    fn a_read_case_and_a_monitor_case_on_one_field_are_two_identifiable_findings() {
        let read = case(Verdict::Defect);
        let mut mon = case(Verdict::Defect);
        mon.phase = CasePhase::Monitor;
        assert_eq!(read.record_type, mon.record_type);
        assert_eq!(read.field, mon.field);
        assert_ne!(
            read.id(),
            mon.id(),
            "two probes of the same field must not share one identifier"
        );

        let cases = vec![read, mon];
        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 1,
                record_types_covered: vec!["ai".into()],
                record_types_unimplemented: vec![],
                observable_fields: 1,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: field_coverage(&cases, 1),
            counts: Counts::tally(&cases),
            stale_allowlist_rows: vec![],
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases,
        };

        let h = rep.human();
        assert!(
            h.contains("DEFECTS (2)"),
            "both defects are listed. got:\n{h}"
        );
        assert!(h.contains("[ai.VAL read]"), "the read case names its probe");
        assert!(
            h.contains("[ai.VAL monitor]"),
            "the monitor case names its probe. got:\n{h}"
        );
        assert!(h.contains("RUN FAILED"));
        assert_eq!(
            exit_status(&run_failures(&rep.counts, &[], &[])),
            1,
            "two DEFECTs fail the run"
        );

        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rep).expect("report serializes"))
                .expect("valid json");
        let arr = json["cases"].as_array().expect("cases is an array");
        assert_eq!(arr.len(), 2, "both cases survive into the JSON");
        assert_eq!(arr[0]["phase"], "read");
        assert_eq!(arr[1]["phase"], "monitor");
    }

    #[test]
    fn reproducer_renders_a_pasteable_db_and_ops() {
        let r = Reproducer {
            db: "record(calc, \"ORACLE:CALC:7\") {}\n".into(),
            ops: vec!["caput ORACLE:CALC:7.INPM '0'".into()],
        };
        let out = r.render("  ");
        assert!(out.contains("record(calc, \"ORACLE:CALC:7\") {}"));
        assert!(out.contains("caput ORACLE:CALC:7.INPM '0'"));
        assert!(out.contains("softIoc -S -d repro.db"));
    }

    #[test]
    fn human_summary_states_coverage_and_never_hides_errors() {
        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 34,
                record_types_covered: vec!["ai".into()],
                record_types_unimplemented: vec!["aai".into()],
                observable_fields: 100,
                excluded_noaccess_fields: 20,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: Coverage {
                enumerated: 100,
                measured: 60,
                errored: 40,
            },
            counts: Counts::tally(&[case(Verdict::Errored), case(Verdict::Defect)]),
            stale_allowlist_rows: vec![StaleRow {
                id: "CBUG-E1".into(),
                why: "compress FIFO".into(),
                driven_by: vec!["--phase all"],
            }],
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases: vec![case(Verdict::Errored), case(Verdict::Defect)],
        };
        let h = rep.human();
        assert!(h.contains("60/100 = 60.0%"), "states coverage honestly");
        assert!(h.contains("THE DENOMINATOR"));
        assert!(h.contains("NOT implemented"), "names the unmeasurable gap");
        assert!(h.contains("STALE ALLOWLIST ROWS"));
        assert!(h.contains("ERROR"));
        assert!(!h.contains("No defects, no errors"));
        assert!(
            h.contains("RUN FAILED"),
            "the printed verdict must match the exit code"
        );
    }

    /// The report's printed verdict and the exit code are the same judgement.
    /// A run with nothing wrong and nothing errored, but a record type that
    /// never loaded, printed "Every case ran and was adjudicated" and exited 0
    /// — a whole record type going dark, reported as a clean sweep.
    #[test]
    fn a_dark_record_type_fails_the_verdict_the_report_prints() {
        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 2,
                record_types_covered: vec!["ai".into()],
                record_types_unimplemented: vec!["calc".into()],
                observable_fields: 100,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: Coverage {
                enumerated: 100,
                measured: 20,
                errored: 0,
            },
            counts: Counts::tally(&[case(Verdict::Agreed)]),
            stale_allowlist_rows: vec![],
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases: vec![case(Verdict::Agreed)],
        };
        assert_eq!(rep.counts.defect, 0);
        assert_eq!(rep.counts.errored, 0);

        let h = rep.human();
        assert!(h.contains("RUN FAILED"), "{h}");
        assert!(h.contains("calc"), "the dark type must be named: {h}");
        assert!(!h.contains("Every case ran and was adjudicated"), "{h}");
        assert_eq!(
            exit_status(&run_failures(
                &rep.counts,
                &rep.denominator.record_types_unimplemented,
                &rep.stale_allowlist_rows,
            )),
            1,
        );
    }

    #[test]
    fn json_round_trips_the_differences() {
        let mut c = case(Verdict::Defect);
        c.differences = vec![Difference {
            surface: Surface::NativeType,
            c: "DBF_ULONG".into(),
            rust: "DBF_LONG".into(),
        }];
        let rep = Report {
            denominator: Denominator {
                dbd: "x".into(),
                record_types_in_dbd: 1,
                record_types_covered: vec![],
                record_types_unimplemented: vec![],
                observable_fields: 1,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: Coverage::default(),
            counts: Counts::tally(std::slice::from_ref(&c)),
            stale_allowlist_rows: vec![],
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases: vec![c],
        };
        let j = rep.to_json();
        assert!(j.contains("\"native_type\""));
        assert!(j.contains("DBF_ULONG"));
        assert!(j.contains("\"defect\""));
    }

    /// A run where every case AGREED but a record type never loaded has not
    /// measured that record type at all. Before this, the exit rule read only
    /// `defect` and `errored`, so the whole type going dark exited 0 and the
    /// run read as clean — the false-clean this harness exists to prevent,
    /// committed by the harness itself.
    #[test]
    fn an_unimplemented_record_type_fails_the_run_with_zero_defects_and_zero_errors() {
        let counts = Counts::tally(&[case(Verdict::Agreed), case(Verdict::Agreed)]);
        assert_eq!(counts.defect, 0);
        assert_eq!(counts.errored, 0);

        let failures = run_failures(&counts, &["aai".to_string(), "waveform".to_string()], &[]);
        assert_eq!(exit_status(&failures), 1, "the run must exit non-zero");
        assert_eq!(
            failures.len(),
            1,
            "one reason, naming the types: {failures:?}"
        );
        assert!(failures[0].contains("aai"), "{failures:?}");
        assert!(failures[0].contains("waveform"), "{failures:?}");
    }

    /// The other side of the same rule: with nothing wrong and nothing
    /// unmeasured, the run exits 0. Without this the fix above could be
    /// "always fail", which reports nothing at all.
    #[test]
    fn a_clean_fully_implemented_run_exits_zero() {
        let counts = Counts::tally(&[case(Verdict::Agreed), case(Verdict::ExpectedDeviation)]);
        let failures = run_failures(&counts, &[], &[]);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(exit_status(&failures), 0);
    }

    /// A justified deviation that stopped happening is a finding, and the run
    /// must fail on it. The row is driven stale through the real allowlist —
    /// the scope is exercised and nothing fires — rather than hand-built, so
    /// this pins the path a run actually takes.
    ///
    /// Before this, `stale_rows()` was correct, the report printed the row, and
    /// the exit code ignored it: the one place where "the harness noticed" and
    /// "the harness reported clean" coexisted.
    #[test]
    fn a_stale_allowlist_row_fails_the_run_with_zero_defects_and_zero_errors() {
        const F6: &str = r#"
schema = 1
[[deviation]]
id = "CBUG-F6"
bucket = "NOT-REPRODUCED"
record_types = ["calc"]
fields = ["INPM"]
surface = ["put_accepted"]
why = "C's special() rejects SPC_MOD; port accepts."
"#;
        let mut al = crate::allowlist::Allowlist::parse(F6).expect("fixture parses");
        let ctx = crate::allowlist::MatchContext {
            record_type: "calc",
            field: "INPM",
            dbf: crate::dbd::DbfType::InLink,
            class: Some("link-constant"),
        };
        // The run drove the put and the two sides agreed: the deviation the row
        // documents did not happen where the row points.
        al.note_compared(&ctx, &[Surface::PutAccepted]);
        let stale: Vec<StaleRow> = al.stale_rows().into_iter().map(StaleRow::of).collect();
        assert_eq!(stale.len(), 1, "the row must be stale, not unexercised");

        let counts = Counts::tally(&[case(Verdict::Agreed)]);
        assert_eq!(counts.defect, 0);
        assert_eq!(counts.errored, 0);

        let failures = run_failures(&counts, &[], &stale);
        assert_eq!(exit_status(&failures), 1, "{failures:?}");
        assert!(failures[0].contains("CBUG-F6"), "{failures:?}");

        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 1,
                record_types_covered: vec!["calc".into()],
                record_types_unimplemented: vec![],
                observable_fields: 10,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: Coverage {
                enumerated: 10,
                measured: 10,
                errored: 0,
            },
            counts,
            stale_allowlist_rows: stale,
            unexercised_allowlist_rows: vec![],
            fired_allowlist_rows: vec![],
            cases: vec![case(Verdict::Agreed)],
        };
        let h = rep.human();
        assert!(h.contains("RUN FAILED"), "{h}");
        assert!(!h.contains("Every case ran and was adjudicated"), "{h}");
    }

    /// An UNEXERCISED row must be told the phase that can actually drive it.
    ///
    /// `Phase::PvaRead` and `Phase::PvaMonitor` sit deliberately outside
    /// `Phase::All`, so for a row whose only surfaces are PVA ones the old
    /// advice — "widen the run (e.g. --phase all)" — named a phase that
    /// structurally cannot reach it, no matter how wide the record-type set is.
    /// Four of the five rows a `--phase all` run leaves unexercised on this
    /// tree are exactly that shape (CBUG-G1, DESIGN-ASYN-BOUT,
    /// INSTR-QSRV2-LONGIN-UTAG, INSTR-QSRV2-WAVEFORM-DEMO).
    #[test]
    fn an_unexercised_row_names_the_phase_that_can_drive_it() {
        const ROWS: &str = r#"
schema = 1
[[deviation]]
id = "CBUG-PVA1"
bucket = "NOT-REPRODUCED"
record_types = ["asyn"]
fields = ["BOUT"]
surface = ["value_marking"]
why = "PVA read surface only."

[[deviation]]
id = "CBUG-MON1"
bucket = "NOT-REPRODUCED"
record_types = ["bo"]
fields = []
surface = ["seed_event"]
why = "PVA monitor surface only."

[[deviation]]
id = "CBUG-CA1"
bucket = "NOT-REPRODUCED"
record_types = ["calc"]
fields = ["INPM"]
surface = ["put_accepted"]
why = "CA surface."

[[deviation]]
id = "CBUG-BOTH1"
bucket = "NOT-REPRODUCED"
record_types = ["waveform"]
fields = []
surface = ["value_string", "value_marking"]
why = "One row, both lanes."
"#;
        let al = crate::allowlist::Allowlist::parse(ROWS).expect("fixture parses");
        let rows: Vec<StaleRow> = al
            .unexercised_rows()
            .into_iter()
            .map(StaleRow::of)
            .collect();
        let by = |id: &str| {
            rows.iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("{id} is unexercised"))
                .clone()
        };

        assert_eq!(by("CBUG-PVA1").driven_by, ["--phase pvaread"]);
        assert_eq!(by("CBUG-MON1").driven_by, ["--phase pvamonitor"]);
        assert_eq!(by("CBUG-CA1").driven_by, ["--phase all"]);
        assert_eq!(
            by("CBUG-BOTH1").driven_by,
            ["--phase all", "--phase pvaread"]
        );

        let rep = Report {
            denominator: Denominator {
                dbd: "softIoc.dbd".into(),
                record_types_in_dbd: 1,
                record_types_covered: vec!["calc".into()],
                record_types_unimplemented: vec![],
                observable_fields: 1,
                excluded_noaccess_fields: 0,
                excluded_undrivable_val: Vec::new(),
            },
            field_coverage: Coverage {
                enumerated: 1,
                measured: 1,
                errored: 0,
            },
            counts: Counts::tally(&[case(Verdict::Agreed)]),
            stale_allowlist_rows: vec![],
            unexercised_allowlist_rows: rows,
            fired_allowlist_rows: vec![],
            cases: vec![case(Verdict::Agreed)],
        };
        let h = rep.human();
        let line = |id: &str| {
            h.lines()
                .skip_while(|l| !l.trim_start().starts_with(id))
                .nth(1)
                .unwrap_or_default()
                .to_string()
        };
        assert!(line("CBUG-PVA1").contains("`--phase pvaread`"), "{h}");
        assert!(
            !line("CBUG-PVA1").contains("--phase all"),
            "a PVA-only row must never be sent to --phase all: {h}"
        );
        assert!(line("CBUG-MON1").contains("`--phase pvamonitor`"), "{h}");
        assert!(line("CBUG-CA1").contains("`--phase all`"), "{h}");
    }

    /// The pre-existing half of the rule, kept asserted at the new owner: an
    /// unmeasurable case fails exactly like a wrong one.
    #[test]
    fn a_defect_and_an_error_each_fail_the_run_on_their_own() {
        let d = Counts::tally(&[case(Verdict::Defect)]);
        assert_eq!(exit_status(&run_failures(&d, &[], &[])), 1);
        let e = Counts::tally(&[case(Verdict::Errored)]);
        assert_eq!(exit_status(&run_failures(&e, &[], &[])), 1);
    }
}
