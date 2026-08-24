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
//! Each crate keeps a short `tests/record_seam_gate.rs` naming *its own*
//! sources; the rule, its message, and the slicer's own boundary tests live
//! here, once. Dev-only and `publish = false`: it reads source text and is
//! never linked into a shipped artefact.
//!
//! # The rule
//!
//! In the production scope of every listed source, none of [`BANNED`] may
//! appear. The seam is always named by path at a call site, so a textual
//! census is exact for it: `runtime::task::spawn(` matches whether the path is
//! spelled `crate::`, `epics_base_rs::` or `epics_libcom_rs::`, and does not
//! match `spawn_background(`, which is the whole point.
//!
//! A file that genuinely wants the ambient executor — a long-lived loop task
//! started from the caller's own runtime rather than from a framework callback
//! — does not belong in a crate's list at all. Leaving it out is a decision the
//! reader can check, which is why the lists carry the reason.

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

/// The file with its `#[cfg(test)]` modules removed — every column-0
/// `#[cfg(test)]` through the next column-0 `}`. Test code may name the ambient
/// seam freely: it runs under `#[tokio::test]`.
///
/// Column-0 only, and deliberately so. A nested `#[cfg(test)]` inside an `impl`
/// is not recognised, so its contents stay in scope and are still censused —
/// the conservative direction. The failure mode this must never have is the
/// opposite one: silently eating production code and reporting a vacuous zero.
pub fn production_scope(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_test = false;
    for line in src.lines() {
        if !in_test && line.starts_with("#[cfg(test)]") {
            in_test = true;
            continue;
        }
        if in_test {
            if line.starts_with('}') {
                in_test = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Assert that no listed source names the ambient seam in production scope.
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
        let prod = production_scope(src);
        // Cheap fail-closed check against a list that has gone stale — an
        // entry pointing at a file that was emptied or replaced by a
        // re-export scores zero for every needle and looks clean. The slicer
        // itself is covered by the fixtures below, not by a per-file heuristic.
        assert!(
            prod.contains("fn "),
            "{name}: production scope holds no function at all — this list \
             entry is stale, not clean"
        );
        for banned in BANNED {
            assert_eq!(
                prod.matches(banned).count(),
                0,
                "{name}: `{banned}` in production scope. Record processing is \
                 reached from threads with no tokio runtime, so a tail \
                 deferred here must use the `_background` counterpart. If this \
                 site is a caller's own await, or a long-lived loop the caller \
                 starts from its own runtime, take the file out of the list \
                 with that reason."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The attribute spellings are assembled with `concat!` so this file's own
    // fixtures are not themselves census hits when someone greps the tree.
    const CFG_TEST: &str = concat!("#[cfg", "(test)]");

    #[test]
    fn the_slicer_keeps_production_and_drops_a_column_zero_test_module() {
        let src = format!(
            "fn keep() {{}}\n{CFG_TEST}\nmod t {{\n    fn drop_me() {{}}\n}}\nfn keep2() {{}}\n"
        );
        let prod = production_scope(&src);
        assert!(prod.contains("fn keep()"));
        assert!(prod.contains("fn keep2()"));
        assert!(!prod.contains("drop_me"));
    }

    #[test]
    fn the_slicer_leaves_an_indented_test_attribute_in_scope() {
        // Conservative direction: not recognised, so still censused.
        let src = format!("impl A {{\n    {CFG_TEST}\n    fn inner() {{}}\n}}\n");
        assert!(production_scope(&src).contains("fn inner()"));
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
