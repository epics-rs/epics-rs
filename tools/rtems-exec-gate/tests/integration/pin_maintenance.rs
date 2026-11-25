//! Filling `SCOPE_GATED` in at a merge, by boundary rather than by story.
//!
//! The entry that LEAVES the pin is the one that has to be read carefully, and
//! there are four ways for it to leave. Exactly one of them — the gate being
//! removed — puts reactor-dependent sites back into the census with nobody
//! accounting for them; the other three are a file that stopped needing to be
//! named, and dropping it loses no coverage. A merge review that cannot tell
//! them apart either re-litigates every removal or waves all of them through,
//! so each has a case here.

use rtems_exec_gate::{Dropped, scope_gated_literal, why_dropped};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn tokio_test() -> &'static str {
    concat!("#[tokio", "::test]\nasync fn t() {}\n")
}

fn gate() -> &'static str {
    rtems_exec_gate::FILE_GATE_ATTR
}

fn crate_at(name: &str, files: &[(&str, String)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    for (rel, text) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        fs::write(&path, text).expect("write fixture");
    }
    dir
}

#[test]
fn a_file_that_left_the_tree_is_gone() {
    let dir = crate_at(
        "pin_gone",
        &[("src/lib.rs", format!("{}\n{}", gate(), tokio_test()))],
    );
    assert_eq!(why_dropped(&dir, "src/vanished.rs"), Dropped::Gone);
}

#[test]
fn a_gate_that_lost_its_last_site_still_gates() {
    // The gate is untouched; what changed is that its last reactor-dependent
    // site did. Nothing returned to the census, so the entry leaves the pin and
    // the two reactor-free tests it still removes are the cost report's business.
    let dir = crate_at(
        "pin_still_gated",
        &[(
            "src/lib.rs",
            format!("{}\n#[test]\nfn a() {{}}\n#[test]\nfn b() {{}}\n", gate()),
        )],
    );
    assert_eq!(
        why_dropped(&dir, "src/lib.rs"),
        Dropped::StillGated { collateral: 2 }
    );
}

#[test]
fn a_file_a_parent_now_removes_whole_did_not_return_its_sites() {
    let dir = crate_at(
        "pin_mod_gated",
        &[
            (
                "src/lib.rs",
                format!("{}\npub mod inner;\n", rtems_exec_gate::GATE_ATTR),
            ),
            ("src/inner.rs", tokio_test().to_owned()),
        ],
    );
    assert_eq!(
        why_dropped(&dir, "src/inner.rs"),
        Dropped::ModGated {
            declared_in: "src/lib.rs".to_owned(),
            line: 2,
        }
    );
}

#[test]
fn a_gate_that_is_simply_gone_hands_its_sites_back() {
    // The one that is a real change: the file still holds the site, nothing
    // removes it any more, and it now needs a census marker.
    let dir = crate_at(
        "pin_gate_lost",
        &[("src/lib.rs", format!("{}\n{}", tokio_test(), tokio_test()))],
    );
    assert_eq!(
        why_dropped(&dir, "src/lib.rs"),
        Dropped::GateLost { sites: 2 }
    );
}

#[test]
fn the_printed_constant_is_the_shape_the_pin_is_written_in() {
    let mut set = BTreeSet::new();
    set.insert(("epics-ca-rs".to_owned(), "src/server/tcp.rs".to_owned()));
    set.insert(("epics-pva-rs".to_owned(), "tests/tls.rs".to_owned()));
    assert_eq!(
        scope_gated_literal(&set),
        "const SCOPE_GATED: &[(&str, &str)] = &[\n    \
         (\"epics-ca-rs\", \"src/server/tcp.rs\"),\n    \
         (\"epics-pva-rs\", \"tests/tls.rs\"),\n];\n"
    );
}

/// The printed text is pasted into the pin and then met by `cargo fmt --all`,
/// which is mandatory here, so the printer reproduces rustfmt's wrapping rather
/// than leaving the wrapping to it. A reproduced rule is a guess until the real
/// formatter agrees with it, and it can only be wrong at the width where the
/// wrapping turns on — so the entries here straddle that width rather than
/// sampling around it. If rustfmt's own default moves, this fails here instead
/// of at a merge.
#[test]
fn the_printed_constant_is_what_rustfmt_would_write() {
    let mut set = BTreeSet::new();
    for width in 58..=66 {
        // `("cc", "xx…")` is exactly `width` characters wide.
        set.insert(("cc".to_owned(), "x".repeat(width - 10)));
    }
    let printed = scope_gated_literal(&set);
    assert_eq!(
        rustfmt(&printed),
        printed,
        "the printed constant is not a rustfmt fixed point, so pasting it \
         leaves the tree failing `cargo fmt --check` on a current value"
    );
}

/// Pipe a snippet through the toolchain's `rustfmt`, the way `env-codegen` and
/// `dbd-codegen` do. Absence is a failure and not a skip: every commit here
/// runs `cargo fmt --all`, so a machine without the formatter cannot answer the
/// question this test asks, and a skip would leave it answering nothing.
fn rustfmt(src: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("`rustfmt` on PATH; `cargo fmt --all` is a gate on every commit here");
    child
        .stdin
        .take()
        .expect("rustfmt stdin")
        .write_all(src.as_bytes())
        .expect("write to rustfmt");
    let out = child.wait_with_output().expect("rustfmt runs");
    assert!(out.status.success(), "rustfmt failed: {}", out.status);
    String::from_utf8(out.stdout).expect("rustfmt emits UTF-8")
}

#[test]
fn an_empty_measurement_still_prints_a_constant_that_compiles() {
    // A tree that gates nothing is a legitimate reading — and a printer that
    // emits nothing at all there would be pasted over the constant as a
    // syntax error at exactly the moment nobody is looking closely.
    assert_eq!(
        scope_gated_literal(&BTreeSet::new()),
        "const SCOPE_GATED: &[(&str, &str)] = &[\n];\n"
    );
}
