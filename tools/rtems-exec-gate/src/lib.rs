//! Mechanical enforcement of the `rtems-exec-model` test-gating rule.
//!
//! Under `--features rtems-exec-model` the `runtime::task` seam routes `spawn`
//! to the std-thread background executor, which has no tokio reactor. A test
//! that needs an ambient reactor therefore cannot run in that configuration —
//! not because it is wrong, but because the feature deliberately does not start
//! one. Those tests are gated out; the rest stay in and are expected to pass.
//!
//! That classification used to live only in reviewers' heads, so a newly added
//! async test would silently redden the feature-ON suite and the next person
//! had to rediscover why. This crate makes the classification a checked
//! property of the source tree: read a crate's own sources, and fail naming the
//! rule.
//!
//! # Why this is a crate and not a test file
//!
//! It began as one test file in `epics-ca-rs`. Every crate that declares
//! `rtems-exec-model` has the same hole — `epics-pva-rs` had it, and the
//! feature-ON suite there reddened exactly as predicted — and the answer to
//! "the same rule in three crates" is not three copies of a 300-line audit.
//! Each crate now keeps a ten-line `tests/rtems_exec_model_gate.rs` that calls
//! [`assert_crate_is_accounted_for`]; the rule, its message and its own
//! boundary tests live here, once.
//!
//! Dev-only and `publish = false`: it reads source text and is never linked
//! into a shipped artefact.
//!
//! # The rule
//!
//! Every *reactor-dependent test site* in the scanned crate must be accounted
//! for by exactly one of four things:
//!
//! 1. a file-level `#![cfg(not(feature = "rtems-exec-model"))]`;
//! 2. an enclosing column-0 `mod` carrying `#[cfg(..., not(feature =
//!    "rtems-exec-model"))]` — the `server_connection_drop_tests` precedent;
//! 3. a per-test `#[cfg(not(feature = "rtems-exec-model"))]` directly above it
//!    — the `protocol_tests.rs` precedent, which keeps that file's pure
//!    wire-format tests running feature-ON;
//!
//! Either gate may also be spelled `cfg(tokio_backend)`, which selects the
//! same host builds and additionally excludes the target — see
//! [`BACKEND_GATE_PREDICATE`] for why a test whose subject is UDP has to be
//! spelled that way;
//! 4. a file-level census marker declaring how many ungated sites the file
//!    has and why they may stay ungated — either they were checked to pass
//!    feature-ON, or the file does not build or run in that configuration at
//!    all. Written as the first thing in a comment:
//!    `// RTEMS-EXEC-MODEL-ALLOW(N): why`.
//!
//! A "reactor-dependent test site" is a `#[tokio::test]` attribute, or a line
//! in test code that builds a tokio runtime by hand. Both spellings obtain the
//! ambient reactor that the feature does not provide, so both are counted.
//!
//! # Why this fails closed
//!
//! Option 4 is a census, not a blanket waiver: the declared `N` must equal the
//! number of ungated sites found. Adding an async test to an already-declared
//! file changes the count and fails the guard, which is the case a plain
//! allowlist would have let through. Adding a *new* file with async tests and
//! no declaration at all fails for want of any accounting. Either way the
//! author is told the rule and must state which of the four applies.
//!
//! Bumping `N` is a deliberate, reviewable act, and it is not self-certifying:
//! the caller runs *inside* the feature-ON suite, so a site vouched for by a
//! bumped count still has to actually pass there.
//!
//! Note for editors of this file: the census marker is recognised only at the
//! start of a comment body, so prose may name it freely as long as no comment
//! line *begins* with it. Fixture sources below assemble the attribute
//! spellings with `concat!` for the same reason the entry-point guards do —
//! otherwise this crate's own body would match itself when it is scanned.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// The gate spelled as it appears in source, at file, module and test scope.
pub const GATE_ATTR: &str = r#"#[cfg(not(feature = "rtems-exec-model"))]"#;
pub const FILE_GATE_ATTR: &str = r#"#![cfg(not(feature = "rtems-exec-model"))]"#;
pub const GATE_PREDICATE: &str = r#"not(feature = "rtems-exec-model")"#;
/// The second spelling of the same gate, and the one a *source* item uses.
///
/// `tokio_backend` is emitted by the crate's `build.rs` as the negation of
/// `exec_backend` — `target_os = "rtems"` or the `rtems-exec-model` feature —
/// so `cfg(tokio_backend)` and `cfg(not(feature = "rtems-exec-model"))` select
/// the same set of host builds. They are not interchangeable, though: only
/// `tokio_backend` also excludes the target, which is why the items a test
/// like this reaches for (a `SearchTransport::Udp`, an `AsyncUdpV4`) carry it.
/// A test that names such an item has to carry the same predicate its subject
/// does, or it fails to compile — so this guard has to count it as a gate,
/// otherwise the census would keep vouching feature-ON for tests that do not
/// run there.
///
/// `not(tokio_backend)` is deliberately *not* a gate: it selects the exec
/// backend, which is the configuration the census is about.
pub const BACKEND_GATE_PREDICATE: &str = "tokio_backend";
pub const CENSUS_MARKER: &str = "RTEMS-EXEC-MODEL-ALLOW(";

/// Printed with every violation so the failure explains itself without the
/// reader having to find this file first.
pub const RULE: &str = "\
rtems-exec-model gating rule: every #[tokio::test] and every hand-built tokio
runtime in test code must be accounted for by exactly one of
  (1) file-level #![cfg(not(feature = \"rtems-exec-model\"))]
  (2) an enclosing column-0 mod gated with not(feature = \"rtems-exec-model\")
      or with tokio_backend
  (3) a per-test #[cfg(not(feature = \"rtems-exec-model\"))] — or #[cfg(..
      tokio_backend ..)] — directly above it
  (4) a file-level census marker `// RTEMS-EXEC-MODEL-ALLOW(N): why`, where N
      is exactly the number of ungated sites in the file
Pick (1)-(3) if the test needs a tokio reactor the feature does not start. Pick
(4) only after checking the test actually passes under --features
rtems-exec-model, or that the file does not build or run there at all — and say
which in the reason.";

// ---------------------------------------------------------------------------
// The audit
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct Audit {
    /// 1-based lines of reactor-dependent sites that no gate accounts for.
    pub ungated: Vec<usize>,
    /// `N` from the census marker, if the file declares one.
    pub declared: Option<usize>,
    /// Human-readable rule breaches; empty means the file is compliant.
    pub violations: Vec<String>,
}

/// Column-0 module region state, tracked so a gated `mod` covers its contents.
///
/// This leans on rustfmt-normalised layout — a top-level `mod x {` opens at
/// column 0 and its `}` closes at column 0 — which is sound here because
/// `cargo fmt --all` is a mandatory gate on this repo.
#[derive(Clone, Copy, Default)]
struct Region {
    is_test: bool,
    is_gated: bool,
}

pub fn audit_source(path: &str, text: &str, integration: bool) -> Audit {
    let lines: Vec<&str> = text.lines().collect();
    let file_gated = lines.iter().any(|l| l.trim() == FILE_GATE_ATTR);
    let markers: Vec<usize> = lines.iter().filter_map(|l| parse_census(l)).collect();
    let declared = markers.first().copied();

    // An integration test binary is itself test code; its tests sit at column 0
    // rather than inside a `#[cfg(test)] mod`.
    let base = Region {
        is_test: integration,
        is_gated: false,
    };
    let mut region = base;
    let mut pending: Vec<&str> = Vec::new();
    let mut ungated = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let col0 = !raw.is_empty() && raw.len() == trimmed.len();

        // -- column-0 module bookkeeping ------------------------------------
        if col0 {
            if raw.starts_with("#[") {
                pending.push(trimmed);
            } else if is_mod_open(raw) {
                region = Region {
                    is_test: base.is_test || pending.iter().any(|a| a.contains("cfg(test)")),
                    is_gated: pending.iter().any(|a| is_gate_attr(a)),
                };
                pending.clear();
            } else if raw.starts_with('}') {
                region = base;
                pending.clear();
            } else if !raw.starts_with("//") {
                pending.clear();
            }
        }

        // -- reactor-dependent sites ----------------------------------------
        let is_tokio_test = trimmed.starts_with(concat!("#[tokio", "::test"));
        let builds_runtime = region.is_test && builds_a_runtime(trimmed);
        if !is_tokio_test && !builds_runtime {
            continue;
        }
        if file_gated || region.is_gated {
            continue;
        }
        if is_tokio_test && per_test_gated(&lines, idx) {
            continue;
        }
        ungated.push(idx + 1);
    }

    let mut violations = Vec::new();
    if markers.len() > 1 {
        violations.push(format!(
            "{path}: {} census markers; a file must declare exactly one, or the \
             count that is checked is whichever happens to come first.",
            markers.len()
        ));
    }
    match declared {
        None if !ungated.is_empty() => {
            let mut msg = String::new();
            let _ = write!(
                msg,
                "{path}: {} reactor-dependent test site(s) at line(s) {:?} are not \
                 accounted for, and the file declares no census marker.",
                ungated.len(),
                ungated
            );
            violations.push(msg);
        }
        Some(n) if n != ungated.len() => {
            let mut msg = String::new();
            let _ = write!(
                msg,
                "{path}: census marker declares {n} ungated reactor-dependent test \
                 site(s), but {} were found at line(s) {:?}. If you added one, run it \
                 under --features rtems-exec-model and only then bump the count.",
                ungated.len(),
                ungated
            );
            violations.push(msg);
        }
        _ => {}
    }

    Audit {
        ungated,
        declared,
        violations,
    }
}

/// `mod foo {` / `pub mod foo {` at column 0 — an inline module opening.
fn is_mod_open(raw: &str) -> bool {
    let rest = raw.strip_prefix("pub ").unwrap_or(raw);
    rest.starts_with("mod ") && rest.trim_end().ends_with('{')
}

/// A hand-built tokio runtime: the other way a test obtains a reactor.
fn builds_a_runtime(trimmed: &str) -> bool {
    trimmed.contains(concat!("tokio::runtime::Runtime", "::new"))
        || trimmed.contains(concat!("tokio::runtime::Builder", "::new_"))
}

/// Is the attribute block directly above `idx` carrying the per-test gate?
///
/// Walks up over attributes, comments and blank lines only, so a preceding
/// test's gate cannot be mistaken for this one's — any code line (a closing
/// brace, in practice) stops the walk.
fn per_test_gated(lines: &[&str], idx: usize) -> bool {
    for above in lines[..idx].iter().rev() {
        let t = above.trim();
        if is_gate_attr(t) {
            return true;
        }
        if t.is_empty() || t.starts_with("#[") || t.starts_with("//") {
            continue;
        }
        return false;
    }
    false
}

/// Does this attribute select *away* from the exec backend?
///
/// Either spelling counts — see [`BACKEND_GATE_PREDICATE`]. `tokio_backend`
/// under a `not(...)` does the opposite and is rejected, so a test gated to
/// run **only** on the exec backend stays in the census where it belongs.
fn is_gate_attr(attr: &str) -> bool {
    if !attr.starts_with("#[") || !attr.contains("cfg(") {
        return false;
    }
    if attr.contains(GATE_PREDICATE) {
        return true;
    }
    attr.contains(BACKEND_GATE_PREDICATE) && !attr.contains(concat!("not(", "tokio_backend", ")"))
}

/// `// RTEMS-EXEC-MODEL-ALLOW(N): why` — recognised only at the start of a
/// comment body, so prose elsewhere can name the marker without declaring one.
///
/// A plain `//` is accepted as well as a `//!` doc comment: in `src/` the
/// marker is a directive to this guard, not documentation, and should not land
/// in the rendered rustdoc for the module.
fn parse_census(line: &str) -> Option<usize> {
    let body = line.trim().strip_prefix("//")?;
    let body = body.strip_prefix('!').unwrap_or(body).trim_start();
    let digits = body.strip_prefix(CENSUS_MARKER)?;
    let end = digits.find(')')?;
    digits[..end].trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Live scan over this crate
// ---------------------------------------------------------------------------

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Scan one crate and panic, naming the rule, if anything is unaccounted for.
///
/// `manifest_dir` is the caller's `env!("CARGO_MANIFEST_DIR")`; `min_files` is
/// a floor on how many `.rs` files the scan must find, which is what turns a
/// silently-empty scan — a moved directory, a bad path — into a failure rather
/// than a pass. A guard that scans nothing reports green forever.
///
/// Call it from a `tests/rtems_exec_model_gate.rs` that is itself gated to
/// `#![cfg(feature = "rtems-exec-model")]`, so the census is checked in exactly
/// the configuration it describes.
pub fn assert_crate_is_accounted_for(manifest_dir: &str, min_files: usize) {
    let root = Path::new(manifest_dir);
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("tests"), &mut files);
    files.sort();
    assert!(
        files.len() >= min_files,
        "the audit found only {} source files under {}; it is not scanning the crate",
        files.len(),
        root.display()
    );

    let mut violations = Vec::new();
    for path in &files {
        let integration = path.starts_with(root.join("tests"));
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let text = fs::read_to_string(path).expect("read crate source");
        violations.extend(audit_source(&rel, &text, integration).violations);
    }

    assert!(
        violations.is_empty(),
        "{RULE}\n\n{} file(s) breach it:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The guard's own tests — one per accounting boundary, so each of the four
// options is shown to satisfy the rule and each way of breaching it is shown
// to fire. Sources are assembled with `concat!` so this file cannot match
// itself during the live scan above.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod fixtures {
    use super::*;

    fn tokio_test() -> &'static str {
        concat!("#[tokio", "::test]")
    }

    fn runtime_build() -> &'static str {
        concat!(
            "    let rt = tokio::runtime::Builder",
            "::new_current_thread().build();"
        )
    }

    /// An async test with no accounting at all — the base failure this guard
    /// exists to catch.
    #[test]
    fn an_ungated_async_test_is_a_violation() {
        let src = format!("{}\nasync fn t() {{}}\n", tokio_test());
        let audit = audit_source("new_test.rs", &src, true);
        assert_eq!(audit.ungated, vec![1]);
        assert_eq!(audit.violations.len(), 1, "{:?}", audit.violations);
        assert!(
            audit.violations[0].contains("no census marker"),
            "the message must say what is missing: {}",
            audit.violations[0]
        );
    }

    /// Option 1: file-level gate.
    #[test]
    fn a_file_level_gate_accounts_for_every_test_in_the_file() {
        let src = format!(
            "{FILE_GATE_ATTR}\n{a}\nasync fn one() {{}}\n{a}\nasync fn two() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("gated.rs", &src, true);
        assert!(audit.ungated.is_empty());
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// Option 2: enclosing gated column-0 module — the
    /// `server_connection_drop_tests` shape.
    #[test]
    fn a_gated_column_zero_module_accounts_for_its_tests() {
        let src = format!(
            "#[cfg(all(test, {GATE_PREDICATE}))]\nmod tests {{\n    {a}\n    async fn t() {{}}\n}}\n",
            a = tokio_test()
        );
        let audit = audit_source("src/x.rs", &src, false);
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// ...and the module's gate must stop at its closing brace, or the guard
    /// would silently cover everything that follows it.
    #[test]
    fn a_gated_module_does_not_cover_tests_after_its_closing_brace() {
        let src = format!(
            "#[cfg(all(test, {GATE_PREDICATE}))]\nmod gated {{\n    {a}\n    async fn inside() {{}}\n}}\n\
             #[cfg(test)]\nmod open {{\n    {a}\n    async fn outside() {{}}\n}}\n",
            a = tokio_test()
        );
        let audit = audit_source("src/x.rs", &src, false);
        assert_eq!(audit.ungated.len(), 1, "only the second test is exposed");
        assert_eq!(audit.violations.len(), 1);
    }

    /// The backend spelling of option 2, which is what a test whose subject
    /// only exists on `tokio_backend` has to carry.
    #[test]
    fn a_backend_gated_column_zero_module_accounts_for_its_tests() {
        let src = format!(
            "#[cfg(all(test, {BACKEND_GATE_PREDICATE}))]\nmod tests {{\n    {a}\n    async fn t() {{}}\n}}\n",
            a = tokio_test()
        );
        let audit = audit_source("src/x.rs", &src, false);
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// ...and the backend spelling of option 3.
    #[test]
    fn a_backend_gated_per_test_attribute_accounts_for_just_that_test() {
        let src = format!(
            "// why\n#[cfg({BACKEND_GATE_PREDICATE})]\n{a}\nasync fn gated() {{}}\n\n{a}\nasync fn open() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("t.rs", &src, true);
        assert_eq!(audit.ungated.len(), 1, "only the ungated test is counted");
    }

    /// The inverse selects the exec backend, so it is not a gate: a test
    /// carrying it *does* run feature-ON and stays in the census.
    #[test]
    fn the_exec_backend_predicate_is_not_a_gate() {
        let src = format!(
            "#[cfg(not({BACKEND_GATE_PREDICATE}))]\n{a}\nasync fn t() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("t.rs", &src, true);
        assert_eq!(audit.ungated.len(), 1, "{:?}", audit);
    }

    /// Option 3: per-test gate — the `protocol_tests.rs` shape, which is what
    /// keeps that file's pure wire-format tests running feature-ON.
    #[test]
    fn a_per_test_gate_accounts_for_just_that_test() {
        let src = format!(
            "// why\n{GATE_ATTR}\n{a}\nasync fn gated() {{}}\n\n{a}\nasync fn open() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("mixed.rs", &src, true);
        assert_eq!(
            audit.ungated,
            vec![6],
            "the gate must apply to the test directly below it and no other"
        );
    }

    /// Option 4: the census, at its two boundaries — exact match passes.
    #[test]
    fn a_census_marker_matching_the_count_is_compliant() {
        let src = format!(
            "//! {CENSUS_MARKER}1): proven to pass feature-ON\n{}\nasync fn t() {{}}\n",
            tokio_test()
        );
        let audit = audit_source("neutral.rs", &src, true);
        assert_eq!(audit.declared, Some(1));
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// ...and the case a plain allowlist would have let through: a new async
    /// test added to a file that already carries a marker.
    #[test]
    fn adding_a_test_to_a_declared_file_breaks_its_census() {
        let src = format!(
            "//! {CENSUS_MARKER}1): proven to pass feature-ON\n{a}\nasync fn old() {{}}\n\n{a}\nasync fn added() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("neutral.rs", &src, true);
        assert_eq!(audit.ungated.len(), 2);
        assert_eq!(audit.violations.len(), 1, "{:?}", audit.violations);
        assert!(
            audit.violations[0].contains("declares 1"),
            "the message must contrast declared with found: {}",
            audit.violations[0]
        );
    }

    /// A marker left behind after its tests were gated is equally a breach —
    /// the census is exact in both directions.
    #[test]
    fn a_stale_census_marker_is_a_violation() {
        let src = format!("//! {CENSUS_MARKER}2): stale\n{FILE_GATE_ATTR}\n");
        let audit = audit_source("stale.rs", &src, true);
        assert_eq!(audit.violations.len(), 1, "{:?}", audit.violations);
    }

    /// The second spelling: a hand-built runtime inside test code counts too,
    /// so the rule cannot be sidestepped by dropping the attribute.
    #[test]
    fn a_hand_built_runtime_in_test_code_is_a_site() {
        let src = format!(
            "#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{\n{}\n    }}\n}}\n",
            runtime_build()
        );
        let audit = audit_source("src/x.rs", &src, false);
        assert_eq!(audit.ungated.len(), 1, "{:?}", audit.ungated);
    }

    /// ...but the same call in production code is not a test site, so the
    /// guard does not fire on binaries that legitimately start a runtime.
    #[test]
    fn a_hand_built_runtime_outside_test_code_is_not_a_site() {
        let src = format!("fn main() {{\n{}\n}}\n", runtime_build());
        let audit = audit_source("src/bin/y.rs", &src, false);
        assert!(audit.ungated.is_empty(), "{:?}", audit.ungated);
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// The marker is only a declaration when it opens the comment body; prose
    /// that merely names it — as this guard's own module doc does — must not
    /// count, or the guard would declare a census on itself.
    #[test]
    fn the_marker_is_only_recognised_at_the_start_of_a_comment() {
        assert_eq!(parse_census(&format!("// {CENSUS_MARKER}3): why")), Some(3));
        assert_eq!(
            parse_census(&format!("//! {CENSUS_MARKER}3): why")),
            Some(3)
        );
        assert_eq!(
            parse_census(&format!("    // {CENSUS_MARKER}3): why")),
            Some(3)
        );
        assert_eq!(
            parse_census(&format!("//! see {CENSUS_MARKER}N) for the rule")),
            None
        );
        assert_eq!(
            parse_census(&format!("let s = \"{CENSUS_MARKER}3)\";")),
            None
        );
    }

    /// Two markers would make the checked count depend on line order, so the
    /// file must declare exactly one.
    #[test]
    fn two_census_markers_in_one_file_is_a_violation() {
        let src = format!(
            "// {CENSUS_MARKER}1): first\n// {CENSUS_MARKER}9): second\n{}\nasync fn t() {{}}\n",
            tokio_test()
        );
        let audit = audit_source("double.rs", &src, true);
        assert_eq!(audit.violations.len(), 1, "{:?}", audit.violations);
        assert!(
            audit.violations[0].contains("2 census markers"),
            "{}",
            audit.violations[0]
        );
    }
}
