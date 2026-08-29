//! A test that walks every entry of `RECORD_TYPES` must build its records
//! through `tests/module_records`, not through `db_loader::create_record`.
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

use std::path::Path;

#[test]
fn every_record_types_walker_opts_into_the_module_registry() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut walkers = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&tests_dir)
        .expect("the crate's tests directory")
        .map(|e| e.expect("a readable directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    entries.sort();

    for path in entries {
        let text = std::fs::read_to_string(&path).expect("a readable test file");
        if !text.contains("RECORD_TYPES") {
            continue;
        }
        walkers += 1;
        if !text.contains("mod module_records;") {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
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
        "these tests walk RECORD_TYPES without `mod module_records;`, so the \
         seven module-owned record types are unregistered in their process: \
         {offenders:?}"
    );
}
