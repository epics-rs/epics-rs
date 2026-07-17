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

/// One adjudicated case.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseResult {
    pub record_type: String,
    pub field: String,
    /// The boundary class driven, for put cases.
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
    pub fn id(&self) -> String {
        match &self.class {
            Some(c) => format!("{}.{}[{}]", self.record_type, self.field, c),
            None => format!("{}.{}", self.record_type, self.field),
        }
    }
}

/// The buckets. They must reconcile.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Counts {
    pub ran: usize,
    pub agreed: usize,
    pub expected_deviation: usize,
    pub defect: usize,
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
                Verdict::Errored => c.errored += 1,
            }
        }
        c
    }

    /// Every case landed in exactly one bucket. If this ever fails, the harness
    /// has lost cases and no number it prints can be trusted.
    pub fn check(&self) -> Result<(), String> {
        let sum = self.agreed + self.expected_deviation + self.defect + self.errored;
        if sum != self.ran {
            return Err(format!(
                "counts do not reconcile: ran={} but buckets sum to {sum}",
                self.ran
            ));
        }
        Ok(())
    }
}

/// A stale allowlist row: a justified deviation that stopped happening.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleRow {
    pub id: String,
    pub why: String,
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
            "  excluded (DBF_NOACCESS): {} (no CA client can reach these)\n\n",
            d.excluded_noaccess_fields
        ));

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
            "  expected deviation : {}  (allowlisted against doc/upstream-c-bugs.md)\n",
            c.expected_deviation
        ));
        s.push_str(&format!("  DEFECT             : {}\n", c.defect));
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
                     Its silence measures nothing. Widen the run (e.g. --phase all) to judge it.\n",
                    r.id
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

        let errs: Vec<_> = self.errors().collect();
        if !errs.is_empty() {
            s.push_str(&format!(
                "ERRORS ({}) — cases that could NOT be measured\n",
                errs.len()
            ));
            // Group by message: a boot failure produces one error per field and
            // printing all of them would bury the cause.
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
                s.push_str(&format!("  {n:>5}x  {}\n", truncate(msg, 110)));
            }
            s.push('\n');
        }

        if c.defect == 0 && c.errored == 0 {
            s.push_str("No defects, no errors. Every case ran and was adjudicated.\n");
        }
        s
    }
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

    fn case(v: Verdict) -> CaseResult {
        CaseResult {
            record_type: "ai".into(),
            field: "VAL".into(),
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

    /// The guard that catches a harness silently dropping cases.
    #[test]
    fn a_lost_case_makes_the_counts_fail_to_reconcile() {
        let bad = Counts {
            ran: 10,
            agreed: 4,
            expected_deviation: 1,
            defect: 1,
            errored: 1,
        };
        let err = bad.check().unwrap_err();
        assert!(err.contains("do not reconcile"), "got: {err}");
    }

    /// An errored case must never be reported as agreement, and must not be
    /// counted as coverage.
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
}
