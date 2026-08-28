//! Mechanical enforcement of the record-processing runtime-seam rule.
//!
//! `runtime::task::spawn` follows the *ambient* execution model, so on a hosted
//! build it needs a tokio runtime entered on the calling thread. Record
//! processing does not get one. Three callers reach it without a runtime:
//! every blocking CA/PVA connection thread drives processing from a plain
//! `std::thread` through `block_on_sync` → `park_on`; the periodic-scan threads
//! drive it on their own banded threads; and any tail already deferred to the
//! process-global background executor runs on a callback-pool worker. Naming
//! the ambient seam there panics with "there is no reactor running" and kills
//! that thread.
//!
//! The `_background` counterparts — `spawn_background`, `sleep_background`,
//! `interval_background`, `spawn_blocking_background` — land on the
//! process-global background executor on every backend, which is the one
//! executor whose existence does not depend on the caller's thread.
//!
//! # Why this is a crate and not a test module
//!
//! It began as one `#[cfg(test)]` module in `epics-base-rs`, reading its own
//! nine files through `include_str!`. That census cannot see another crate:
//! `include_str!` may not escape the package directory of a published crate.
//! So the rule stopped at the crate boundary and the defect walked across it —
//! `std-rs`'s `ThrottleRecord::spawn_value_sync` and `asyn-rs`'s
//! `AsynRecord::special` / `process_cycle` were all naming `tokio::spawn` from
//! record-support callbacks, and the throttle one began panicking the moment
//! its caller moved onto the callback pool.
//!
//! The answer to "the same rule in three crates" is not three copies of it.
//! Each crate keeps a short `tests/record_seam_gate.rs` (or, for
//! `epics-base-rs`, a module in `server/mod.rs`) naming *its own* roots and
//! exemptions; the rule and its message live here, once. The production slice
//! it runs on is one level further down again, in `source-guard`, shared with
//! every other source guard in the workspace. Dev-only and `publish = false`:
//! it reads source text and is never linked into a shipped artefact.
//!
//! # The rule
//!
//! In the production scope of every source under a crate's census roots, none
//! of [`BANNED`] may appear. The seam is always named by path at a call site,
//! so a textual census is exact for it: `runtime::task::spawn(` matches
//! whether the path is spelled `crate::`, `epics_base_rs::` or
//! `epics_libcom_rs::`, and does not match `spawn_background(`, which is the
//! whole point.
//!
//! # Roots, not lists
//!
//! Each crate names a *directory*, not a set of files. The list form came
//! first and could only be as complete as its author's memory: `epics-base-rs`
//! named 9 of the 115 sources under `src/server`, `std-rs` 6 of 10. A tail
//! deferred in any unnamed file was invisible to a census built to make the
//! rule impossible to break by eye. [`assert_no_ambient_seam_in_tree`] reads
//! the directory, so a new file is covered the moment it exists.
//!
//! A file that genuinely wants the ambient executor — a long-lived loop task
//! started from the caller's own runtime rather than from a framework callback
//! — is named in that crate's `EXEMPT` list *with its reason*, because the
//! reason is the part a reader has to be able to check. An exemption naming a
//! file that no longer exists fails too: a waiver covering nothing is the same
//! staleness the list form had.

use std::fs;
use std::path::{Path, PathBuf};

/// Every spelling of the ambient runtime seam that a record-processing path
/// must not name. `tokio::task::yield_now` is deliberately absent: it wakes
/// the waker immediately when polled off a runtime, so it is correct on either
/// executor.
pub const BANNED: &[&str] = &[
    "tokio::spawn(",
    "tokio::task::spawn_blocking(",
    "tokio::time::sleep(",
    "tokio::time::sleep_until(",
    "tokio::time::interval(",
    "runtime::task::spawn(",
    "runtime::task::spawn_blocking(",
    "runtime::task::sleep(",
    "runtime::task::interval(",
];

/// The file with its test-only items removed, under this crate's old name.
///
/// The rule is [`source_guard`]'s, and the two properties this file's own
/// version did not have are what the census gains: a `#[cfg(test)]` nested in
/// an `impl` is recognised at its own indentation, and comments are out of
/// scope. Both were once argued as the conservative direction, but they are
/// not conservative in the direction that matters — a doc comment explaining
/// *why* `tokio::spawn` is banned on a record path is prose naming the needle,
/// and the census exists to find call sites.
pub fn production_scope(src: &str) -> String {
    source_guard::production_str(src, source_guard::Comments::Strip)
}

/// The rule for one source, applied to its production scope.
fn check_source(name: &str, src: &str) {
    let prod = production_scope(src);
    for banned in BANNED {
        assert_eq!(
            prod.matches(banned).count(),
            0,
            "{name}: `{banned}` in production scope. Record processing is \
             reached from threads with no tokio runtime, so a tail deferred \
             here must use the `_background` counterpart. If this site is a \
             caller's own await, or a long-lived loop the caller starts from \
             its own runtime, name the file in the census's exempt list with \
             that reason."
        );
    }
}

/// Assert that no listed source names the ambient seam in production scope.
///
/// The per-source primitive, kept public because it is what the boundary tests
/// below exercise directly. A *crate* should call
/// [`assert_no_ambient_seam_in_tree`] instead: a hand-written list can only be
/// as complete as its author's memory, and this one was not — see that
/// function.
///
/// `sources` is `(display name, contents)`, built by the caller with
/// `include_str!` so the census reads the same bytes the crate compiles.
pub fn assert_no_ambient_seam(sources: &[(&str, &str)]) {
    assert!(
        !sources.is_empty(),
        "a crate that calls this gate must name at least one source; an empty \
         list proves nothing"
    );
    for (name, src) in sources {
        // Cheap fail-closed check against a list that has gone stale — an
        // entry pointing at a file that was emptied or replaced by a
        // re-export scores zero for every needle and looks clean. The tree
        // form needs no such heuristic: it reads whatever is on disk.
        assert!(
            production_scope(src).contains("fn "),
            "{name}: production scope holds no function at all — this list \
             entry is stale, not clean"
        );
        check_source(name, src);
    }
}

/// Assert that no source under `roots` names the ambient seam, exempting only
/// the files named in `exempt` with their reasons.
///
/// # Why a walk and not a list
///
/// [`assert_no_ambient_seam`] can only see the files its caller remembered to
/// write down, and `include_str!` needs a literal path, so the list cannot be
/// generated. In `epics-base-rs` that list named 9 of the 115 sources under
/// `src/server` — a deferred tail added to any of the other 106 was invisible
/// to a census whose whole purpose was to make the rule impossible to break by
/// eye. This form reads the directory instead, so a *new* file is covered the
/// moment it exists.
///
/// # Both directions fail closed
///
/// A file under `roots` that names the seam and is not exempt fails, and an
/// `exempt` entry that matches no file on disk also fails — otherwise a
/// renamed or deleted file would leave a waiver behind that silently covers
/// nothing, which is the same staleness the list form had.
///
/// `crate_dir` is the caller's `env!("CARGO_MANIFEST_DIR")`; `roots` and the
/// paths in `exempt` are relative to it and are spelled with `/`. Every
/// `exempt` entry carries the reason the file may name the seam, because that
/// reason is the part a reader has to be able to check.
pub fn assert_no_ambient_seam_in_tree(crate_dir: &str, roots: &[&str], exempt: &[(&str, &str)]) {
    assert!(
        !roots.is_empty(),
        "a crate that calls this gate must name at least one root; an empty \
         list proves nothing"
    );
    let base = Path::new(crate_dir);
    let mut sources = Vec::new();
    for root in roots {
        let dir = base.join(root);
        assert!(
            dir.is_dir(),
            "census root `{root}` is not a directory under {crate_dir} — the \
             root moved and the census is now reading nothing"
        );
        collect_rs(&dir, base, &mut sources);
    }
    assert!(
        !sources.is_empty(),
        "the census roots {roots:?} hold no .rs file at all — that is a broken \
         census, not a clean one"
    );
    sources.sort();

    let mut unused: Vec<&str> = exempt.iter().map(|(path, _)| *path).collect();
    for name in &sources {
        if let Some(i) = unused.iter().position(|p| p == name) {
            unused.swap_remove(i);
            continue;
        }
        if exempt.iter().any(|(path, _)| path == name) {
            continue;
        }
        let src = fs::read_to_string(base.join(name))
            .unwrap_or_else(|e| panic!("{name}: cannot read source for the census: {e}"));
        check_source(name, &src);
    }
    assert!(
        unused.is_empty(),
        "census exemptions name files that do not exist under {roots:?}: \
         {unused:?}. A waiver for a file that is gone covers nothing — delete \
         it or fix the path."
    );
}

/// Every `.rs` file under `dir`, named relative to `base` with `/` separators.
fn collect_rs(dir: &Path, base: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read census directory {}: {e}", dir.display()))
        .map(|e| e.expect("cannot read census directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rs(&path, base, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel = path
                .strip_prefix(base)
                .expect("census walked outside the crate directory");
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    // The attribute spellings are assembled with `concat!` so this file's own
    // fixtures are not themselves census hits when someone greps the tree.
    const CFG_TEST: &str = concat!("#[cfg", "(test)]");

    /// The slice rule has its own boundary suite in `source-guard`; what is
    /// this crate's to prove is that the census runs on the production scope
    /// and not on the file — including for a helper nested in an `impl`, which
    /// the slicer this delegated to could not see.
    #[test]
    fn a_seam_named_only_from_a_nested_test_helper_is_allowed() {
        let src = format!(
            "impl A {{\n    {CFG_TEST}\n    fn inner() {{ tokio::spawn(g()); }}\n\
             \n    pub fn ship() {{}}\n}}\n"
        );
        assert_no_ambient_seam(&[("a.rs", &src)]);
    }

    #[test]
    fn a_production_ambient_spawn_is_caught() {
        let src = "fn f() { tokio::spawn(g()); }\n";
        let caught = std::panic::catch_unwind(|| assert_no_ambient_seam(&[("f.rs", src)]));
        assert!(caught.is_err(), "the census must reject a production spawn");
    }

    #[test]
    fn the_same_spawn_inside_a_test_module_is_allowed() {
        let src = format!("fn f() {{}}\n{CFG_TEST}\nmod t {{\n    tokio::spawn(g());\n}}\n");
        assert_no_ambient_seam(&[("f.rs", &src)]);
    }

    #[test]
    fn the_background_counterparts_are_not_matched() {
        let src = "fn f() {\n    crate::runtime::task::spawn_background(g());\n    \
                   crate::runtime::task::sleep_background(d);\n    \
                   crate::runtime::task::interval_background(d);\n    \
                   crate::runtime::task::spawn_blocking_background(h);\n}\n";
        assert_no_ambient_seam(&[("f.rs", src)]);
    }

    #[test]
    fn a_stale_list_entry_with_no_function_is_rejected() {
        let caught =
            std::panic::catch_unwind(|| assert_no_ambient_seam(&[("gone.rs", "pub use x::y;\n")]));
        assert!(
            caught.is_err(),
            "a file with no `fn ` must be reported stale"
        );
    }

    /// The message of a caught panic.
    ///
    /// A case that asserts only `is_err()` proves it caught *a* panic, not
    /// *its own*: every failure mode of a fixture — a root that is missing
    /// because a peer removed it, a file that vanished mid-walk — arrives as
    /// an `Err` too, so such a case scores a pass while proving nothing. The
    /// cases below match the panic they meant to provoke.
    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<String>() {
            return s.clone();
        }
        if let Some(s) = payload.downcast_ref::<&'static str>() {
            return (*s).to_string();
        }
        panic!("panic payload is neither String nor &str");
    }

    /// A throwaway crate directory holding `files`, under a root this process
    /// created exclusively and removes with the returned handle.
    ///
    /// The root was `<tmp>/record-seam-gate-<tag>`, cleared with
    /// `remove_dir_all` on the way in: unique per *case*, but the same path in
    /// every concurrent copy of this binary, so a second run of the suite on
    /// one box deleted the first one's tree mid-walk. Two processes at 4
    /// threads, 30 rounds: 19 rounds failed outright and 6 further cases
    /// passed on a panic they never meant to provoke.
    ///
    /// A per-target-dir root would not have closed it — both of those runs
    /// share one target directory. `tempdir` names the root with `O_EXCL`
    /// instead, so no peer can be inside it whatever else is running, and a
    /// freshly created directory is empty by construction, which is the
    /// guarantee the removal on the way in was there for.
    ///
    /// The caller binds the `TempDir` for the length of the case; dropping it
    /// removes the tree, and it is the only thing here that removes anything.
    fn fixture_tree(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().expect("cannot create the fixture root");
        for (rel, body) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
        }
        dir
    }

    #[test]
    fn the_walk_sees_a_file_no_list_named() {
        // The defect the tree form exists to close: `b.rs` is in the tree and
        // in no hand-written list.
        let dir = fixture_tree(&[
            ("src/a.rs", "fn a() {}\n"),
            ("src/sub/b.rs", "fn b() { tokio::spawn(g()); }\n"),
        ]);
        let crate_dir = dir.path().to_string_lossy().into_owned();
        let caught =
            std::panic::catch_unwind(|| assert_no_ambient_seam_in_tree(&crate_dir, &["src"], &[]));
        let msg = panic_message(caught.expect_err("a walked-but-unlisted file must be caught"));
        assert!(
            msg.contains("src/sub/b.rs: `tokio::spawn(` in production scope"),
            "caught a different failure, so the walk is still unproven: {msg}"
        );
    }

    #[test]
    fn an_exempt_file_may_name_the_seam() {
        let dir = fixture_tree(&[
            ("src/a.rs", "fn a() {}\n"),
            ("src/sub/b.rs", "fn b() { tokio::spawn(g()); }\n"),
        ]);
        let crate_dir = dir.path().to_string_lossy().into_owned();
        assert_no_ambient_seam_in_tree(
            &crate_dir,
            &["src"],
            &[(
                "src/sub/b.rs",
                "long-lived loop started from the caller's runtime",
            )],
        );
    }

    #[test]
    fn a_stale_exemption_is_rejected() {
        // The list form's own failure mode, kept out of the tree form: a
        // waiver naming a file that is gone covers nothing.
        let dir = fixture_tree(&[("src/a.rs", "fn a() {}\n")]);
        let crate_dir = dir.path().to_string_lossy().into_owned();
        let caught = std::panic::catch_unwind(|| {
            assert_no_ambient_seam_in_tree(&crate_dir, &["src"], &[("src/gone.rs", "why")])
        });
        let msg =
            panic_message(caught.expect_err("an exemption for a missing file must be caught"));
        assert!(
            msg.contains("census exemptions name files that do not exist")
                && msg.contains("src/gone.rs"),
            "caught a different failure, so the staleness check is still unproven: {msg}"
        );
    }

    #[test]
    fn a_missing_root_is_rejected_rather_than_read_as_clean() {
        let dir = fixture_tree(&[("src/a.rs", "fn a() {}\n")]);
        let crate_dir = dir.path().to_string_lossy().into_owned();
        // The control that tells "this root moved" apart from "the fixture is
        // gone": both raise the same panic about the same root name, so only a
        // clean pass over a root that IS there proves which one fired below.
        assert_no_ambient_seam_in_tree(&crate_dir, &["src"], &[]);

        let caught = std::panic::catch_unwind(|| {
            assert_no_ambient_seam_in_tree(&crate_dir, &["moved"], &[])
        });
        let msg = panic_message(caught.expect_err("a root that moved must fail, not score zero"));
        assert!(
            msg.contains("census root `moved` is not a directory"),
            "caught a different failure, so the moved root is still unproven: {msg}"
        );
    }

    #[test]
    fn the_walk_ignores_non_rust_files_and_test_modules() {
        let dir = fixture_tree(&[
            (
                "src/a.rs",
                &format!("fn a() {{}}\n{CFG_TEST}\nmod t {{\n    tokio::spawn(g());\n}}\n"),
            ),
            ("src/notes.txt", "tokio::spawn("),
        ]);
        let crate_dir = dir.path().to_string_lossy().into_owned();
        assert_no_ambient_seam_in_tree(&crate_dir, &["src"], &[]);
    }

    #[test]
    fn every_path_spelling_of_the_seam_is_matched() {
        for path in [
            "crate::runtime::task::spawn(",
            "epics_base_rs::runtime::task::spawn(",
            "epics_libcom_rs::runtime::task::spawn(",
        ] {
            let src = format!("fn f() {{ {path}g()); }}\n");
            let caught = std::panic::catch_unwind(|| assert_no_ambient_seam(&[("f.rs", &src)]));
            assert!(caught.is_err(), "{path} must be caught");
        }
    }
}
