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
//! - a row that never fired -> **STALE**, and reported. The deviation has
//!   vanished: either the port regressed back onto C's bug, or C fixed it
//!   upstream. Both are findings. A harness that only checked the first two
//!   would let a silent regression sit forever behind a stale justification.

use std::collections::BTreeSet;
use std::path::Path;

use crate::diff::Difference;

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

/// The loaded allowlist, plus which rows actually fired during the run.
#[derive(Debug, Clone)]
pub struct Allowlist {
    pub rows: Vec<Deviation>,
    fired: BTreeSet<String>,
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
        })
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

    /// Enabled rows that never matched anything during the run.
    ///
    /// Each is a finding: the deviation it describes no longer happens. Either
    /// the port regressed onto C's bug (bad) or C fixed it upstream (good, and
    /// the catalogue entry should be retired). Reported either way.
    pub fn stale_rows(&self) -> Vec<&Deviation> {
        self.rows
            .iter()
            .filter(|r| r.enabled && !self.fired.contains(&r.id))
            .collect()
    }

    pub fn fired_rows(&self) -> &BTreeSet<String> {
        &self.fired
    }
}

impl Deviation {
    fn matches(&self, ctx: &MatchContext<'_>, d: &Difference) -> bool {
        // Each stated constraint must hold; each omitted one is a wildcard.
        let ok = |list: &[String], v: &str| list.is_empty() || list.iter().any(|x| x == v);

        ok(&self.record_types, ctx.record_type)
            && ok(&self.fields, ctx.field)
            && ok(&self.surface, d.surface.as_str())
            && match ctx.class {
                Some(c) => ok(&self.classes, c),
                // A case with no boundary class (a pure read probe) can only
                // match a row that does not constrain classes.
                None => self.classes.is_empty(),
            }
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

    #[test]
    fn a_row_that_never_fires_is_reported_stale() {
        let mut al = Allowlist::parse(F6).unwrap();
        assert_eq!(al.stale_rows().len(), 1, "nothing fired yet");
        al.match_diff(
            &ctx("calc", "INPM", Some("link-constant")),
            &diff(Surface::PutAccepted, "false", "true"),
        );
        assert!(al.stale_rows().is_empty(), "it fired, so it is not stale");
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
        assert!(al.stale_rows().is_empty(), "disabled rows are not stale");
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
