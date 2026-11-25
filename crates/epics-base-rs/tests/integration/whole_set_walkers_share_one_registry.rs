//! A test that walks every entry of `RECORD_TYPES` must build its records
//! through `tests/integration/module_records`, not through `db_loader::create_record`.
//!
//! `RECORD_TYPES` has 37 entries; Base's default registry claims only the 30
//! that `stdRecords.dbd` declares. The other seven — acalcout, asyn, busy,
//! scalcout, sseq, swait, transform — are owned by the crates that vendor
//! them and are registered by the application that loads them, which is what
//! C does too: a `.dbd` a module ships is loaded by the IOC that wants it.
//! A whole-set walker is such an application, so it opts in through the
//! shared fixture.
//!
//! This is a gate and not a convention because the failure is silent. Of the
//! four walkers that existed when the seven types left the default registry,
//! only one panicked; the other three sat behind `let Ok(rec) = … else {
//! continue; }` or `.ok()?` and reported green over a set they had stopped
//! covering. A fifth walker written afterwards on a branch that still had the
//! old registry reintroduced the same failure at merge. Reviewing each new
//! walker cannot close that — the sweep is only true at the instant it is
//! run, and the next walker arrives on another branch.

use std::path::{Path, PathBuf};

#[test]
fn every_record_types_walker_opts_into_the_module_registry() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut walkers = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for path in test_sources(&tests_dir) {
        let text = std::fs::read_to_string(&path).expect("a readable test file");
        if !text.contains("RECORD_TYPES") {
            continue;
        }
        walkers += 1;
        if !text.contains("mod module_records;") && !text.contains("use crate::module_records") {
            let name = path.strip_prefix(&tests_dir).unwrap().display().to_string();
            offenders.push(name);
        }
    }

    assert!(
        walkers >= 8,
        "the scan found only {walkers} files mentioning RECORD_TYPES; it is \
         reading the wrong directory or the constant was renamed"
    );
    assert!(
        offenders.is_empty(),
        "these tests walk RECORD_TYPES without `mod module_records;` or `use \
         crate::module_records`, so the seven module-owned record types are \
         unregistered in their process: {offenders:?}"
    );
}

/// Every file that is a test target's root or one of its test modules: each
/// `tests/*.rs`, and each `tests/<dir>/*.rs` of a merged `tests/<dir>/main.rs`
/// target. Helper directories below either are not tests of their own.
fn test_sources(tests_dir: &Path) -> Vec<PathBuf> {
    let rs = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .expect("a readable tests directory")
            .map(|e| e.expect("a readable directory entry").path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "rs"))
            .collect()
    };
    let mut out = rs(tests_dir);
    for dir in std::fs::read_dir(tests_dir)
        .expect("the crate's tests directory")
        .map(|e| e.expect("a readable directory entry").path())
        .filter(|d| d.join("main.rs").is_file())
    {
        out.extend(rs(&dir));
    }
    out.sort();
    out
}
