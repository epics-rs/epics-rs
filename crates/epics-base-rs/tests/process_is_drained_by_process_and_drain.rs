//! Invariant: **a cycle path never calls `Record::process` on an instance
//! except through `RecordInstance::process_and_drain`.**
//!
//! `process()` marks the fields it posts; only the drain after it turns the
//! marks into posts. A caller that processed without draining left a
//! `PROCESS_POSTED` type's marks to the next cycle (SIMM=RAW in
//! `simulate_or_resolve` did). The helper is the one place both happen.

use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The file's non-test text: `#[cfg(test)]` items run to the end of the file
/// in this crate, and `*_tests.rs` / `tests.rs` files are test modules.
fn non_test_text(path: &Path) -> Option<String> {
    let name = path.file_name().unwrap().to_str().unwrap();
    if name == "tests.rs" || name.ends_with("_tests.rs") {
        return None;
    }
    let text = std::fs::read_to_string(path).unwrap();
    let end = text.find("\n#[cfg(test)]").unwrap_or(text.len());
    Some(text[..end].to_string())
}

#[test]
fn no_cycle_path_processes_without_the_drain() {
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut bypasses = Vec::new();
    for path in files {
        let Some(text) = non_test_text(&path) else {
            continue;
        };
        // Whitespace-free, so `inst.record\n    .process()` is one match.
        let squeezed: String = text.split_whitespace().collect();
        for needle in [".record.process()", "record_mut().process()"] {
            for (at, _) in squeezed.match_indices(needle) {
                let before = &squeezed[at.saturating_sub(40)..at];
                bypasses.push(format!("{}: ..{before}{needle}", path.display()));
            }
        }
    }
    // The helper's own call is the one allowed site.
    assert_eq!(
        bypasses.len(),
        1,
        "Record::process outside RecordInstance::process_and_drain: {bypasses:#?}"
    );
    assert!(
        bypasses[0].contains("record_instance.rs"),
        "the one call is not in the helper: {bypasses:#?}"
    );
}
