//! Invariant: **a cycle path never calls `Record::process` on an instance
//! except through `RecordInstance::process_and_drain`.**
//!
//! `process()` marks the fields it posts; only the drain after it turns the
//! marks into posts. A caller that processed without draining left a
//! `PROCESS_POSTED` type's marks to the next cycle (SIMM=RAW in
//! `simulate_or_resolve` did). The helper is the one place both happen.

use std::path::{Path, PathBuf};

/// Every non-test source file of every workspace `src` directory, as
/// (path, production text): `source_guard::production_str` is the one rule
/// for what a `#[cfg(test)]` item is, comments stripped. `*_tests.rs`,
/// `tests.rs` and anything under a `tests` directory are test modules and are
/// not scanned.
fn production_sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in workspace_src_dirs() {
        for (label, text) in source_guard::rust_sources(&dir) {
            let name = label.rsplit('/').next().unwrap();
            if name == "tests.rs"
                || name.ends_with("_tests.rs")
                || label.split('/').any(|c| c == "tests")
            {
                continue;
            }
            out.push((
                format!("{}/{label}", dir.display()),
                source_guard::production_str(text, source_guard::Comments::Strip),
            ));
        }
    }
    out
}

/// Every `src` directory of every workspace member (`crates/*`, `examples/*`,
/// `tools/*`), not only this crate's.
fn workspace_src_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut dirs = Vec::new();
    for group in ["crates", "examples", "tools"] {
        for member in std::fs::read_dir(root.join(group)).unwrap() {
            let src = member.unwrap().path().join("src");
            if src.is_dir() {
                dirs.push(src);
            }
        }
    }
    dirs
}

/// The body of every `fn <name>(` in `text` — signature to the closing brace
/// at the signature's indentation — and the text with those lines blanked. A
/// declaration ending in `;` has no body and is left alone.
fn split_out_fns(text: &str, name: &str) -> (Vec<String>, String) {
    let lines: Vec<&str> = text.lines().collect();
    let mut bodies = Vec::new();
    let mut rest = Vec::new();
    let mut k = 0;
    while k < lines.len() {
        if lines[k].trim_start().contains(&format!("fn {name}(")) {
            let indent = &lines[k][..lines[k].len() - lines[k].trim_start().len()];
            let head_end = (k..lines.len()).find(|&j| {
                let t = lines[j].trim_end();
                t.ends_with('{') || t.ends_with(';')
            });
            if let Some(h) = head_end.filter(|&h| lines[h].trim_end().ends_with('{')) {
                let close_marker = format!("{indent}}}");
                if let Some(close) = (h..lines.len()).find(|&j| lines[j] == close_marker) {
                    bodies.push(lines[k..=close].join("\n"));
                    k = close + 1;
                    continue;
                }
            }
        }
        rest.push(lines[k]);
        k += 1;
    }
    (bodies, rest.join("\n"))
}

/// Whitespace-free, so `inst.record\n    .process()` is one match.
fn hits(text: &str, needles: &[&str]) -> Vec<String> {
    let squeezed: String = text.split_whitespace().collect();
    let mut out = Vec::new();
    for needle in needles {
        for (at, _) in squeezed.match_indices(needle) {
            let before = &squeezed[at.saturating_sub(40)..at];
            out.push(format!("..{before}{needle}"));
        }
    }
    out
}

/// `RecordInstance` is the one owner of processing a record:
///
/// * `Record::process` is called only from `process_record`, and that only
///   from `process_and_drain` and `process_hooked`;
/// * a record's `apply_invalid_output_value` hook may call its own
///   `self.process()` (ao's IVOA=Set_output_to_IVOV does), and that is the one
///   place a record does. The hook is called only from
///   `apply_invalid_output_value_and_drain`, so its process is drained there
///   by construction rather than by a list of records that happen to be safe.
///
/// Each allowance is the body of a function, so a second call elsewhere in the
/// same file is still a bypass.
#[test]
fn no_cycle_path_processes_without_the_drain() {
    const PROCESS: &[&str] = &[".record.process()", "record_mut().process()"];
    const HOOK_CALL: &[&str] = &[".record.apply_invalid_output_value("];
    const SELF_PROCESS: &[&str] = &["self.process()"];
    const CALLER: &[&str] = &["self.process_record()"];

    let mut bypasses = Vec::new();
    let mut owner_process = Vec::new();
    let mut callers = Vec::new();
    let mut owner_hook = Vec::new();
    for (path, text) in production_sources() {
        let tag = |h: String| format!("{path}: {h}");
        let mut rest = text;
        // Only `RecordInstance`'s own: a same-named function elsewhere is not
        // the owner and would blank a bypass.
        if path.ends_with("src/server/record/record_instance.rs") {
            let (bodies, r) = split_out_fns(&rest, "process_record");
            owner_process.extend(bodies.iter().flat_map(|b| hits(b, PROCESS)).map(tag));
            rest = r;
            for caller in ["process_and_drain", "process_hooked"] {
                let (bodies, r) = split_out_fns(&rest, caller);
                callers.extend(bodies.iter().flat_map(|b| hits(b, CALLER)).map(tag));
                rest = r;
            }
            bypasses.extend(hits(&rest, CALLER).into_iter().map(tag));
            let (bodies, r) = split_out_fns(&rest, "apply_invalid_output_value_and_drain");
            owner_hook.extend(bodies.iter().flat_map(|b| hits(b, HOOK_CALL)).map(tag));
            rest = r;
        }
        // The hook's own body: the one place a record calls `self.process()`.
        let (_, rest) = split_out_fns(&rest, "apply_invalid_output_value");
        bypasses.extend(
            hits(&rest, &[PROCESS, HOOK_CALL, SELF_PROCESS].concat())
                .into_iter()
                .map(tag),
        );
    }
    assert_eq!(
        owner_process.len(),
        1,
        "RecordInstance::process_record must call Record::process exactly \
         once: {owner_process:#?}"
    );
    assert_eq!(
        callers.len(),
        2,
        "process_and_drain and process_hooked must each call process_record \
         once: {callers:#?}"
    );
    assert_eq!(
        owner_hook.len(),
        1,
        "RecordInstance::apply_invalid_output_value_and_drain must call the hook \
         exactly once: {owner_hook:#?}"
    );
    assert!(
        bypasses.is_empty(),
        "processing a record outside RecordInstance's drained entries: {bypasses:#?}"
    );
}
