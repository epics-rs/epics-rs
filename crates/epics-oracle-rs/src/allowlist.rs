//! The expected-deviation allowlist: `doc/upstream-c-bugs.md`, made executable.
//!
//! Under the product policy (`doc/strategy-2026-07-13.md` §2) a C-vs-port
//! difference is not automatically a port bug — the port deliberately refuses
//! to reproduce C's bugs. So the oracle needs to know which differences are
//! *justified*, and the justification already exists as a catalogue. This
//! module turns the NOT-REPRODUCED entries into matchable rules.
//!
//! The rules live in `allowlist/expected-deviations.toml`, not inline in this
//! file, so the data is reviewable next to the catalogue it transcribes and can
//! be edited without touching harness code.
//!
//! # The two artifacts check each other
//!
//! - a diff matching a row -> **EXPECTED DEVIATION** (not a failure)
//! - a diff matching no row -> **PORT DEFECT**
//! - a row the run DROVE and that still never fired -> **STALE**, and reported.
//!   The deviation has vanished: either the port regressed back onto C's bug, or
//!   C fixed it upstream. Both are findings. A harness that only checked the
//!   first two would let a silent regression sit forever behind a stale
//!   justification.
//! - a row the run never drove -> **UNEXERCISED**, and reported separately as
//!   coverage, never as a finding.
//!
//! That last split is load-bearing. Staleness is a claim about what we *saw*, so
//! it may only be asserted over a surface we actually looked at. A `--phase read`
//! run never drives a put, so a `put_accepted` row cannot fire — calling it stale
//! would fabricate a finding out of a narrowed scope, and a check that cries wolf
//! is one people learn to ignore, which costs exactly the silent regressions it
//! exists to catch. "I could not look" and "I looked and it was fine" are
//! different answers here too, for the same reason ERROR is not AGREED.

use std::collections::BTreeSet;
use std::path::Path;

use crate::diff::{Difference, Surface};

/// One transcribed NOT-REPRODUCED entry, as a matchable rule.
///
/// Every constraint is optional and every omitted constraint is a wildcard, so
/// a row is exactly as narrow as it was written. That is deliberate: a row that
/// is too broad would swallow genuine defects as "expected", which is the one
/// failure this whole mechanism exists to prevent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Deviation {
    /// The CBUG id in `doc/upstream-c-bugs.md` that justifies this row.
    pub id: String,
    /// `NOT-REPRODUCED` (a live deviation) or `REPRODUCED` (recorded but not
    /// expected to fire today — see `enabled`).
    pub bucket: String,
    #[serde(default)]
    pub upstream: String,
    /// Why this difference is legitimate. Required — a row without a
    /// justification is not an allowlist entry, it is a suppression.
    pub why: String,
    #[serde(default)]
    pub record_types: Vec<String>,
    #[serde(default)]
    pub fields: Vec<String>,
    /// Observable surfaces this deviation may show up on (`diff::Surface`).
    #[serde(default)]
    pub surface: Vec<String>,
    /// Boundary classes (from `cases.rs`) this deviation is limited to.
    #[serde(default)]
    pub classes: Vec<String>,
    /// A REPRODUCED entry is carried in the file for traceability but must NOT
    /// match anything: the port reproduces C, so the oracle must see agreement.
    /// If it ever fires, that is itself a finding.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, serde::Deserialize)]
struct File {
    #[serde(default)]
    schema: u32,
    #[serde(default, rename = "deviation")]
    deviations: Vec<Deviation>,
}

/// The loaded allowlist, plus which rows fired during the run and which rows the
/// run was even in a position to fire.
#[derive(Debug, Clone)]
pub struct Allowlist {
    pub rows: Vec<Deviation>,
    fired: BTreeSet<String>,
    /// Rows whose scope was actually observed: a case in the row's
    /// record/field/class scope ran, and one of the row's surfaces was compared
    /// on it. A row outside this set was never given the chance to fire, so its
    /// silence says nothing.
    exercised: BTreeSet<String>,
}

/// The context a difference occurred in, which the rules match against.
#[derive(Debug, Clone)]
pub struct MatchContext<'a> {
    pub record_type: &'a str,
    pub field: &'a str,
    /// The boundary class driven, if this case drove a put.
    pub class: Option<&'a str>,
}

impl Allowlist {
    /// The allowlist that ships with the harness.
    pub fn default_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("allowlist/expected-deviations.toml")
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read allowlist {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let f: File = toml::from_str(text).map_err(|e| format!("bad allowlist TOML: {e}"))?;
        if f.schema != 1 {
            return Err(format!("unsupported allowlist schema {}", f.schema));
        }
        for d in &f.deviations {
            if d.why.trim().is_empty() {
                return Err(format!(
                    "allowlist row {} has no justification — an allowlist entry \
                     without a `why` is a suppression, not an expected deviation",
                    d.id
                ));
            }
        }
        Ok(Self {
            rows: f.deviations,
            fired: BTreeSet::new(),
            exercised: BTreeSet::new(),
        })
    }

    /// Record that a case ran and that these surfaces were compared on it —
    /// whether they agreed or not.
    ///
    /// This is what makes [`Self::stale_rows`] honest. A row is only expected to
    /// fire if the run actually looked where the row points; a `--phase read` run
    /// never drives a put, so a `put_accepted` row that stays silent has told us
    /// nothing at all.
    pub fn note_compared(&mut self, ctx: &MatchContext<'_>, compared: &[Surface]) {
        for row in &self.rows {
            if row.enabled
                && row.in_scope(ctx)
                && compared.iter().any(|s| row.covers_surface(*s))
                && !self.exercised.contains(&row.id)
            {
                self.exercised.insert(row.id.clone());
            }
        }
    }

    /// Find the row that justifies this difference, if any, and record that it
    /// fired.
    pub fn match_diff(&mut self, ctx: &MatchContext<'_>, d: &Difference) -> Option<String> {
        let hit = self
            .rows
            .iter()
            .find(|row| row.enabled && row.matches(ctx, d))
            .map(|row| row.id.clone())?;
        self.fired.insert(hit.clone());
        Some(hit)
    }

    /// Enabled rows whose scope the run DID observe, and which still never fired.
    ///
    /// Each is a finding: the deviation it describes no longer happens. Either
    /// the port regressed onto C's bug (bad) or C fixed it upstream (good, and
    /// the catalogue entry should be retired). Reported either way.
    ///
    /// A row the run never exercised is NOT stale — see [`Self::unexercised_rows`].
    /// Conflating the two turns every scoped run into a source of fabricated
    /// findings, and a check that cries wolf is a check people learn to ignore.
    pub fn stale_rows(&self) -> Vec<&Deviation> {
        self.rows
            .iter()
            .filter(|r| r.enabled && !self.fired.contains(&r.id) && self.exercised.contains(&r.id))
            .collect()
    }

    /// Enabled rows the run never put in a position to fire — no case in their
    /// scope ran, or none of their surfaces was compared.
    ///
    /// Not a finding. It is coverage: these deviations went unmeasured, and a run
    /// that could not look must not be read as a run that looked and saw nothing.
    pub fn unexercised_rows(&self) -> Vec<&Deviation> {
        self.rows
            .iter()
            .filter(|r| r.enabled && !self.fired.contains(&r.id) && !self.exercised.contains(&r.id))
            .collect()
    }

    pub fn fired_rows(&self) -> &BTreeSet<String> {
        &self.fired
    }
}

impl Deviation {
    fn matches(&self, ctx: &MatchContext<'_>, d: &Difference) -> bool {
        self.in_scope(ctx) && self.covers_surface(d.surface)
    }

    /// Everything about a row except the surface: does this row point at the case
    /// that just ran?
    ///
    /// Split out of [`Self::matches`] so that "did the run look here?" and "did the
    /// row fire?" are decided by ONE rule. If they drifted apart, a row could be
    /// judged stale against a scope it was never matched under.
    fn in_scope(&self, ctx: &MatchContext<'_>) -> bool {
        // Each stated constraint must hold; each omitted one is a wildcard.
        let ok = |list: &[String], v: &str| list.is_empty() || list.iter().any(|x| x == v);

        ok(&self.record_types, ctx.record_type)
            && ok(&self.fields, ctx.field)
            && match ctx.class {
                Some(c) => ok(&self.classes, c),
                // A case with no boundary class (a pure read probe) can only
                // match a row that does not constrain classes.
                None => self.classes.is_empty(),
            }
    }

    fn covers_surface(&self, s: Surface) -> bool {
        self.surface.is_empty() || self.surface.iter().any(|x| x == s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Surface;

    fn diff(surface: Surface, c: &str, r: &str) -> Difference {
        Difference {
            surface,
            c: c.into(),
            rust: r.into(),
        }
    }

    fn ctx<'a>(rt: &'a str, f: &'a str, class: Option<&'a str>) -> MatchContext<'a> {
        MatchContext {
            record_type: rt,
            field: f,
            class,
        }
    }

    /// The real CBUG-F6 row: C rejects puts to calc INPM..INPU, the port accepts.
    const F6: &str = r#"
schema = 1
[[deviation]]
id = "CBUG-F6"
bucket = "NOT-REPRODUCED"
record_types = ["calc", "calcout"]
fields = ["INPM", "INPN"]
surface = ["put_accepted"]
why = "C's special() rejects SPC_MOD; port accepts. Documented fields unwritable in C."
"#;

    #[test]
    fn a_matching_diff_is_an_expected_deviation() {
        let mut al = Allowlist::parse(F6).unwrap();
        let hit = al.match_diff(
            &ctx("calc", "INPM", Some("link-constant")),
            &diff(Surface::PutAccepted, "false", "true"),
        );
        assert_eq!(hit.as_deref(), Some("CBUG-F6"));
    }

    #[test]
    fn the_same_diff_on_an_unlisted_field_is_a_defect_not_a_deviation() {
        let mut al = Allowlist::parse(F6).unwrap();
        // INPA is NOT in the row's field list -- C writes it fine, so a
        // put_accepted diff there is a genuine port defect.
        assert!(
            al.match_diff(
                &ctx("calc", "INPA", Some("link-constant")),
                &diff(Surface::PutAccepted, "false", "true")
            )
            .is_none()
        );
    }

    #[test]
    fn the_same_diff_on_an_unlisted_record_type_is_a_defect() {
        let mut al = Allowlist::parse(F6).unwrap();
        assert!(
            al.match_diff(
                &ctx("ai", "INPM", Some("link-constant")),
                &diff(Surface::PutAccepted, "false", "true")
            )
            .is_none()
        );
    }

    /// A row justifies a difference on the surface it names -- and only there.
    /// A value difference on the same field is a separate, unjustified finding.
    #[test]
    fn a_row_does_not_launder_a_diff_on_a_different_surface() {
        let mut al = Allowlist::parse(F6).unwrap();
        assert!(
            al.match_diff(
                &ctx("calc", "INPM", Some("link-constant")),
                &diff(Surface::ValueString, "1", "2")
            )
            .is_none()
        );
    }

    /// STALE means "we drove it and the deviation did not happen". The run must
    /// have LOOKED: a row is only stale once a case in its scope was compared on
    /// one of its surfaces and still agreed.
    #[test]
    fn a_row_whose_scope_was_exercised_and_did_not_fire_is_stale() {
        let mut al = Allowlist::parse(F6).unwrap();
        let c = ctx("calc", "INPM", Some("link-constant"));

        al.note_compared(&c, &[Surface::PutAccepted]);
        assert_eq!(
            al.stale_rows().len(),
            1,
            "we drove the put and C and the port agreed — the deviation stopped"
        );
        assert!(al.unexercised_rows().is_empty());

        al.match_diff(&c, &diff(Surface::PutAccepted, "false", "true"));
        assert!(al.stale_rows().is_empty(), "it fired, so it is not stale");
    }

    /// The defect this split exists to kill: a `--phase read` run never drives a
    /// put, so a `put_accepted` row CANNOT fire. Calling that stale invents a
    /// finding out of a narrowed scope — and a check that cries wolf gets ignored,
    /// which costs the real regressions it exists to catch.
    #[test]
    fn a_row_the_run_never_drove_is_unexercised_not_stale() {
        let mut al = Allowlist::parse(F6).unwrap();

        // Nothing ran at all.
        assert!(
            al.stale_rows().is_empty(),
            "a run that did not look is not evidence"
        );
        assert_eq!(al.unexercised_rows().len(), 1);

        // A read-phase sweep over the very same field: every read surface is
        // compared, but the put surface the row names never is.
        let c = ctx("calc", "INPM", None);
        al.note_compared(
            &c,
            &[
                Surface::NativeType,
                Surface::ValueString,
                Surface::ValueNumeric,
                Surface::AccessRights,
            ],
        );
        assert!(
            al.stale_rows().is_empty(),
            "the row's surface was never compared — its silence measures nothing"
        );
        assert_eq!(al.unexercised_rows().len(), 1);
    }

    /// Exercise is scoped by record/field too, not just by surface.
    #[test]
    fn a_case_outside_the_rows_scope_does_not_exercise_it() {
        let mut al = Allowlist::parse(F6).unwrap();
        al.note_compared(
            &ctx("ai", "INPM", Some("link-constant")),
            &[Surface::PutAccepted],
        );
        assert!(
            al.stale_rows().is_empty(),
            "the row is scoped to calc/calcout; an ai case says nothing about it"
        );
        assert_eq!(al.unexercised_rows().len(), 1);
    }

    /// A REPRODUCED row must never match: the port carries C's bug on purpose,
    /// so the oracle must see agreement. If it fires, the port drifted.
    #[test]
    fn a_disabled_reproduced_row_never_matches_and_is_never_stale() {
        let toml = r#"
schema = 1
[[deviation]]
id = "CBUG-E2"
bucket = "REPRODUCED"
enabled = false
surface = ["value_string"]
why = "port reproduces C's bare cast on purpose; agreement expected today"
"#;
        let mut al = Allowlist::parse(toml).unwrap();
        assert!(
            al.match_diff(
                &ctx("ai", "VAL", Some("over-max")),
                &diff(Surface::ValueString, "-2147483648", "2147483647")
            )
            .is_none(),
            "a REPRODUCED row must not launder a diff into 'expected'"
        );
        al.note_compared(&ctx("ai", "VAL", Some("over-max")), &[Surface::ValueString]);
        assert!(al.stale_rows().is_empty(), "disabled rows are not stale");
        assert!(
            al.unexercised_rows().is_empty(),
            "nor unexercised — a disabled row is outside the ledger entirely"
        );
    }

    #[test]
    fn a_row_without_a_justification_is_rejected_as_a_suppression() {
        let toml = r#"
schema = 1
[[deviation]]
id = "CBUG-X1"
bucket = "NOT-REPRODUCED"
why = "  "
"#;
        let err = Allowlist::parse(toml).unwrap_err();
        assert!(err.contains("suppression"), "got: {err}");
    }

    /// The shipped file must parse and every row must cite a CBUG id.
    #[test]
    fn shipped_allowlist_parses_and_every_row_cites_a_cbug() {
        let al = Allowlist::load(&Allowlist::default_path()).expect("shipped allowlist parses");
        assert!(!al.rows.is_empty());
        for r in &al.rows {
            assert!(r.id.starts_with("CBUG-"), "row {:?} cites no CBUG", r.id);
            assert!(!r.why.trim().is_empty());
        }
    }
}
