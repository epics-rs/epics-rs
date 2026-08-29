//! The expected-deviation allowlist, made executable.
//!
//! Under the product policy a C-vs-port difference is not automatically a port
//! bug — the port deliberately refuses to reproduce C's bugs. So the oracle
//! needs to know which differences are *justified*, and the justification
//! already exists as a catalogue. This module turns the NOT-REPRODUCED entries
//! into matchable rules.
//!
//! The rules live in `allowlist/expected-deviations.toml`, not inline in this
//! file, so the data is reviewable next to the catalogue it transcribes and can
//! be edited without touching harness code.
//!
//! # Four buckets, three justification bases
//!
//! Most rows are C-bug refusals: a `NOT-REPRODUCED` (or, disabled, `REPRODUCED`)
//! entry, and each such row MUST cite the `CBUG-…` id that justifies it. Two
//! further buckets are justified by their `why` rather than by a catalogue
//! entry, and cite no CBUG:
//!
//! - `DESIGN-DIVERGENCE` — an *intentional port design choice* that is neither a
//!   C-bug refusal nor a port defect (the port deliberately does something other
//!   than the spec).
//! - `INSTRUMENT-SUPERSET` — the *opposite* direction: the port matches the
//!   measured spec, but the ground-truth **instrument** (`softIocPVX`, whose
//!   `.dbd` is a superset of the oracle's `softIoc.dbd`) serves more than the
//!   spec. The difference is an artifact of the instrument exceeding the spec,
//!   not of the port falling short of it.
//!
//! The CBUG-citation rule is relaxed for those two buckets, and only them; every
//! row still needs a non-empty `id` (it keys the fired/stale ledger) and a
//! non-empty `why`. All four buckets match, fire, and go stale by the same rules
//! — the bucket changes only what *justifies* the row, not how it behaves. See
//! [`Allowlist::parse`].
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

use crate::dbd::DbfType;
use crate::diff::{Difference, Surface};

/// A C-bug refusal expected to fire today. Requires a `CBUG-…` `id`.
pub const BUCKET_NOT_REPRODUCED: &str = "NOT-REPRODUCED";
/// A C bug the port still reproduces on purpose (carried disabled for
/// traceability; see `enabled`). Requires a `CBUG-…` `id`.
pub const BUCKET_REPRODUCED: &str = "REPRODUCED";
/// An intentional port design difference — neither a C-bug refusal nor a defect.
/// Justified by its `why` alone; the `CBUG-…` `id` requirement is relaxed for
/// this bucket only.
pub const BUCKET_DESIGN_DIVERGENCE: &str = "DESIGN-DIVERGENCE";
/// A ground-truth **instrument** superset: the port matches the measured spec,
/// but `softIocPVX` (whose `.dbd` is a superset of the oracle's `softIoc.dbd`)
/// serves more. Justified by its `why`; no `CBUG-…` `id` required.
pub const BUCKET_INSTRUMENT_SUPERSET: &str = "INSTRUMENT-SUPERSET";

/// One expected-deviation rule: a C-bug refusal, or an intentional port
/// design divergence.
///
/// Every constraint is optional and every omitted constraint is a wildcard, so
/// a row is exactly as narrow as it was written. That is deliberate: a row that
/// is too broad would swallow genuine defects as "expected", which is the one
/// failure this whole mechanism exists to prevent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Deviation {
    /// The row's identifier and ledger key. For a `NOT-REPRODUCED`/`REPRODUCED`
    /// row this is the `CBUG-…` id that justifies it;
    /// for a `DESIGN-DIVERGENCE` row it is a design tag that cites no CBUG. Always
    /// non-empty and unique — it keys the fired/exercised/stale sets.
    pub id: String,
    /// One of [`BUCKET_NOT_REPRODUCED`], [`BUCKET_REPRODUCED`],
    /// [`BUCKET_DESIGN_DIVERGENCE`], or [`BUCKET_INSTRUMENT_SUPERSET`]. Selects
    /// what justifies the row (a CBUG id vs. a design/instrument rationale); it
    /// does not affect matching. Validated in [`Allowlist::parse`].
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
    /// Declared `DBF_*` types of the destination field this deviation is limited
    /// to, in `.dbd` spelling.
    ///
    /// The third way to anchor a row to a part of the surface, and the only one
    /// that fits a deviation whose family is a *type* family rather than a place:
    /// CBUG-E2 is `dbConvert`'s double->integer cast, which reaches every integer
    /// and `epicsEnum16` destination of every record type and no float one. No
    /// `record_types`/`fields` list can say that without either lying by omission
    /// or restating the `.dbd`.
    #[serde(default)]
    pub dbf_types: Vec<String>,
    /// Observable surfaces this deviation may show up on (`diff::Surface`).
    #[serde(default)]
    pub surface: Vec<String>,
    /// Boundary classes (from `cases.rs`) this deviation is limited to.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Content constraint for a coarse-blob surface (the PVA `value_marking`
    /// surface is one whole `pvxget` rendering per channel, not a per-leaf
    /// difference). When non-empty, the deviation matches ONLY if the port's
    /// side adds one or more leaf lines whose path is listed here and the two
    /// renderings are otherwise identical — no line removed, no other line
    /// changed. This keeps a row that justifies "the port serves a leaf pvxs
    /// omits" (e.g. CBUG-G1's `display.precision`) from laundering any other
    /// marking difference on the same channel. Empty => no content constraint,
    /// the surface+scope match alone decides (the CA behaviour).
    #[serde(default)]
    pub port_adds_leaves: Vec<String>,
    /// The symmetric content constraint for the reversed direction: the GROUND
    /// TRUTH (reference) carries extra choices in a single string-array leaf
    /// (e.g. an `NTEnum` `value.choices`) that the port omits. When non-empty,
    /// the deviation matches ONLY if the sole difference between the two
    /// renderings is on one shared leaf line whose choice list on the reference
    /// is the port's list plus EXACTLY the names here — no other leaf changed, no
    /// choice removed, and the ground truth adds no choice beyond those named.
    /// This justifies "the instrument's `.dbd` is a superset of the measured
    /// spec" (QSRV2 example device support) without laundering any other marking
    /// difference. Mutually exclusive with `port_adds_leaves` (validated in
    /// [`Allowlist::parse`]).
    #[serde(default)]
    pub ground_truth_adds_choices: Vec<String>,
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
    /// The destination field's declared type, from the `.dbd`.
    ///
    /// Not optional. `record_type` + `field` name *where* a case ran but not
    /// *what it wrote into*, and a deviation about a conversion is about the
    /// destination type: `ai.VAL` and `longin.VAL` are the same place-shaped
    /// context and opposite answers for CBUG-E2. Every path that adjudicates
    /// supplies it, so a row's `dbf_types` can never be silently unenforced.
    pub dbf: DbfType,
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
        // parse() is the sole fallible constructor, so validating here makes the
        // per-bucket invariants hold for every `Allowlist` by construction, not
        // just for the shipped file.
        for d in &f.deviations {
            if d.id.trim().is_empty() {
                return Err(
                    "allowlist row has an empty `id` — every row needs a unique, \
                     non-empty id; it keys the fired/exercised/stale ledger"
                        .to_string(),
                );
            }
            match d.bucket.as_str() {
                // A C-bug refusal must name the CBUG it transcribes: the
                // catalogue is the justification, so a row that cites none is not
                // an allowlist entry, it is a bare suppression.
                BUCKET_NOT_REPRODUCED | BUCKET_REPRODUCED => {
                    if !d.id.starts_with("CBUG-") {
                        return Err(format!(
                            "allowlist row {} is bucket {} but does not cite a CBUG id — \
                             a {}/{} row transcribes a bug and must name it",
                            d.id, d.bucket, BUCKET_NOT_REPRODUCED, BUCKET_REPRODUCED
                        ));
                    }
                }
                // A design divergence or an instrument-superset artifact is
                // justified by its `why` (rationale), not by a catalogue entry:
                // the CBUG-citation rule is relaxed here, and here only.
                BUCKET_DESIGN_DIVERGENCE | BUCKET_INSTRUMENT_SUPERSET => {}
                other => {
                    return Err(format!(
                        "allowlist row {} has unknown bucket {other:?} — expected one of \
                         {BUCKET_NOT_REPRODUCED}, {BUCKET_REPRODUCED}, \
                         {BUCKET_DESIGN_DIVERGENCE}, {BUCKET_INSTRUMENT_SUPERSET}",
                        d.id
                    ));
                }
            }
            // The two content constraints point in opposite directions (port adds
            // vs. ground truth adds); a row that set both would have no coherent
            // meaning, so reject it rather than silently honour one.
            if !d.port_adds_leaves.is_empty() && !d.ground_truth_adds_choices.is_empty() {
                return Err(format!(
                    "allowlist row {} sets both `port_adds_leaves` and \
                     `ground_truth_adds_choices` — a row may use at most one content \
                     constraint; they describe opposite directions",
                    d.id
                ));
            }
            if d.why.trim().is_empty() {
                return Err(format!(
                    "allowlist row {} has no justification — an allowlist entry \
                     without a `why` is a suppression, not an expected deviation",
                    d.id
                ));
            }
            // Every `dbf_types` entry must be a real `.dbd` spelling, or the row
            // silently narrows to nothing and reads as a bound it does not have.
            for t in &d.dbf_types {
                if DbfType::from_dbd_name(t).is_none() {
                    return Err(format!(
                        "allowlist row {} names `{t}` in `dbf_types`, which is not a \
                         .dbd DBF_* type",
                        d.id
                    ));
                }
            }
            // A row must be anchored to some part of the surface. `surface` and
            // `classes` say which observable and which written value, not WHERE:
            // a row constraining neither the record type, nor the field, nor the
            // destination type applies to the whole denominator, and an
            // allowlist whose rows can do that justifies every difference the
            // run will ever find. That is the one failure this mechanism exists
            // to prevent, so it is rejected at load rather than measured around.
            if d.record_types.is_empty() && d.fields.is_empty() && d.dbf_types.is_empty() {
                return Err(format!(
                    "allowlist row {} is unbounded — it names no `record_types`, no \
                     `fields` and no `dbf_types`, so it matches the entire surface. \
                     A deviation must state where it happens.",
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

    /// An allowlist with no rows — for tests and for phases that adjudicate
    /// without loading the shipped file. Every difference is then unjustified,
    /// i.e. a DEFECT, exactly as before any allowlist existed.
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            fired: BTreeSet::new(),
            exercised: BTreeSet::new(),
        }
    }

    /// PVA analogue of [`Self::note_compared`]: mark rows exercised whose scope
    /// this case observed, keyed on PVA surface **name** strings rather than the
    /// CA [`Surface`] enum. Same honesty contract — a row is exercised only if a
    /// case in its scope ran and one of its surfaces was compared.
    pub fn note_compared_pva(&mut self, ctx: &MatchContext<'_>, compared: &[&str]) {
        let newly: Vec<String> = self
            .rows
            .iter()
            .filter(|row| {
                row.enabled
                    && row.in_scope(ctx)
                    && compared.iter().any(|s| row.covers_surface_str(s))
                    && !self.exercised.contains(&row.id)
            })
            .map(|row| row.id.clone())
            .collect();
        self.exercised.extend(newly);
    }

    /// PVA analogue of [`Self::match_diff`]: find the row that justifies this
    /// difference on a coarse-blob surface, given the full reference/observed
    /// renderings so a row's `port_adds_leaves` content constraint can be
    /// checked. Records that the row fired.
    pub fn match_pva_diff(
        &mut self,
        ctx: &MatchContext<'_>,
        surface: &str,
        reference: &str,
        observed: &str,
    ) -> Option<String> {
        let hit = self
            .rows
            .iter()
            .find(|row| row.enabled && row.matches_pva(ctx, surface, reference, observed))
            .map(|row| row.id.clone())?;
        self.fired.insert(hit.clone());
        Some(hit)
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
    /// CA match: scope + surface + the content constraint.
    ///
    /// Identical in shape to [`Self::matches_pva`], and that is the point. The
    /// content predicate used to be reachable only from the PVA path, so a CA
    /// row could claim a difference with no constraint at all on what the two
    /// sides actually said. One rule now decides both, with the CA
    /// [`Difference`]'s two readings standing in for the PVA renderings.
    fn matches(&self, ctx: &MatchContext<'_>, d: &Difference) -> bool {
        self.in_scope(ctx) && self.covers_surface(d.surface) && self.content_ok(&d.c, &d.rust)
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
            && ok(&self.dbf_types, ctx.dbf.as_dbd_name())
            && match ctx.class {
                Some(c) => ok(&self.classes, c),
                // A case with no boundary class (a pure read probe) can only
                // match a row that does not constrain classes.
                None => self.classes.is_empty(),
            }
    }

    fn covers_surface(&self, s: Surface) -> bool {
        self.covers_surface_str(s.as_str())
    }

    fn covers_surface_str(&self, s: &str) -> bool {
        self.surface.is_empty() || self.surface.iter().any(|x| x == s)
    }

    /// PVA match: scope + surface, plus the coarse-blob content constraint.
    ///
    /// The content constraint is what makes a `value_marking` row safe: the PVA
    /// surface is one whole `pvxget` rendering per channel, so a bare
    /// scope+surface match would justify *any* marking difference on the
    /// channel. `port_adds_leaves` narrows it to "the port added these leaf lines
    /// and nothing else moved"; `ground_truth_adds_choices` is its mirror, "the
    /// ground truth added exactly these choices to one leaf and nothing else
    /// moved". See [`Self::content_ok`].
    fn matches_pva(
        &self,
        ctx: &MatchContext<'_>,
        surface: &str,
        reference: &str,
        observed: &str,
    ) -> bool {
        self.in_scope(ctx)
            && self.covers_surface_str(surface)
            && self.content_ok(reference, observed)
    }

    /// The coarse-blob content predicate: dispatches on whichever content
    /// constraint the row declared. At most one may be set (enforced by
    /// [`Allowlist::parse`]); a row with neither imposes no constraint.
    fn content_ok(&self, reference: &str, observed: &str) -> bool {
        if !self.port_adds_leaves.is_empty() {
            return self.port_adds_leaves_ok(reference, observed);
        }
        if !self.ground_truth_adds_choices.is_empty() {
            return self.ground_truth_adds_choices_ok(reference, observed);
        }
        true
    }

    /// The port-adds-only content predicate.
    ///
    /// Both renderings are reduced to their set of non-empty trimmed lines
    /// (order-independent — pvxs and the port emit the leaves in different
    /// orders). The difference is justified iff every line the port added names
    /// a leaf in `port_adds_leaves`, at least one line was added, and no line
    /// was removed or changed.
    fn port_adds_leaves_ok(&self, reference: &str, observed: &str) -> bool {
        let refl = line_set(reference);
        let obsl = line_set(observed);
        let added: Vec<&String> = obsl.difference(&refl).collect();
        let removed = refl.difference(&obsl).count();
        removed == 0
            && !added.is_empty()
            && added.iter().all(|line| {
                let leaf = line.split_whitespace().next().unwrap_or("");
                self.port_adds_leaves.iter().any(|allowed| allowed == leaf)
            })
    }

    /// The ground-truth-adds-choices content predicate — the mirror of
    /// [`Self::port_adds_leaves_ok`].
    ///
    /// The difference is justified iff the SOLE differing line on each side is
    /// the same string-array leaf (one line only-in-reference, one only-in-
    /// observed, sharing a leaf path), the port's choice set is a subset of the
    /// reference's (nothing removed), and the choices the reference adds are
    /// EXACTLY `ground_truth_adds_choices` — no fewer (or the named superset did
    /// not actually appear) and no more (or some other choice is unaccounted
    /// for, which must stay a defect). Any other leaf differing fails the sole-
    /// difference test, so no unrelated marking difference is laundered.
    fn ground_truth_adds_choices_ok(&self, reference: &str, observed: &str) -> bool {
        let refl = line_set(reference);
        let obsl = line_set(observed);
        let only_ref: Vec<&String> = refl.difference(&obsl).collect();
        let only_obs: Vec<&String> = obsl.difference(&refl).collect();
        // Exactly one leaf line differs on each side...
        if only_ref.len() != 1 || only_obs.len() != 1 {
            return false;
        }
        let leaf = |l: &str| l.split_whitespace().next().unwrap_or("").to_string();
        // ...and it is the same leaf on both (a changed leaf, not a leaf that
        // one side dropped and the other invented).
        if leaf(only_ref[0]) != leaf(only_obs[0]) {
            return false;
        }
        let ref_choices = parse_choice_list(only_ref[0]);
        let port_choices = parse_choice_list(only_obs[0]);
        // The port removed no choice the reference kept...
        if !port_choices.is_subset(&ref_choices) {
            return false;
        }
        // ...and the reference's extras are exactly the named superset.
        let extra: BTreeSet<String> = ref_choices.difference(&port_choices).cloned().collect();
        let named: BTreeSet<String> = self.ground_truth_adds_choices.iter().cloned().collect();
        extra == named
    }
}

/// A rendering reduced to its set of non-empty trimmed lines. pvxs and the port
/// emit leaves in different orders, so content predicates compare line SETS.
fn line_set(s: &str) -> BTreeSet<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The quoted choice strings inside the `[...]` of a rendered string-array leaf
/// line, as a set. `value.choices string[] = {3}["A", "B"]` -> {"A", "B"}.
///
/// Splits on `"` delimiters, so choices containing commas or brackets are fine;
/// EPICS menu choice strings never contain an embedded quote, so no unescaping
/// is needed.
fn parse_choice_list(line: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = line;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        match after.find('"') {
            Some(close) => {
                out.insert(after[..close].to_string());
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    out
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

    fn ctx<'a>(rt: &'a str, f: &'a str, dbf: DbfType, class: Option<&'a str>) -> MatchContext<'a> {
        MatchContext {
            record_type: rt,
            field: f,
            dbf,
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
            &ctx("calc", "INPM", DbfType::InLink, Some("link-constant")),
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
                &ctx("calc", "INPA", DbfType::InLink, Some("link-constant")),
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
                &ctx("ai", "INPM", DbfType::InLink, Some("link-constant")),
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
                &ctx("calc", "INPM", DbfType::InLink, Some("link-constant")),
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
        let c = ctx("calc", "INPM", DbfType::InLink, Some("link-constant"));

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
        let c = ctx("calc", "INPM", DbfType::InLink, None);
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
            &ctx("ai", "INPM", DbfType::InLink, Some("link-constant")),
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
record_types = ["ai"]
surface = ["value_string"]
why = "port reproduces C's bare cast on purpose; agreement expected today"
"#;
        let mut al = Allowlist::parse(toml).unwrap();
        assert!(
            al.match_diff(
                &ctx("ai", "VAL", DbfType::Double, Some("over-max")),
                &diff(Surface::ValueString, "-2147483648", "2147483647")
            )
            .is_none(),
            "a REPRODUCED row must not launder a diff into 'expected'"
        );
        al.note_compared(
            &ctx("ai", "VAL", DbfType::Double, Some("over-max")),
            &[Surface::ValueString],
        );
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

    /// The shipped file must parse; every row needs a justification, and every
    /// C-bug-refusal row must still cite a CBUG id. DESIGN-DIVERGENCE and
    /// INSTRUMENT-SUPERSET rows are justified by their `why` alone and are exempt
    /// from the CBUG-citation rule.
    #[test]
    fn shipped_allowlist_parses_and_c_bug_rows_cite_a_cbug() {
        let al = Allowlist::load(&Allowlist::default_path()).expect("shipped allowlist parses");
        assert!(!al.rows.is_empty());
        for r in &al.rows {
            assert!(!r.id.trim().is_empty(), "every row needs an id");
            assert!(
                !r.why.trim().is_empty(),
                "row {:?} has no justification",
                r.id
            );
            match r.bucket.as_str() {
                BUCKET_NOT_REPRODUCED | BUCKET_REPRODUCED => {
                    assert!(
                        r.id.starts_with("CBUG-"),
                        "C-bug row {:?} cites no CBUG",
                        r.id
                    );
                }
                BUCKET_DESIGN_DIVERGENCE | BUCKET_INSTRUMENT_SUPERSET => {}
                other => panic!("row {:?} has unknown bucket {other:?}", r.id),
            }
        }
    }

    /// A DESIGN-DIVERGENCE row parses without a CBUG id and matches its scope
    /// exactly as a NOT-REPRODUCED row would — the bucket changes what justifies
    /// the row, not how it fires. This is the ASYN.BOUT shape.
    #[test]
    fn a_design_divergence_row_parses_and_matches_its_scope() {
        let toml = r#"
schema = 1
[[deviation]]
id = "DESIGN-ASYN-BOUT"
bucket = "DESIGN-DIVERGENCE"
record_types = ["asyn"]
fields = ["BOUT"]
surface = ["value_marking"]
why = "port lifts C's fixed 80-byte OMAX cap and serves the live written length"
"#;
        let mut al =
            Allowlist::parse(toml).expect("design-divergence row parses without a CBUG id");
        let bout = ctx("asyn", "BOUT", DbfType::Char, None);
        // The PVA value/marking contract is the surface a BOUT value diff reports
        // on (pvaread.rs). An empty `port_adds_leaves` means scope+surface
        // decides, and the field-level scope keeps it to this one channel.
        let hit = al.match_pva_diff(&bout, "value_marking", "epicsInt8_t[80]", "epicsInt8_t[0]");
        assert_eq!(hit.as_deref(), Some("DESIGN-ASYN-BOUT"));
        assert!(al.fired_rows().contains("DESIGN-ASYN-BOUT"));

        // Same diff on a field the row does not name is still unjustified.
        assert!(
            al.match_pva_diff(
                &ctx("asyn", "AOUT", DbfType::String, None),
                "value_marking",
                "a",
                "b"
            )
            .is_none(),
            "the row is scoped to BOUT; another field is a separate finding"
        );
    }

    /// The CBUG-citation invariant is scoped by bucket: a C-bug-refusal row
    /// without a CBUG id is rejected, a design-divergence row without one is
    /// accepted.
    #[test]
    fn the_cbug_citation_invariant_is_scoped_to_c_bug_buckets() {
        let not_reproduced_without_cbug = r#"
schema = 1
[[deviation]]
id = "NOPE-1"
bucket = "NOT-REPRODUCED"
why = "claims to refuse a C bug but names none"
"#;
        let err = Allowlist::parse(not_reproduced_without_cbug).unwrap_err();
        assert!(
            err.contains("does not cite a CBUG id"),
            "a NOT-REPRODUCED row without a CBUG id must fail: {err}"
        );

        // A design-divergence row with the same non-CBUG id is accepted.
        let design_without_cbug = r#"
schema = 1
[[deviation]]
id = "DESIGN-1"
bucket = "DESIGN-DIVERGENCE"
record_types = ["ai"]
why = "intentional port design choice, justified here rather than by a CBUG"
"#;
        assert!(Allowlist::parse(design_without_cbug).is_ok());

        // ...and so is an instrument-superset row.
        let instrument_without_cbug = r#"
schema = 1
[[deviation]]
id = "INSTR-1"
bucket = "INSTRUMENT-SUPERSET"
record_types = ["ai"]
why = "ground-truth instrument dbd superset, justified here rather than by a CBUG"
"#;
        assert!(Allowlist::parse(instrument_without_cbug).is_ok());
    }

    /// A row that names no record type, no field and no destination type
    /// matches the whole denominator, so it justifies every difference the run
    /// will ever find. That is the one failure the allowlist exists to prevent,
    /// and it is refused at load rather than measured around.
    #[test]
    fn a_row_bounded_by_nothing_is_rejected_at_parse() {
        let toml = r#"
schema = 1
[[deviation]]
id = "CBUG-Z1"
bucket = "NOT-REPRODUCED"
surface = ["value_string"]
classes = ["over-max"]
why = "surface and class say WHAT, never WHERE — this row is the whole surface"
"#;
        let err = Allowlist::parse(toml).unwrap_err();
        assert!(err.contains("unbounded"), "got: {err}");

        // Any one of the three anchors is enough.
        for anchor in [
            "record_types = [\"ai\"]",
            "fields = [\"VAL\"]",
            "dbf_types = [\"DBF_LONG\"]",
        ] {
            let bounded = toml.replace("surface =", &format!("{anchor}\nsurface ="));
            assert!(
                Allowlist::parse(&bounded).is_ok(),
                "{anchor} bounds the row: {:?}",
                Allowlist::parse(&bounded).err()
            );
        }
    }

    /// A `dbf_types` entry that is not a `.dbd` spelling would narrow the row to
    /// nothing while reading as a bound, so it is a load error, not a silent
    /// never-match.
    #[test]
    fn an_unknown_dbf_type_spelling_is_rejected() {
        let toml = r#"
schema = 1
[[deviation]]
id = "CBUG-Z2"
bucket = "NOT-REPRODUCED"
dbf_types = ["DBF_LNOG"]
why = "typo'd destination type must not read as a bound"
"#;
        let err = Allowlist::parse(toml).unwrap_err();
        assert!(err.contains("dbf_types"), "got: {err}");
    }

    /// The CA path and the PVA path now decide with ONE rule. A row carrying a
    /// content constraint used to be scope-and-surface only on the CA side, so
    /// it justified any difference on the channel; the constraint is honoured
    /// there too now.
    #[test]
    fn a_content_constrained_row_does_not_launder_an_unrelated_ca_difference() {
        let toml = r#"
schema = 1
[[deviation]]
id = "CBUG-Z3"
bucket = "NOT-REPRODUCED"
record_types = ["ai"]
surface = ["value_string"]
port_adds_leaves = ["display.precision"]
why = "the port serves a leaf the reference drops — and nothing else may move"
"#;
        let mut al = Allowlist::parse(toml).unwrap();
        assert!(
            al.match_diff(
                &ctx("ai", "VAL", DbfType::Double, None),
                &diff(Surface::ValueString, "0", "inf")
            )
            .is_none(),
            "the readings are a changed value, not an added leaf"
        );
    }

    /// An unknown bucket string is a typo, not a fifth policy — rejected.
    #[test]
    fn an_unknown_bucket_is_rejected() {
        let toml = r#"
schema = 1
[[deviation]]
id = "CBUG-Z9"
bucket = "NOT-REPRODUCEDD"
why = "typo'd bucket must not silently become a wildcard"
"#;
        let err = Allowlist::parse(toml).unwrap_err();
        assert!(err.contains("unknown bucket"), "got: {err}");
    }

    // --- INSTRUMENT-SUPERSET: the QSRV2 demo device-support residuals. ---

    /// A DTYP `value_marking` rendering with the given `value.choices` line and
    /// every other leaf held fixed — the real longin/waveform DTYP shape, so the
    /// tests exercise the sole-differing-leaf logic faithfully.
    fn dtyp_blob(choices_line: &str) -> String {
        [
            "value.index int32_t = 0",
            choices_line,
            "alarm.severity int32_t = 3",
            "alarm.status int32_t = 2",
            "alarm.message string = \"UDF\"",
            "timeStamp.secondsPastEpoch int64_t = 631152000",
            "timeStamp.nanoseconds int32_t = 0",
            "timeStamp.userTag int32_t = 0",
            "display.description string = \"\"",
        ]
        .join("\n")
    }

    // The post-asyn-fix residual state: the port carries every asyn menu choice,
    // so the SOLE extra the ground truth serves is the QSRV2 demo choice.
    const LONGIN_REF: &str = "value.choices string[] = {7}[\"Soft Channel\", \"Async Soft Channel\", \"General Time\", \"asynInt32\", \"asynUInt32Digital\", \"asynInt64\", \"QSRV2 Set UTag\"]";
    const LONGIN_PORT: &str = "value.choices string[] = {6}[\"Soft Channel\", \"Async Soft Channel\", \"General Time\", \"asynInt32\", \"asynUInt32Digital\", \"asynInt64\"]";
    const WAVEFORM_REF: &str = "value.choices string[] = {22}[\"Soft Channel\", \"asynOctetCmdResponse\", \"asynOctetWriteRead\", \"asynOctetRead\", \"asynOctetWrite\", \"asynOctetWriteBinary\", \"asynInt8ArrayIn\", \"asynInt8ArrayOut\", \"asynInt16ArrayIn\", \"asynInt16ArrayOut\", \"asynInt32ArrayIn\", \"asynInt32ArrayOut\", \"asynFloat32ArrayIn\", \"asynFloat32ArrayOut\", \"asynFloat64ArrayIn\", \"asynFloat64ArrayOut\", \"asynInt32TimeSeries\", \"asynFloat64TimeSeries\", \"asynInt64ArrayIn\", \"asynInt64ArrayOut\", \"asynInt64TimeSeries\", \"QSRV2 Demo\"]";
    const WAVEFORM_PORT: &str = "value.choices string[] = {21}[\"Soft Channel\", \"asynOctetCmdResponse\", \"asynOctetWriteRead\", \"asynOctetRead\", \"asynOctetWrite\", \"asynOctetWriteBinary\", \"asynInt8ArrayIn\", \"asynInt8ArrayOut\", \"asynInt16ArrayIn\", \"asynInt16ArrayOut\", \"asynInt32ArrayIn\", \"asynInt32ArrayOut\", \"asynFloat32ArrayIn\", \"asynFloat32ArrayOut\", \"asynFloat64ArrayIn\", \"asynFloat64ArrayOut\", \"asynInt32TimeSeries\", \"asynFloat64TimeSeries\", \"asynInt64ArrayIn\", \"asynInt64ArrayOut\", \"asynInt64TimeSeries\"]";

    fn longin_instr_allowlist() -> Allowlist {
        Allowlist::parse(
            "schema = 1\n\
             [[deviation]]\n\
             id = \"INSTR-QSRV2-LONGIN-UTAG\"\n\
             bucket = \"INSTRUMENT-SUPERSET\"\n\
             record_types = [\"longin\"]\n\
             fields = [\"DTYP\"]\n\
             surface = [\"value_marking\"]\n\
             ground_truth_adds_choices = [\"QSRV2 Set UTag\"]\n\
             why = \"softIocPVX.dbd superset links pvxs devLoPDBQ2UTag\"\n",
        )
        .expect("instrument-superset row parses without a CBUG id")
    }

    #[test]
    fn an_instrument_superset_row_matches_a_ground_truth_added_choice() {
        let mut al = longin_instr_allowlist();
        let hit = al.match_pva_diff(
            &ctx("longin", "DTYP", DbfType::Device, None),
            "value_marking",
            &dtyp_blob(LONGIN_REF),
            &dtyp_blob(LONGIN_PORT),
        );
        assert_eq!(hit.as_deref(), Some("INSTR-QSRV2-LONGIN-UTAG"));
        assert!(al.fired_rows().contains("INSTR-QSRV2-LONGIN-UTAG"));
        assert!(al.stale_rows().is_empty(), "a fired row is not stale");
    }

    /// Anti-launder, same discipline as `port_adds_leaves`: a second differing
    /// leaf on the same channel means the whole case stays a DEFECT.
    #[test]
    fn an_instrument_superset_row_does_not_launder_a_second_marking_difference() {
        let mut al = longin_instr_allowlist();
        let port = dtyp_blob(LONGIN_PORT)
            .replace("alarm.severity int32_t = 3", "alarm.severity int32_t = 4");
        assert!(
            al.match_pva_diff(
                &ctx("longin", "DTYP", DbfType::Device, None),
                "value_marking",
                &dtyp_blob(LONGIN_REF),
                &port,
            )
            .is_none(),
            "a second differing leaf must not ride in on the choice constraint"
        );
    }

    /// The ground truth's extra must be EXACTLY the named choice: an unnamed
    /// extra is unaccounted for and keeps the case a defect.
    #[test]
    fn an_instrument_superset_row_requires_the_extra_to_be_exactly_the_named_choices() {
        let mut al = longin_instr_allowlist();
        let ref_two = "value.choices string[] = {8}[\"Soft Channel\", \"Async Soft Channel\", \"General Time\", \"asynInt32\", \"asynUInt32Digital\", \"asynInt64\", \"QSRV2 Set UTag\", \"Surprise Choice\"]";
        assert!(
            al.match_pva_diff(
                &ctx("longin", "DTYP", DbfType::Device, None),
                "value_marking",
                &dtyp_blob(ref_two),
                &dtyp_blob(LONGIN_PORT),
            )
            .is_none(),
            "an unnamed extra choice must keep the case a defect"
        );
    }

    /// Direction matters: if the PORT carries a choice the ground truth lacks,
    /// that is not an instrument superset and must not fire.
    #[test]
    fn an_instrument_superset_row_does_not_match_when_the_port_adds_a_choice() {
        let mut al = longin_instr_allowlist();
        let port_extra = "value.choices string[] = {7}[\"Soft Channel\", \"Async Soft Channel\", \"General Time\", \"asynInt32\", \"asynUInt32Digital\", \"asynInt64\", \"Port Only\"]";
        assert!(
            al.match_pva_diff(
                &ctx("longin", "DTYP", DbfType::Device, None),
                "value_marking",
                &dtyp_blob(LONGIN_REF),
                &dtyp_blob(port_extra),
            )
            .is_none(),
            "the port carrying an extra choice is the wrong direction"
        );
    }

    /// The two content constraints point in opposite directions; a row may set
    /// at most one.
    #[test]
    fn a_row_may_not_set_both_content_constraints() {
        let toml = r#"
schema = 1
[[deviation]]
id = "INSTR-BAD"
bucket = "INSTRUMENT-SUPERSET"
surface = ["value_marking"]
port_adds_leaves = ["display.precision"]
ground_truth_adds_choices = ["QSRV2 Demo"]
why = "cannot point both directions at once"
"#;
        let err = Allowlist::parse(toml).unwrap_err();
        assert!(err.contains("at most one content constraint"), "got: {err}");
    }

    /// The two SHIPPED instrument-superset rows fire on the residual QSRV2 diffs
    /// for both longin and waveform — the end-state the main worker's combined
    /// run reaches once the asyn menu fix is integrated.
    #[test]
    fn the_shipped_instrument_superset_rows_fire_on_the_residual_qsrv2_diffs() {
        let mut al = Allowlist::load(&Allowlist::default_path()).unwrap();
        let longin = al.match_pva_diff(
            &ctx("longin", "DTYP", DbfType::Device, None),
            "value_marking",
            &dtyp_blob(LONGIN_REF),
            &dtyp_blob(LONGIN_PORT),
        );
        assert_eq!(longin.as_deref(), Some("INSTR-QSRV2-LONGIN-UTAG"));
        let waveform = al.match_pva_diff(
            &ctx("waveform", "DTYP", DbfType::Device, None),
            "value_marking",
            &dtyp_blob(WAVEFORM_REF),
            &dtyp_blob(WAVEFORM_PORT),
        );
        assert_eq!(waveform.as_deref(), Some("INSTR-QSRV2-WAVEFORM-DEMO"));
    }
}
