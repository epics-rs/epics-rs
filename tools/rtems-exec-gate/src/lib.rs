//! Mechanical enforcement of the exec-backend test-gating rule.
//!
//! On the exec backend the `runtime::task` seam routes `spawn` to the
//! std-thread background executor, which has no tokio reactor. A test that
//! needs an ambient reactor therefore cannot run in that configuration — not
//! because it is wrong, but because the backend deliberately does not start
//! one. Those tests are gated out; the rest stay in and are expected to pass.
//!
//! The backend is selected by `EPICS_RS_BUILD_EXEC_BACKEND` (see
//! [`EXEC_BACKEND_ENV`]) together with the target triple, and it used to be
//! selected by a cargo feature as well. That is why this census reads only
//! `exec_backend`/`tokio_backend` now: one axis, one name, and no way for a
//! gate to name the target where it meant the backend.
//!
//! That classification used to live only in reviewers' heads, so a newly added
//! async test would silently redden the exec-backend suite and the next person
//! had to rediscover why. This crate makes the classification a checked
//! property of the source tree: read a crate's own sources, and fail naming
//! the rule.
//!
//! # Why this is a crate and not a test file
//!
//! It began as one test file in `epics-ca-rs`. Every crate that derives the
//! backend cfg has the same hole — `epics-pva-rs` had it, and the exec-backend
//! suite there reddened exactly as predicted — and the answer to "the same
//! rule in three crates" is not three copies of a 300-line audit. Each crate
//! now keeps a ten-line `tests/rtems_exec_model_gate.rs` that calls
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
//! 1. a file-level `#![cfg(..)]` gate;
//! 2. an enclosing column-0 `mod` carrying a `#[cfg(..)]` gate — the
//!    `server_connection_drop_tests` precedent;
//! 3. a per-test `#[cfg(..)]` gate directly above it — the
//!    `protocol_tests.rs` precedent, which keeps that file's pure wire-format
//!    tests running on the exec backend;
//!
//! A *gate*, at any of those three scopes, is any `cfg` predicate that is
//! false on the exec backend — however it is spelled and however rustfmt
//! wrapped it. `tokio_backend` and `not(any(exec_backend, ..))` are the two
//! the tree uses, and both are read by evaluating the predicate rather than by
//! matching a literal, so a third spelling needs no edit here. What is *not*
//! a gate falls out of the same evaluation: `not(tokio_backend)` selects the
//! exec backend, and `any(not(feature = ".."), feature = "client")` still
//! holds on the exec backend, so tests carrying either stay in the census;
//! 4. a census marker declaring how many ungated sites follow it and why
//!    they may stay ungated — either they were checked to pass on the exec
//!    backend, or the file does not build or run in that configuration at
//!    all. Written as
//!    the first thing in a comment: `// RTEMS-EXEC-MODEL-ALLOW(N): why`.
//!    A marker vouches for the sites between it and the NEXT marker, so a
//!    file may carry one marker at the top counting everything below it, or
//!    one marker beside each site counting just that site.
//!
//! A "reactor-dependent test site" is a `#[tokio::test]` attribute, or a line
//! in test code that builds a tokio runtime by hand. Both spellings obtain the
//! ambient reactor that the exec backend does not provide, so both are
//! counted.
//!
//! # Why this fails closed
//!
//! Option 4 is a census, not a blanket waiver: each marker's `N` must equal
//! the number of ungated sites between it and the next marker. Adding an async
//! test to an already-declared file changes some marker's count and fails the
//! guard, which is the case a plain allowlist would have let through. Adding a
//! *new* file with async tests and no declaration at all fails for want of any
//! accounting. Either way the author is told the rule and must state which of
//! the four applies.
//!
//! Which marker fails is the reason the count is regional rather than a single
//! per-file total. A file-wide total is invalidated by an edit anywhere in the
//! file — `epics-base-rs`'s `h6_gate_released_across_async.rs` carried a stale
//! `4` from the commit that added its fifth test — and the number that has
//! gone wrong is then nowhere near the line that broke it. A marker that
//! counts only what follows it can be put beside the site it vouches for,
//! where the next author is already editing.
//!
//! Bumping `N` is a deliberate, reviewable act, and it is not self-certifying:
//! the caller runs *inside* the exec-backend suite, so a site vouched for by a
//! bumped count still has to actually pass there.
//!
//! Note for editors of this file: the census marker is recognised only at the
//! start of a comment body, so prose may name it freely as long as no comment
//! line *begins* with it. Fixture sources below assemble the attribute
//! spellings with `concat!` for the same reason the entry-point guards do —
//! otherwise this crate's own body would match itself when it is scanned.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The gate spelled as it appears in source, at file, module and test scope.
///
/// One spelling, because there is one name. While the backend was also a cargo
/// feature this read `not(feature = "rtems-exec-model")`, and `tokio_backend`
/// was a *second* spelling of the same decision — two names for one axis, and
/// only one of them also excluded the embedded targets. The feature is gone,
/// the backend is chosen by `EPICS_RS_BUILD_EXEC_BACKEND`, and what a source
/// item carries is what a test naming that item must carry: this.
pub const GATE_ATTR: &str = "#[cfg(tokio_backend)]";
pub const FILE_GATE_ATTR: &str = "#![cfg(tokio_backend)]";
pub const GATE_PREDICATE: &str = "tokio_backend";
/// The positive twin of [`GATE_PREDICATE`], and the other way the tree spells
/// the same decision.
///
/// `build.rs` emits exactly one of `exec_backend` / `tokio_backend`, so
/// `not(any(exec_backend, ..))` — the shape `client/transport.rs` and
/// `client_native/server_conn.rs` use — is as much a gate as `tokio_backend`
/// is. Recognising it is not a courtesy: a gate the guard cannot read is a
/// site it counts as ungated, and the fix for that false red is to spell the
/// gate the guard likes, which is how a census ends up describing its
/// instrument instead of the tree.
///
/// `not(tokio_backend)` is deliberately *not* a gate: it selects the exec
/// backend, which is the configuration the census is about.
pub const EXEC_BACKEND_PREDICATE: &str = "exec_backend";
pub const CENSUS_MARKER: &str = "RTEMS-EXEC-MODEL-ALLOW(";

/// Printed with every violation so the failure explains itself without the
/// reader having to find this file first.
pub const RULE: &str = "\
exec-backend gating rule: every #[tokio::test] and every hand-built tokio
runtime in test code must be accounted for by exactly one of
  (1) a file-level #![cfg(..)] gate
  (2) an enclosing column-0 mod carrying a #[cfg(..)] gate
  (3) a per-test #[cfg(..)] gate directly above it
where a gate is any cfg predicate that is false on the exec backend:
tokio_backend and not(any(exec_backend, ..)) are the spellings in use, and the
guard evaluates the predicate rather than matching either of them
  (4) a census marker `// RTEMS-EXEC-MODEL-ALLOW(N): why`, where N is exactly
      the number of ungated sites between it and the next marker — one marker
      per file, or one beside each site
Pick (1)-(3) if the test needs a tokio reactor the backend does not start. Pick
(4) only after checking the test actually passes under
EPICS_RS_BUILD_EXEC_BACKEND=thread, or that the file does not build or run
there at all — and say which in the reason.";

// ---------------------------------------------------------------------------
// The audit
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct Audit {
    /// 1-based lines of reactor-dependent sites that no gate accounts for.
    pub ungated: Vec<usize>,
    /// 1-based lines of reactor-dependent sites a file-level `#![cfg(..)]` or
    /// an enclosing gated `mod` removed from the census.
    ///
    /// Options (1) and (2) are the only two accountings that take a site out
    /// of [`Audit::ungated`] without leaving anything at the site to see. A
    /// per-test gate — option (3) — is a line directly above the test, so it
    /// reads as an accounting in the diff; a whole-file or whole-module gate
    /// is one line far away that silently empties everything below it. That
    /// asymmetry is the reason this vector exists and per-test gates are not
    /// in it: a caller can pin this number and make the emptying loud.
    pub scope_gated: Vec<usize>,
    /// 1-based lines of reactor-FREE tests — `#[test]`, `#[epics_test]` — that
    /// the same file-level `#![cfg(..)]` or gated `mod` removed along with the
    /// sites above. Report-only: nothing asserts it.
    ///
    /// [`Audit::scope_gated`] says a gate fired; this says what else went with
    /// it, and without the pair a gated file is unreadable. A file with two
    /// reactor-dependent sites and ten reactor-free tests under one gate is a
    /// candidate to split — the ten cost nothing to keep and are checking
    /// something in exactly the configuration the gate removes them from. A
    /// file with three sites and no collateral is correctly gated and needs no
    /// further thought. Both report the same `scope_gated` length, so the
    /// census could not tell them apart until this existed.
    ///
    /// Reactor-free is decided by what the site predicate already owns: a test
    /// whose body builds a runtime is a site, not collateral, and is attributed
    /// to the nearest test attribute above it.
    pub collateral: Vec<usize>,
    /// The file's declared total: the sum of every census marker's `N`,
    /// `None` when the file declares none. The check itself is per marker —
    /// this is the whole-file figure a caller would want to report.
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
    audit_inner(path, text, integration, false)
}

/// What a gate cost by removing this whole file, for a file no gate inside it
/// mentions.
///
/// A parent's `#[cfg(<gate>)] mod x;` puts a file under a gate exactly as a
/// `#![cfg(..)]` written inside it would, and leaves nothing in the file to
/// read — so the shape is invisible to any reading of the file alone, and the
/// tests it removes were knowable only by resolving the declaration. This is
/// [`audit_source`] told from outside what the file cannot say about itself,
/// projected to the same [`GateCost`] the in-file shape reports, so the two
/// total together.
///
/// Cost only, no accounting: the file is still scanned normally by
/// [`audit_crate`], which is where its census markers are checked. Judging it
/// twice would turn every marker in a removed file into a stale one.
pub fn removal_cost(path: &str, text: &str, integration: bool) -> GateCost {
    let audit = audit_inner(path, text, integration, true);
    GateCost {
        file: path.to_owned(),
        sites: audit.scope_gated.len(),
        collateral: audit.collateral.len(),
    }
}

fn audit_inner(path: &str, text: &str, integration: bool, removed_by_parent: bool) -> Audit {
    let lines: Vec<&str> = text.lines().collect();
    let attrs = Attrs::collect(&lines);
    let file_gated = removed_by_parent
        || lines.iter().enumerate().any(|(i, l)| {
            l.trim_start().starts_with("#![")
                && attrs.opened[i].as_deref().is_some_and(is_gate_attr)
        });
    // (1-based line, declared count) so a violation can name the marker that
    // is wrong rather than the file that contains it.
    let markers: Vec<(usize, usize)> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, l)| parse_census(l).map(|n| (idx + 1, n)))
        .collect();
    let declared = if markers.is_empty() {
        None
    } else {
        Some(markers.iter().map(|(_, n)| n).sum())
    };

    // An integration test binary is itself test code; its tests sit at column 0
    // rather than inside a `#[cfg(test)] mod`.
    let base = Region {
        is_test: integration,
        is_gated: false,
    };
    let mut region = base;
    let mut pending: Vec<&str> = Vec::new();
    let mut ungated = Vec::new();
    let mut scope_gated = Vec::new();
    // Gated reactor-free test attributes, each with whether a runtime-building
    // line below it has claimed it — a claimed entry is a site, not collateral.
    let mut gated_tests: Vec<(usize, bool)> = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let col0 = !raw.is_empty() && raw.len() == trimmed.len();

        // -- column-0 module bookkeeping ------------------------------------
        if col0 && !attrs.continuation[idx] {
            match attrs.opened[idx].as_deref() {
                // An inner attribute applies to the file, not to the item
                // below it, so it falls through to the reset arm exactly as an
                // ordinary line would.
                Some(attr) if !trimmed.starts_with("#![") => pending.push(attr),
                _ if is_mod_open(raw) => {
                    region = Region {
                        is_test: base.is_test || pending.iter().any(|a| requires_test(a)),
                        is_gated: pending.iter().any(|a| is_gate_attr(a)),
                    };
                    pending.clear();
                }
                _ if raw.starts_with('}') => {
                    region = base;
                    pending.clear();
                }
                _ if raw.starts_with("//") => {}
                _ => pending.clear(),
            }
        }

        // -- reactor-free tests a scope gate would take with it -------------
        if (file_gated || region.is_gated) && is_reactor_free_test(trimmed) {
            gated_tests.push((idx + 1, false));
        }

        // -- reactor-dependent sites ----------------------------------------
        let is_tokio_test = trimmed.starts_with(concat!("#[tokio", "::test"));
        let builds_runtime = region.is_test && builds_a_runtime(trimmed);
        if !is_tokio_test && !builds_runtime {
            continue;
        }
        if file_gated || region.is_gated {
            scope_gated.push(idx + 1);
            if is_tokio_test {
                // A tokio test is a site in its own right and never collateral.
                // It enters the list already claimed so that a runtime built
                // inside its body is attributed here rather than to the
                // reactor-free test above it.
                gated_tests.push((idx + 1, true));
            } else if let Some(last) = gated_tests.last_mut() {
                last.1 = true;
            }
            continue;
        }
        if is_tokio_test && per_test_gated(&lines, &attrs, idx) {
            continue;
        }
        ungated.push(idx + 1);
    }

    let mut violations = Vec::new();
    // A removed file's markers are checked where the file is scanned normally;
    // re-judging it here would read every one of them as stale, because being
    // removed is exactly what empties `ungated`.
    if removed_by_parent {
        return Audit {
            ungated,
            scope_gated,
            collateral: unclaimed(gated_tests),
            declared,
            violations,
        };
    }
    // Sites above the first marker are vouched for by nothing: a marker only
    // ever speaks for what follows it.
    let first_marker = markers.first().map(|(line, _)| *line);
    let orphans: Vec<usize> = match first_marker {
        Some(first) => ungated.iter().copied().filter(|s| *s < first).collect(),
        None => ungated.clone(),
    };
    if !orphans.is_empty() {
        let mut msg = String::new();
        match first_marker {
            None => {
                let _ = write!(
                    msg,
                    "{path}: {} reactor-dependent test site(s) at line(s) {:?} are not \
                     accounted for, and the file declares no census marker.",
                    orphans.len(),
                    orphans
                );
            }
            Some(first) => {
                let _ = write!(
                    msg,
                    "{path}: {} reactor-dependent test site(s) at line(s) {:?} are not \
                     accounted for: the file's first census marker is at line {first}, \
                     below them.",
                    orphans.len(),
                    orphans
                );
            }
        }
        violations.push(msg);
    }
    for (i, (line, n)) in markers.iter().enumerate() {
        let end = markers
            .get(i + 1)
            .map(|(next, _)| *next)
            .unwrap_or(usize::MAX);
        let found: Vec<usize> = ungated
            .iter()
            .copied()
            .filter(|s| *s > *line && *s < end)
            .collect();
        if found.len() != *n {
            let mut msg = String::new();
            let _ = write!(
                msg,
                "{path}:{line}: census marker declares {n} ungated reactor-dependent \
                 test site(s) below it, but {} were found at line(s) {:?}. If you added \
                 one, run it under EPICS_RS_BUILD_EXEC_BACKEND=thread and give it its own \
                 marker; only then does a count change.",
                found.len(),
                found
            );
            violations.push(msg);
        }
    }

    Audit {
        ungated,
        scope_gated,
        collateral: unclaimed(gated_tests),
        declared,
        violations,
    }
}

/// The gated test attributes no runtime-building line below them claimed.
fn unclaimed(gated_tests: Vec<(usize, bool)>) -> Vec<usize> {
    gated_tests
        .into_iter()
        .filter(|(_, claimed)| !claimed)
        .map(|(line, _)| line)
        .collect()
}

/// A test attribute that asks for no reactor.
///
/// Read as a path — the last segment decides — rather than matched against a
/// list of spellings, because this workspace never writes `epics_test` bare:
/// it is `#[epics_macros_rs::epics_test]` 1834 times and
/// `#[epics_base_rs::epics_test]` 11 times, and a literal list would have
/// counted zero of them while looking like it worked. A third import path for
/// the same macro needs no edit here.
///
/// `#[tokio::test]` is excluded by path: the site predicate owns it.
fn is_reactor_free_test(trimmed: &str) -> bool {
    let Some(body) = trimmed.strip_prefix("#[") else {
        return false;
    };
    let path = body.split(['(', ']', ' ']).next().unwrap_or(body);
    if path == concat!("tokio", "::test") {
        return false;
    }
    let last = path.rsplit("::").next().unwrap_or(path);
    last == "test" || last == concat!("epics", "_test")
}

/// `mod foo {` / `pub mod foo {` at column 0 — an inline module opening.
fn is_mod_open(raw: &str) -> bool {
    let rest = raw.strip_prefix("pub ").unwrap_or(raw);
    rest.starts_with("mod ") && rest.trim_end().ends_with('{')
}

/// `mod foo;` at column 0 — a module declaration, whose body is another file.
fn mod_decl_name(raw: &str) -> Option<&str> {
    let rest = match raw.strip_prefix("pub") {
        // `pub mod x;`, `pub(crate) mod x;`, `pub(super) mod x;`
        Some(after) => after.trim_start_matches(|c| c != 'm'),
        None => raw,
    };
    let name = rest.strip_prefix("mod ")?.trim().strip_suffix(';')?.trim();
    let ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    ok.then_some(name)
}

/// The file a `#[path = "..."]` attribute names, if this is one.
fn path_attr_value(attr: &str) -> Option<String> {
    let body = attr.strip_prefix("#[")?.strip_suffix(']')?.trim();
    let rest = body.strip_prefix("path")?.trim_start().strip_prefix('=')?;
    let quoted = rest.trim();
    Some(quoted.strip_prefix('"')?.strip_suffix('"')?.to_owned())
}

/// One `mod x;` declaration a gate removes, as read from the declaring file.
struct GatedModDecl {
    line: usize,
    name: String,
    /// The `#[path = "..."]` override, relative to the DECLARING FILE's
    /// directory — which is the rule for a `mod` declaration, and is not the
    /// module directory the unqualified form resolves against.
    path_override: Option<String>,
}

/// The column-0 `#[cfg(<gate>)] mod x;` declarations in one file.
///
/// This is the accounting no reading of the removed file can perform: the gate
/// is here and the tests are there. Column 0 for the same reason
/// [`is_mod_open`] is — `cargo fmt --all` is mandatory on this repo, so a
/// declaration at any other indentation is inside an inline module, whose own
/// gate the site scan already reads.
fn gated_mod_declarations(lines: &[&str], attrs: &Attrs) -> Vec<GatedModDecl> {
    let mut out = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        if raw.is_empty() || raw.len() != trimmed.len() || attrs.continuation[idx] {
            continue;
        }
        match attrs.opened[idx].as_deref() {
            Some(attr) if !trimmed.starts_with("#![") => {
                pending.push(attr);
                continue;
            }
            _ => {}
        }
        if let Some(name) = mod_decl_name(raw)
            && pending.iter().any(|a| is_gate_attr(a))
        {
            out.push(GatedModDecl {
                line: idx + 1,
                name: name.to_owned(),
                path_override: pending.iter().find_map(|a| path_attr_value(a)),
            });
        }
        if !raw.starts_with("//") {
            pending.clear();
        }
    }
    out
}

/// Resolve `.` and `..` without touching the filesystem, so a `#[path]` that
/// climbs out of its directory still compares against the scanned file list.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The directory an unqualified `mod x;` in this file resolves against.
fn module_dir(file: &Path) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
    match file.file_name().and_then(|n| n.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => dir,
        _ => dir.join(file.file_stem().unwrap_or_default()),
    }
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
fn per_test_gated(lines: &[&str], attrs: &Attrs, idx: usize) -> bool {
    for above in (0..idx).rev() {
        if attrs.continuation[above] {
            continue;
        }
        if let Some(attr) = attrs.opened[above].as_deref() {
            if is_gate_attr(attr) {
                return true;
            }
            continue;
        }
        let t = lines[above].trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        return false;
    }
    false
}

// ---------------------------------------------------------------------------
// What a `cfg` predicate is worth in the configuration the census describes
// ---------------------------------------------------------------------------

/// Truth of a `cfg` predicate on the exec backend.
///
/// Three-valued because the configuration decides only two atoms — the
/// `exec_backend`/`tokio_backend` pair `build.rs` derives. Everything else a predicate may name (the host OS, an unrelated
/// feature, a bespoke `--cfg`) is [`Truth::Unknown`], which is never a gate:
/// the guard fails towards counting a site rather than towards excusing one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Truth {
    On,
    Off,
    Unknown,
}

impl Truth {
    fn negate(self) -> Truth {
        match self {
            Truth::On => Truth::Off,
            Truth::Off => Truth::On,
            Truth::Unknown => Truth::Unknown,
        }
    }
}

/// Evaluate a `cfg` predicate with `cfg(test)` worth `test`.
fn eval(pred: &str, test: Truth) -> Truth {
    let p = pred.trim();
    if let Some(inner) = strip_call(p, "not") {
        return eval(inner, test).negate();
    }
    if let Some(inner) = strip_call(p, "all") {
        let vs: Vec<Truth> = split_predicates(inner).map(|x| eval(x, test)).collect();
        if vs.contains(&Truth::Off) {
            return Truth::Off;
        }
        // `all()` over nothing is true, and so is `all` over all-true.
        return if vs.iter().all(|v| *v == Truth::On) {
            Truth::On
        } else {
            Truth::Unknown
        };
    }
    if let Some(inner) = strip_call(p, "any") {
        let vs: Vec<Truth> = split_predicates(inner).map(|x| eval(x, test)).collect();
        if vs.contains(&Truth::On) {
            return Truth::On;
        }
        return if vs.iter().all(|v| *v == Truth::Off) {
            Truth::Off
        } else {
            Truth::Unknown
        };
    }
    atom(p, test)
}

/// `feature = "x"` / `unix` / `test` — everything with no `(..)` under it.
fn atom(p: &str, test: Truth) -> Truth {
    let squashed: String = p.chars().filter(|c| !c.is_whitespace()).collect();
    if squashed == "test" {
        test
    } else if squashed == GATE_PREDICATE {
        Truth::Off
    } else if squashed == EXEC_BACKEND_PREDICATE {
        Truth::On
    } else {
        Truth::Unknown
    }
}

/// `kw(<inner>)` — `None` unless the trailing `)` is the one the leading `(`
/// opened, so `any(a), b` is not read as a call over `a), b`.
fn strip_call<'a>(p: &'a str, kw: &str) -> Option<&'a str> {
    let inner = p
        .strip_prefix(kw)?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    (depth == 0).then_some(inner)
}

/// Split a comma-separated predicate list at depth zero.
fn split_predicates(list: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (i, c) in list.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&list[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if !list[start..].trim().is_empty() {
        out.push(&list[start..]);
    }
    out.into_iter()
}

/// The predicate of `#[cfg(..)]` or `#![cfg(..)]`; `None` for anything else.
fn cfg_body(attr: &str) -> Option<&str> {
    let a = attr.trim();
    let a = a.strip_prefix("#![").or_else(|| a.strip_prefix("#["))?;
    a.strip_suffix(']')?
        .trim()
        .strip_prefix("cfg")?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// Does this attribute compile its item *away* on the exec backend?
///
/// Decided by evaluating the predicate, not by looking for the spelling
/// [`GATE_PREDICATE`] happens to carry: a gate is any predicate that is
/// false in the configuration the census describes, however it is written
/// and however it is wrapped. That is what
/// makes the guard's answer a property of the tree rather than of the list of
/// literals someone remembered to add here — `not(tokio_backend)` and
/// `any(not(feature = ".."), feature = "client")` are *not* gates because
/// their items do exist on the exec backend, and both fall out of the
/// evaluation instead of needing a carve-out.
fn is_gate_attr(attr: &str) -> bool {
    cfg_body(attr).is_some_and(|p| eval(p, Truth::On) == Truth::Off)
}

/// Does this attribute hold only where `cfg(test)` does?
///
/// This is what marks a column-0 `mod` as test code, and therefore whether a
/// hand-built runtime inside it is a site. Asking the predicate rather than
/// looking for the literal `cfg(test)` is what sees `all(test, unix)` and
/// `all(test, target_os = "linux")`, which the literal reads as production
/// code — the direction that hides a breach rather than inventing one.
fn requires_test(attr: &str) -> bool {
    cfg_body(attr)
        .is_some_and(|p| eval(p, Truth::Off) == Truth::Off && eval(p, Truth::On) != Truth::Off)
}

// ---------------------------------------------------------------------------
// Attributes as units, not lines
// ---------------------------------------------------------------------------

/// Every attribute in a file, joined across the lines rustfmt wrapped it over.
///
/// A line-at-a-time reader sees neither a predicate nor an attribute in a
/// wrapped `#[cfg(any(\n    ..\n))]`, and its closing `))]` at column 0 reads
/// as ordinary code — which discards the attributes collected for the `mod`
/// below it. Both halves of that are silent, so the wrapped form has to be
/// reassembled before anything asks it a question.
struct Attrs {
    /// `Some(joined)` on each line that *opens* an attribute.
    opened: Vec<Option<String>>,
    /// The lines an opened attribute continues onto.
    continuation: Vec<bool>,
}

impl Attrs {
    fn collect(lines: &[&str]) -> Attrs {
        let mut opened = vec![None; lines.len()];
        let mut continuation = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            let t = lines[i].trim();
            if !(t.starts_with("#[") || t.starts_with("#![")) {
                i += 1;
                continue;
            }
            let mut joined = t.to_string();
            let mut depth = bracket_depth(t);
            let mut j = i;
            while depth > 0 && j + 1 < lines.len() {
                j += 1;
                continuation[j] = true;
                joined.push(' ');
                joined.push_str(lines[j].trim());
                depth += bracket_depth(lines[j]);
            }
            opened[i] = Some(joined);
            i = j + 1;
        }
        Attrs {
            opened,
            continuation,
        }
    }
}

fn bracket_depth(line: &str) -> i32 {
    line.chars()
        .map(|c| match c {
            '[' => 1,
            ']' => -1,
            _ => 0,
        })
        .sum()
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

/// The entries of one directory the scan walks.
///
/// An ABSENT directory contributes nothing — a crate need not have `tests/`.
/// One that exists and cannot be read does not: the `let Ok(..) else { return }`
/// this replaces made an unreadable directory indistinguishable from an empty
/// one, so the scan reported "nothing to account for" over a tree full of
/// files. The per-entry error is raised for the same reason `entries.flatten()`
/// must not swallow it — that drops files from the walk one at a time.
fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(e) => panic!(
            "rtems-exec-gate: cannot read directory {}: {e}",
            dir.display()
        ),
    };
    entries
        .map(|entry| {
            entry
                .unwrap_or_else(|e| {
                    panic!(
                        "rtems-exec-gate: cannot read an entry of {}: {e}",
                        dir.display()
                    )
                })
                .path()
        })
        .collect()
}

/// Every `.rs` file belonging to the crate rooted at `dir`, excluding nested
/// packages and build output.
fn crate_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for path in dir_entries(dir) {
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default();
            // A nested package carries its own manifest and, if it is a
            // workspace member, its own census; `target` is not source.
            if name == "target" || path.join("Cargo.toml").is_file() {
                continue;
            }
            crate_rust_files(&path, out);
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
/// The scan narrows on TWO axes, `src` and `tests`, and `min_files` is a floor
/// on their SUM — which protects neither. `epics-ca-rs` carries 117 files under
/// `src` against a floor of 100, so renaming `tests/` away takes every
/// integration test out of the census and the sum still clears the floor. A
/// floor has to sit on the same axis as the narrowing it guards, so each root
/// that exists is required to contribute, and the failure names both counts.
///
/// Residual, stated rather than papered over: a `tests/` directory that is
/// DELETED outright contributes zero legitimately — a crate need not have
/// integration tests — and only `min_files` stands behind that. The census
/// added by `tests/every_backend_deriving_crate_is_accounted_for.rs` is what keeps a
/// crate from dropping out of the scan entirely.
///
/// Call it from a `tests/rtems_exec_model_gate.rs` that is itself gated to
/// `#![cfg(exec_backend)]`, so the census is checked in exactly
/// the configuration it describes.
/// One crate's census, as measured — the whole-crate totals behind
/// [`assert_crate_is_accounted_for`].
///
/// Split out because what a caller can usefully pin is whole-crate, and not
/// derivable from a per-file [`Audit`] without redoing the scan.
#[derive(Debug, PartialEq, Eq)]
pub struct CrateCensus {
    /// `.rs` files read, across the `src` and `tests` axes.
    pub files: usize,
    /// Reactor-dependent sites left in the census, summed over those files.
    pub ungated: usize,
    /// The files, in file order, whose file-level `#![cfg(..)]` or gated
    /// column-0 `mod` removed anything at all — a reactor-dependent site, a
    /// reactor-free test, or both — each with what that gate cost.
    ///
    /// A file gated over nothing but reactor-free tests has no
    /// [`Audit::scope_gated`] site and would be absent from a list keyed on
    /// sites, while being the case that loses the most test coverage per gate.
    /// Keyed on cost instead, it is here with `sites: 0`. A caller pinning the
    /// narrower site-accounting subject filters on `sites > 0` at its own
    /// assertion, where the narrowing is visible.
    ///
    /// A list rather than a count, and that is the whole point: a count is
    /// branch-local. Two branches that each change their own gating produce a
    /// merged tree holding a number neither of them computed, and because the
    /// constant itself is untouched on one side the merge is clean and says
    /// nothing. Names compose — the merged tree's set is the union of the two
    /// branches' sets, which is the right answer — and a duplicate entry is
    /// detectable where a wrong total is not.
    ///
    /// The cost rides on the name rather than in a second vector so the two can
    /// never disagree about which files are gated.
    pub scope_gated: Vec<GateCost>,
    /// The crate's `#[cfg(<gate>)] mod x;` declarations, in file order, each
    /// with the files it removes and what removing them cost.
    ///
    /// The other half of the same question `scope_gated` answers, and the half
    /// no reading of a removed file can reach: the gate is in the declaring
    /// file and the tests are in the removed one, so nothing in the removed
    /// file says it is gated. Kept apart from `scope_gated` rather than merged
    /// into it because the remedy differs — an in-file gate is narrowed or
    /// split where it stands, while this one is fixed at a declaration in a
    /// different file — and because the two are disjoint by construction here,
    /// so their costs add without double counting.
    ///
    /// Report-only: it changes no accounting and produces no violation.
    pub mod_gated: Vec<ModGate>,
    /// Per-file rule breaches, in file order; empty means the crate complies.
    pub violations: Vec<String>,
}

/// One `#[cfg(<gate>)] mod x;` declaration and the files it removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModGate {
    /// The file holding the declaration, relative to the manifest directory —
    /// where the remedy goes, which is never the removed file.
    pub declared_in: String,
    /// 1-based line of the `mod x;` declaration.
    pub line: usize,
    /// The module name as declared.
    pub module: String,
    /// The files the declaration removes — its own source and everything
    /// beneath it — each with what the gate cost, in path order. A file an
    /// earlier declaration already claimed, or one its own `#![cfg(..)]`
    /// already put in [`CrateCensus::scope_gated`], is not repeated here.
    pub removes: Vec<GateCost>,
}

/// One scope-gated file and what its gate cost the census.
///
/// [`GateCost::sites`] is the gate's reason and [`GateCost::collateral`] is its
/// price; a reviewer signing off a list of gated files needs both, because the
/// name alone cannot say whether the file was gated for its own code or dragged
/// along beside code that needed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateCost {
    /// Path relative to the manifest directory.
    pub file: String,
    /// Reactor-dependent sites the gate removed — why it is there.
    pub sites: usize,
    /// Reactor-free tests the same gate removed — what it cost. Zero reads as
    /// correctly gated; a large number reads as a candidate to split.
    pub collateral: usize,
}

/// Scan a crate and report its census without judging it.
///
/// Reads EVERY `.rs` file under the manifest directory, stopping only at a
/// nested package — a subdirectory with its own `Cargo.toml`, such as a
/// `cargo-fuzz` target set, which belongs to that package's own census — and at
/// `target`. It used to scan a fixed `["src", "tests"]` and that is a list to
/// step around: a file under `benches`, `examples` or a renamed `tests`
/// directory obtained reactor-dependent test sites that no reading of this
/// crate could see. There is no axis list now, so there is nothing to rename
/// past.
///
/// Panics if the scan itself is broken — an unreadable directory, or a crate
/// with no `.rs` file at all. A census whose scan silently reads nothing
/// reports a compliant crate, which is the failure this whole crate exists to
/// prevent.
pub fn audit_crate(manifest_dir: &str) -> CrateCensus {
    let root = Path::new(manifest_dir);
    let mut files = Vec::new();
    crate_rust_files(root, &mut files);
    assert!(
        !files.is_empty(),
        "the census read no .rs file at all under {}; it is not scanning the crate",
        root.display()
    );
    files.sort();

    let mut census = CrateCensus {
        files: files.len(),
        ungated: 0,
        scope_gated: Vec::new(),
        mod_gated: Vec::new(),
        violations: Vec::new(),
    };
    // Read once: the ordinary scan and the declaration scan both need the text,
    // and the second pass resolves declarations against the same file list.
    let mut sources: Vec<(PathBuf, String, String)> = Vec::with_capacity(files.len());
    // Seeded with the in-file-gated files so a file cannot be counted under
    // both shapes; see [`CrateCensus::mod_gated`].
    let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();
    for path in &files {
        let integration = path.starts_with(root.join("tests"));
        let rel = repo_path(root, path);
        let text = fs::read_to_string(path).expect("read crate source");
        let audit = audit_source(&rel, &text, integration);
        census.ungated += audit.ungated.len();
        if !audit.scope_gated.is_empty() || !audit.collateral.is_empty() {
            claimed.insert(path.clone());
            census.scope_gated.push(GateCost {
                file: rel.clone(),
                sites: audit.scope_gated.len(),
                collateral: audit.collateral.len(),
            });
        }
        census.violations.extend(audit.violations);
        sources.push((path.clone(), rel, text));
    }

    let present: BTreeSet<&PathBuf> = files.iter().collect();
    for (path, rel, text) in &sources {
        let lines: Vec<&str> = text.lines().collect();
        let attrs = Attrs::collect(&lines);
        for decl in gated_mod_declarations(&lines, &attrs) {
            let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
            let head = match &decl.path_override {
                Some(p) => normalise(&dir.join(p)),
                None => {
                    let md = module_dir(path);
                    let flat = md.join(format!("{}.rs", decl.name));
                    if present.contains(&flat) {
                        flat
                    } else {
                        md.join(&decl.name).join("mod.rs")
                    }
                }
            };
            // Everything under the module's own directory goes with it. For
            // `x/mod.rs` that is `x/`; for `x.rs` it is the sibling `x/`.
            let subtree = if head.file_name().is_some_and(|n| n == "mod.rs") {
                head.parent().unwrap_or(Path::new("")).to_path_buf()
            } else {
                head.with_extension("")
            };
            let mut removed: Vec<&PathBuf> = files
                .iter()
                .filter(|f| **f == head || f.starts_with(&subtree))
                .collect();
            removed.sort();
            let mut removes = Vec::new();
            for f in removed {
                if !claimed.insert(f.clone()) {
                    continue;
                }
                let frel = repo_path(root, f);
                let ftext = fs::read_to_string(f).expect("read crate source");
                removes.push(removal_cost(
                    &frel,
                    &ftext,
                    f.starts_with(root.join("tests")),
                ));
            }
            if !removes.is_empty() {
                census.mod_gated.push(ModGate {
                    declared_in: rel.clone(),
                    line: decl.line,
                    module: decl.name,
                    removes,
                });
            }
        }
    }
    census
}

/// Fail if the crate breaches the rule.
///
/// Takes no floor. It used to take a `min_files`, a per-crate number meant to
/// catch a scan that reads nothing — and a floor under a file count that only
/// grows is inert almost immediately: `epics-pva-rs` sat at 156 files against
/// its floor of 150. What that floor was reaching for is now structural.
/// [`audit_crate`] walks every `.rs` file in the crate rather than a fixed list
/// of directories, so there is no axis to rename past, and it fails outright on
/// a crate it finds no source in.
pub fn assert_crate_is_accounted_for(manifest_dir: &str) {
    let census = audit_crate(manifest_dir);
    assert!(
        census.violations.is_empty(),
        "{RULE}\n\n{} file(s) breach it:\n  {}",
        census.violations.len(),
        census.violations.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The workspace half: which crates the rule judges, what the pin should say
// about a tree, and why a file the pin names has stopped measuring as gated.
//
// This used to live only in the census test, reachable only by making a tree's
// own test fail and reading the panic. That is fine for a tree you are editing
// and useless for one you are merging: a merged tree that has not been built
// yet cannot be asked, and a tree that passes prints nothing at all. The
// measurement and the pin's source text come from here now, so a caller that
// only wants to READ a tree can have both without asserting anything about it.
// ---------------------------------------------------------------------------

/// The environment variable that selects the backend on a host build.
///
/// It replaced a cargo feature, and the reason is the one this whole census
/// exists to police: a feature that flips a backend is not additive, so
/// `--all-features` resolved to the reactor-FREE backend and no single cargo
/// invocation meant "everything on". Every gate below now reads a cfg that one
/// rule derives from this variable and the target triple together, so there is
/// no second axis for a gate to name by mistake.
pub const EXEC_BACKEND_ENV: &str = "EPICS_RS_BUILD_EXEC_BACKEND";

/// The one derivation, as every backend-deriving `build.rs` must spell it.
///
/// 23 build scripts carry a copy, because a build script may only `include!`
/// files inside its own package and a shared build-dependency would have to be
/// published before any of the 13 publishable crates could be. Copies that may
/// drift are the defect a single cfg name exists to prevent, so
/// [`derivation_breaches`] holds all 23 against this text byte for byte. That
/// subsumes the narrower audit this move actually requires — the
/// `cargo::rerun-if-env-changed` line, without which cargo reuses artefacts
/// built under the previous value and the backend silently disagrees with the
/// request — because a copy that matches cannot be missing a line of it.
pub const CANONICAL_DERIVATION: &str = r#"    // Build-time backend selection, from the environment rather than from a
    // cargo feature: a feature that flips a backend is not additive, so
    // `--all-features` turned the reactor off and no single invocation meant
    // "everything on". `epics-libcom-rs`'s module docs carry the reasoning;
    // `tools/rtems-exec-gate` holds every copy of this block against that
    // crate's, so 23 derivations of one rule cannot drift apart.
    println!("cargo::rerun-if-env-changed=EPICS_RS_BUILD_EXEC_BACKEND");
    let requested = std::env::var_os("EPICS_RS_BUILD_EXEC_BACKEND").unwrap_or_default();
    let host_exec_backend = match requested.to_string_lossy().as_ref() {
        "thread" => true,
        "" | "tokio" => false,
        bad => panic!(
            "EPICS_RS_BUILD_EXEC_BACKEND={bad}: the exec backend is `thread` \
             (reactor-free std threads) or `tokio` (the host default, which an \
             unset or empty variable also selects)"
        ),
    };
    if embedded_target || host_exec_backend {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }"#;

/// A source file named the way the census and the cost report name it:
/// relative to the crate root and `/`-separated on every host.
///
/// `Path::display` spells the separator the host uses, so the same tree
/// answered `src\lib.rs` on Windows and `src/lib.rs` everywhere else — two
/// answers to a question about the tree, which nothing about the host should
/// reach. `source-guard` builds its labels the same way; the two cannot share
/// one helper, because this crate sits in every RTEMS-closure crate's build
/// graph and taking a dependency to save six lines would put `source-guard`
/// there too.
fn repo_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The path, from a workspace root, of the file holding the `SCOPE_GATED` pin.
pub const PIN_FILE: &str =
    "tools/rtems-exec-gate/tests/every_backend_deriving_crate_is_accounted_for.rs";

/// Every workspace member directory, from the root manifest's own `members`
/// globs rather than from a list repeated here.
///
/// Panics on a glob shape it cannot expand: a shape that silently matched
/// nothing would take its crates out of the census without saying so.
pub fn workspace_members(root: &Path) -> Vec<PathBuf> {
    let manifest = root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));
    let after = text
        .split_once("members")
        .expect("the workspace manifest declares `members`")
        .1;
    let list = after
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .expect("`members` is an array")
        .0;

    let mut out = Vec::new();
    for pattern in list.split(',') {
        let pattern = pattern.trim().trim_matches('"');
        if pattern.is_empty() || pattern.starts_with('#') {
            continue;
        }
        match pattern.strip_suffix("/*") {
            Some(group) => {
                let dir = root.join(group);
                let entries = fs::read_dir(&dir)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
                for entry in entries {
                    let path = entry.expect("workspace group entry").path();
                    if path.join("Cargo.toml").is_file() {
                        out.push(path);
                    }
                }
            }
            None => {
                assert!(
                    !pattern.contains(['*', '?', '[']),
                    "workspace member glob `{pattern}` is a shape this census \
                     cannot expand; teach `workspace_members` about it rather \
                     than leaving its crates unaudited"
                );
                out.push(root.join(pattern));
            }
        }
    }
    out.sort();
    out
}

/// Whether a crate is inside the rule: its `build.rs` derives the backend.
///
/// This read the manifests while the backend was a cargo feature. It cannot
/// any more — there is no key to find — and the replacement is better placed
/// rather than merely equivalent: a manifest key only ever proved that a name
/// existed, while emitting the cfg is the crate actually taking part in the
/// decision the census reasons about.
pub fn derives_the_backend(build_rs: &Path) -> bool {
    fs::read_to_string(build_rs).is_ok_and(|t| t.contains(BACKEND_CFG))
}

/// The `cargo::rustc-cfg=..` line a build script emits for the hosted backend.
pub const BACKEND_CFG: &str = "rustc-cfg=tokio_backend";

/// The crate that owns this rule, and the one crate allowed to name the backend
/// cfg without deriving it.
///
/// It names both atoms all over its fixtures and its own docs while compiling
/// under neither, so the reverse check below would report it forever.
pub const RULE_OWNER: &str = "rtems-exec-gate";

/// Whether a crate's own sources gate on the backend cfg.
///
/// The second source the crate set is checked against. While the backend was a
/// feature, the two sources were the manifests and the build scripts; with the
/// manifest half gone this replaces it, and it fails in the direction that
/// matters: a crate that gates on `tokio_backend` but whose `build.rs` never
/// emits it compiles the gated code away on every target and says nothing.
pub fn names_the_backend_cfg(dir: &Path) -> bool {
    fn walk(dir: &Path, out: &mut bool) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(text) = fs::read_to_string(&path)
            {
                for line in text.lines() {
                    let l = line.trim();
                    let is_cfg = l.starts_with("#[cfg") || l.starts_with("#![cfg");
                    if (is_cfg || l.contains("cfg!("))
                        && (l.contains(GATE_PREDICATE) || l.contains(EXEC_BACKEND_PREDICATE))
                    {
                        *out = true;
                        return;
                    }
                }
            }
        }
    }
    let mut found = false;
    for sub in ["src", "tests", "benches", "examples"] {
        walk(&dir.join(sub), &mut found);
        if found {
            return true;
        }
    }
    false
}

/// Every backend-deriving `build.rs` whose copy of [`CANONICAL_DERIVATION`] has
/// drifted, named with what to do about it.
pub fn derivation_breaches(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for (name, dir) in declaring_crates(root) {
        let build_rs = dir.join("build.rs");
        let text = fs::read_to_string(&build_rs)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", build_rs.display()));
        if !text.contains(CANONICAL_DERIVATION) {
            out.push(format!(
                "{name}/build.rs derives the backend but its copy of the \
                 derivation is not `rtems_exec_gate::CANONICAL_DERIVATION`. \
                 Copy that constant back verbatim: a paraphrase that drops \
                 `cargo::rerun-if-env-changed={EXEC_BACKEND_ENV}` leaves this \
                 crate built against whatever the variable said last time, and \
                 a paraphrase that changes the accepted values leaves one crate \
                 on a different backend from the rest of the graph."
            ));
        }
    }
    out
}

/// The crates the census judges — name and directory — sorted by name.
pub fn declaring_crates(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = workspace_members(root)
        .into_iter()
        .filter(|dir| derives_the_backend(&dir.join("build.rs")))
        .map(|dir| {
            let name = dir
                .file_name()
                .expect("member directory has a name")
                .to_string_lossy()
                .into_owned();
            (name, dir)
        })
        .collect();
    out.sort();
    out
}

/// Whether a gated file belongs in the pin.
///
/// The census reports every file a scope gate cost something; the pin's subject
/// is the narrower one of files whose gate took a reactor-dependent SITE out of
/// the accounting. A gate over nothing but reactor-free tests changes no
/// accounting and belongs to the cost report, not here.
pub fn counts_for_the_pin(cost: &GateCost) -> bool {
    cost.sites != 0
}

/// What `SCOPE_GATED` should say about this tree: every `(crate, file)` whose
/// scope gate took a reactor-dependent site out of the census.
pub fn scope_gated_set(root: &Path) -> BTreeSet<(String, String)> {
    let mut measured = BTreeSet::new();
    for (name, dir) in declaring_crates(root) {
        let path = dir.to_str().expect("crate path is UTF-8");
        for cost in audit_crate(path).scope_gated {
            if counts_for_the_pin(&cost) {
                measured.insert((name.clone(), cost.file.replace('\\', "/")));
            }
        }
    }
    measured
}

/// What the tree's own `SCOPE_GATED` currently names.
///
/// Reads the pin as text — consecutive string literals inside the constant,
/// paired — so a `rustfmt` that splits an entry across three lines reads the
/// same as one that fits it on one.
pub fn pinned_scope_gated(root: &Path) -> BTreeSet<(String, String)> {
    let file = root.join(PIN_FILE);
    let text =
        fs::read_to_string(&file).unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()));
    let body = text
        .split_once("const SCOPE_GATED")
        .and_then(|(_, rest)| rest.split_once("&["))
        .and_then(|(_, rest)| rest.split_once("];"))
        .unwrap_or_else(|| panic!("{} holds no `SCOPE_GATED` constant", file.display()))
        .0;
    let mut lits = Vec::new();
    let mut rest = body;
    while let Some((_, after)) = rest.split_once('"') {
        let Some((lit, tail)) = after.split_once('"') else {
            break;
        };
        lits.push(lit.to_owned());
        rest = tail;
    }
    assert!(
        lits.len() % 2 == 0,
        "`SCOPE_GATED` in {} has an odd number of string literals, so its \
         entries do not pair into (crate, file)",
        file.display()
    );
    lits.chunks(2)
        .map(|p| (p[0].clone(), p[1].clone()))
        .collect()
}

/// The pin's own source text for a set, ready to paste over the constant.
///
/// Emitted in the layout `rustfmt` writes, not the convenient one. `cargo fmt
/// --all` is a mandatory gate here and does not skip this file, so a paste it
/// would re-wrap leaves a perfectly current value failing `--check` — and the
/// moment this text is pasted is a merge, which is the moment nobody is
/// reading the diff. `tools/env-codegen` and `tools/dbd-codegen` reach the same
/// fixed point by piping their emitted source through the real `rustfmt`; this
/// one cannot spawn a process, because it also renders the census assertion's
/// failure message and a missing formatter must not turn a census breach into
/// an unrelated error. So it reproduces the single rule that applies to a
/// `(&str, &str)` entry, and `pin_maintenance.rs` holds the reproduction
/// against the real formatter on entries either side of the width.
pub fn scope_gated_literal(set: &BTreeSet<(String, String)>) -> String {
    // rustfmt's default `fn_call_width`: the widest argument list it keeps on
    // one line before going vertical. A tuple's arguments are its two literals.
    const FN_CALL_WIDTH: usize = 60;

    let mut out = String::from("const SCOPE_GATED: &[(&str, &str)] = &[\n");
    for (c, f) in set {
        let args = format!("\"{c}\", \"{f}\"");
        if args.len() <= FN_CALL_WIDTH {
            let _ = writeln!(out, "    ({args}),");
        } else {
            let _ = writeln!(out, "    (\n        \"{c}\",\n        \"{f}\",\n    ),");
        }
    }
    out.push_str("];\n");
    out
}

/// Why a file the pin names no longer measures as scope-gated.
///
/// The distinction the merge review turns on: only [`Dropped::GateLost`] puts
/// reactor-dependent sites back into the census. The other three are a file
/// that stopped *needing* to be in the pin, and dropping it loses no coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dropped {
    /// The file is not in the tree any more — renamed, moved or deleted.
    Gone,
    /// Still gated. The gate simply has no reactor-dependent site left to take;
    /// `collateral` reactor-free tests still go with it.
    StillGated { collateral: usize },
    /// A parent's `#[cfg] mod` now removes the whole file, so its sites did not
    /// come back either — they moved to the other accounting.
    ModGated { declared_in: String, line: usize },
    /// The gate is gone: `sites` reactor-dependent sites are back in the census
    /// and now need markers. This is the one that is a real change.
    GateLost { sites: usize },
}

/// Classify a `(crate, file)` the pin names but the tree no longer measures.
pub fn why_dropped(crate_dir: &Path, file: &str) -> Dropped {
    let full = crate_dir.join(file);
    if !full.is_file() {
        return Dropped::Gone;
    }
    let census = audit_crate(crate_dir.to_str().expect("crate path is UTF-8"));
    let same = |c: &str| c.replace('\\', "/") == file;
    if let Some(cost) = census.scope_gated.iter().find(|c| same(&c.file)) {
        return Dropped::StillGated {
            collateral: cost.collateral,
        };
    }
    for m in &census.mod_gated {
        if m.removes.iter().any(|c| same(&c.file)) {
            return Dropped::ModGated {
                declared_in: m.declared_in.clone(),
                line: m.line,
            };
        }
    }
    let text = fs::read_to_string(&full).expect("read crate source");
    let audit = audit_source(file, &text, full.starts_with(crate_dir.join("tests")));
    Dropped::GateLost {
        sites: audit.ungated.len(),
    }
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

    /// A file-level gate accounts for its sites, but it must not make them
    /// vanish: gate the file, drop the marker, and every site below is out of
    /// the census with nothing left at the site to say so. Reporting them is
    /// what lets a caller pin the number.
    #[test]
    fn a_file_level_gate_reports_the_sites_it_removed() {
        let src = format!(
            "{FILE_GATE_ATTR}\n{a}\nasync fn one() {{}}\n{a}\nasync fn two() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("gated.rs", &src, true);
        assert!(audit.ungated.is_empty());
        assert_eq!(audit.scope_gated, vec![2, 4]);
    }

    /// The same for option 2 — one `#[cfg(..)]` at column 0 can empty an
    /// arbitrarily large module.
    #[test]
    fn a_gated_module_reports_the_sites_it_removed() {
        let src = format!(
            "{GATE_ATTR}\nmod t {{\n    {a}\n    async fn one() {{}}\n}}\n{a}\nasync fn two() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("modgated.rs", &src, true);
        assert_eq!(audit.scope_gated, vec![3]);
        assert_eq!(
            audit.ungated,
            vec![6],
            "the module closes at column 0, so what follows is back in the census"
        );
    }

    /// Option 3 is deliberately NOT reported: the gate is the line directly
    /// above the test, so removing the test removes the gate with it and the
    /// diff shows both. Counting it would make every per-test gate edit a
    /// whole-crate pin edit for no gain.
    #[test]
    fn a_per_test_gate_is_not_counted_as_scope_gated() {
        let src = format!(
            "// why\n{GATE_ATTR}\n{a}\nasync fn gated() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("pertest.rs", &src, true);
        assert!(audit.ungated.is_empty());
        assert!(audit.scope_gated.is_empty());
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

    /// The inverse selects the exec backend, so it is not a gate: a test
    /// carrying it *does* run on the exec backend and stays in the
    /// census.
    #[test]
    fn the_exec_backend_predicate_is_not_a_gate() {
        let src = format!(
            "#[cfg(not({GATE_PREDICATE}))]\n{a}\nasync fn t() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("t.rs", &src, true);
        assert_eq!(audit.ungated.len(), 1, "{:?}", audit);
    }

    /// Option 3: per-test gate — the `protocol_tests.rs` shape, which is what
    /// keeps that file's pure wire-format tests running on the exec backend.
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
            "//! {CENSUS_MARKER}1): proven to pass on the exec backend\n{}\nasync fn t() {{}}\n",
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
            "//! {CENSUS_MARKER}1): proven to pass on the exec backend\n{a}\nasync fn old() {{}}\n\n{a}\nasync fn added() {{}}\n",
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

    /// A file gate is whatever compiles the file away, not one spelling of it.
    /// `ui25_same_process_discovery.rs` needs `tokio_backend` — the predicate
    /// its UDP subject carries — and had to add a redundant second attribute
    /// purely to be seen, which is the census describing its instrument.
    #[test]
    fn a_file_gate_is_recognised_by_predicate_not_by_spelling() {
        let src = format!(
            "#![cfg(not(any({EXEC_BACKEND_PREDICATE}, ca_blocking_client)))]\n\
             {a}\nasync fn one() {{}}\n{a}\nasync fn two() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("udp.rs", &src, true);
        assert!(audit.ungated.is_empty(), "{:?}", audit.ungated);
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// ...and whitespace inside the attribute is not part of the rule either.
    #[test]
    fn a_file_gate_survives_being_written_without_spaces() {
        let src = format!(
            "#![cfg(not(any({EXEC_BACKEND_PREDICATE},ca_blocking_client)))]\n{}\nasync fn t() {{}}\n",
            tokio_test()
        );
        let audit = audit_source("tight.rs", &src, true);
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// rustfmt wraps a long gate over several lines. Read one line at a time
    /// that is neither a gate nor an attribute, and its closing `))]` at
    /// column 0 additionally discards everything collected for the `mod`.
    #[test]
    fn a_wrapped_module_gate_is_still_a_gate() {
        let src = format!(
            "#[cfg(all(\n    test,\n    feature = \"client\",\n    {GATE_PREDICATE}\n))]\n\
             mod tests {{\n    {a}\n    async fn t() {{}}\n}}\n",
            a = tokio_test()
        );
        let audit = audit_source("src/x.rs", &src, false);
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// The same wrapping directly above a test.
    #[test]
    fn a_wrapped_per_test_gate_is_still_a_gate() {
        let src = format!(
            "#[cfg(all(\n    unix,\n    {GATE_PREDICATE}\n))]\n{a}\nasync fn gated() {{}}\n\n{a}\nasync fn open() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("t.rs", &src, true);
        assert_eq!(audit.ungated, vec![8], "{:?}", audit);
    }

    /// `not(any(exec_backend, ..))` is the third spelling the tree uses —
    /// `client/transport.rs` and `client_native/server_conn.rs` — and it
    /// selects exactly the same builds the other two do.
    #[test]
    fn an_exec_backend_negation_is_a_gate() {
        let src = format!(
            "#[cfg(not(any({EXEC_BACKEND_PREDICATE}, ca_blocking_client)))]\n{a}\nasync fn t() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("t.rs", &src, true);
        assert!(audit.ungated.is_empty(), "{:?}", audit.ungated);
    }

    /// The direction that matters more: a predicate that merely *mentions* the
    /// gate but still holds on the exec backend must not excuse anything.
    /// Substring matching cannot tell these two apart from a real gate, and
    /// getting them wrong hides a site instead of inventing one.
    #[test]
    fn a_predicate_that_still_holds_feature_on_is_not_a_gate() {
        let disjunction = format!(
            "#[cfg(any({GATE_PREDICATE}, feature = \"client\"))]\n{a}\nasync fn t() {{}}\n",
            a = tokio_test()
        );
        assert_eq!(
            audit_source("t.rs", &disjunction, true).ungated,
            vec![2],
            "an `any` arm that is true on the exec backend leaves the test compiled in"
        );
        let partial = format!(
            "#[cfg(not(all({GATE_PREDICATE}, unix)))]\n{a}\nasync fn t() {{}}\n",
            a = tokio_test()
        );
        assert_eq!(
            audit_source("t.rs", &partial, true).ungated,
            vec![2],
            "negating a conjunction the backend only half-decides is not a gate"
        );
    }

    /// A `mod` is test code when its predicate holds only where `cfg(test)`
    /// does — `all(test, target_os = "linux")` is a shape the tree uses, and
    /// reading it as production code makes every hand-built runtime under it
    /// invisible.
    #[test]
    fn a_test_module_gated_on_something_else_still_counts_runtimes() {
        let src = format!(
            "#[cfg(all(test, target_os = \"linux\"))]\nmod t {{\n    #[test]\n    fn f() {{\n{}\n    }}\n}}\n",
            runtime_build()
        );
        let audit = audit_source("src/x.rs", &src, false);
        assert_eq!(audit.ungated.len(), 1, "{:?}", audit.ungated);
    }

    /// A marker beside each site is the shape that keeps a count next to the
    /// thing it counts; each speaks only for what follows it.
    #[test]
    fn a_marker_vouches_only_for_the_sites_below_it() {
        let src = format!(
            "// {CENSUS_MARKER}1): first\n{a}\nasync fn one() {{}}\n\
             // {CENSUS_MARKER}1): second\n{a}\nasync fn two() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("per_site.rs", &src, true);
        assert_eq!(audit.declared, Some(2), "the total is the sum");
        assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    }

    /// ...so a test added below one marker breaks THAT marker, named by its
    /// own line, and leaves every other count alone. This is the case a
    /// single per-file total reports as a number far from the edit.
    #[test]
    fn an_added_test_breaks_the_marker_it_was_added_under() {
        let src = format!(
            "// {CENSUS_MARKER}1): first\n{a}\nasync fn one() {{}}\n\
             // {CENSUS_MARKER}1): second\n{a}\nasync fn two() {{}}\n\
             {a}\nasync fn added() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("per_site.rs", &src, true);
        assert_eq!(audit.violations.len(), 1, "{:?}", audit.violations);
        assert!(
            audit.violations[0].starts_with("per_site.rs:4:"),
            "the failure must name the marker that is wrong: {}",
            audit.violations[0]
        );
    }

    /// A site ABOVE every marker is vouched for by nothing — a marker only
    /// ever speaks for what follows it.
    #[test]
    fn a_site_above_the_first_marker_is_unaccounted() {
        let src = format!(
            "{a}\nasync fn early() {{}}\n// {CENSUS_MARKER}1): late\n{a}\nasync fn late() {{}}\n",
            a = tokio_test()
        );
        let audit = audit_source("late_marker.rs", &src, true);
        assert_eq!(audit.violations.len(), 1, "{:?}", audit.violations);
        assert!(
            audit.violations[0].contains("first census marker is at line 3"),
            "{}",
            audit.violations[0]
        );
    }

    // -- the collateral measurement, one case per boundary ------------------

    /// The shape that motivated the measurement: one gate, a reactor-dependent
    /// site and reactor-free tests, and until now the census reported only the
    /// site. `epics-ca-rs`'s pre-split `repeater.rs` was this at 2 and 10.
    #[test]
    fn a_gate_over_a_mix_reports_both_halves() {
        let src = format!(
            "{}\n{}\nasync fn a() {{}}\n\n#[test]\nfn b() {{}}\n\n#[test]\nfn c() {{}}\n",
            FILE_GATE_ATTR,
            tokio_test()
        );
        let audit = audit_source("mixed.rs", &src, true);
        assert_eq!(audit.scope_gated, vec![2], "the async test is the site");
        assert_eq!(
            audit.collateral,
            vec![5, 8],
            "both plain tests went with it"
        );
    }

    /// The other reading of the same list: a gate whose whole subject needs the
    /// reactor costs nothing and needs no second look.
    #[test]
    fn a_gate_over_only_reactor_work_has_no_collateral() {
        let src = format!(
            "{}\n{}\nasync fn a() {{}}\n{}\nasync fn b() {{}}\n",
            FILE_GATE_ATTR,
            tokio_test(),
            tokio_test()
        );
        let audit = audit_source("pure.rs", &src, true);
        assert_eq!(audit.scope_gated, vec![2, 4]);
        assert!(audit.collateral.is_empty(), "{:?}", audit.collateral);
    }

    /// A gate over nothing but reactor-free tests removes the most coverage per
    /// gate and produces no site at all, so a report keyed on sites cannot see
    /// it. [`audit_crate`] keys on cost for this case.
    #[test]
    fn a_gate_over_only_reactor_free_tests_is_still_a_cost() {
        let src = format!("{}\n#[test]\nfn a() {{}}\n", FILE_GATE_ATTR);
        let audit = audit_source("free.rs", &src, true);
        assert!(audit.scope_gated.is_empty(), "{:?}", audit.scope_gated);
        assert_eq!(audit.collateral, vec![2]);
    }

    /// A plain `#[test]` whose body builds a runtime is reactor-dependent — the
    /// attribute does not decide it, the body does — so it is a site and must
    /// not also be counted as something the gate cost for free.
    #[test]
    fn a_plain_test_that_builds_a_runtime_is_a_site_not_collateral() {
        let src = format!(
            "{}\n#[cfg(test)]\nmod t {{\n    #[test]\n    fn a() {{\n    {}\n    }}\n}}\n",
            FILE_GATE_ATTR,
            runtime_build().trim_start()
        );
        let audit = audit_source("builder.rs", &src, false);
        assert_eq!(audit.scope_gated.len(), 1, "{:?}", audit);
        assert!(audit.collateral.is_empty(), "{:?}", audit.collateral);
    }

    /// Attribution is to the nearest test attribute above, so a runtime built
    /// inside an async test must not reach past it and claim the reactor-free
    /// test before it.
    #[test]
    fn a_runtime_inside_an_async_test_does_not_claim_the_test_above_it() {
        let src = format!(
            "{}\n#[test]\nfn a() {{}}\n\n{}\nasync fn b() {{\n{}\n}}\n",
            FILE_GATE_ATTR,
            tokio_test(),
            runtime_build()
        );
        let audit = audit_source("shielded.rs", &src, true);
        assert_eq!(
            audit.collateral,
            vec![2],
            "the plain test is still collateral"
        );
    }

    /// This workspace never writes `epics_test` bare, so a detector matching
    /// literal spellings would have counted zero of its 1845 uses. The path's
    /// last segment decides, and an attribute that merely modifies a test does
    /// not count.
    #[test]
    fn a_path_qualified_test_macro_counts_and_a_modifier_does_not() {
        let src = format!(
            "{}\n#[epics_macros_rs::epics_test]\nfn a() {{}}\n\n             #[serial_test::serial(epics_env)]\n#[test]\nfn b() {{}}\n",
            FILE_GATE_ATTR
        );
        let audit = audit_source("qualified.rs", &src, true);
        assert_eq!(
            audit.collateral,
            vec![2, 6],
            "the macro and the test, not the modifier"
        );
    }

    /// Option (2) — a gated `mod` rather than a gated file — costs the same way
    /// and is measured the same way.
    #[test]
    fn a_gated_mod_costs_its_reactor_free_tests_too() {
        let src = format!(
            "#[test]\nfn outside() {{}}\n\n{}\nmod t {{\n    #[test]\n    fn inside() {{}}\n}}\n",
            GATE_ATTR
        );
        let audit = audit_source("moded.rs", &src, true);
        assert_eq!(audit.collateral, vec![6], "only the test under the gate");
    }

    /// No gate, no cost: the measurement is what a scope gate removed, not a
    /// count of the file's tests.
    #[test]
    fn an_ungated_file_has_no_collateral() {
        let src = "#[test]\nfn a() {}\n\n#[test]\nfn b() {}\n";
        let audit = audit_source("plain.rs", src, true);
        assert!(audit.collateral.is_empty(), "{:?}", audit.collateral);
    }
}
