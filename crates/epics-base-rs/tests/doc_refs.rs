//! Guards against doc-link rot.
//!
//! Several source and test files reference the parity-review documents by
//! path (e.g. `doc/parity-review/01-calc.md`) as provenance for the fixes
//! they cover. A doc move that forgets to update one of those references
//! leaves a dead path behind that nothing notices. This test scans this
//! crate's `src` and `tests` for such references and asserts each one
//! resolves to a real file relative to the crate root, so the move fails
//! here instead of rotting silently.
//!
//! See `docs/review-tagging-conventions.md`.

use std::fs;
use std::path::{Path, PathBuf};

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Extract every `…/parity-review/….md` path token from `text`.
///
/// A token ends at the matched `.md` and starts at the nearest preceding
/// delimiter (whitespace, backtick, quote, or opening paren) — the shapes the
/// references actually take in comments: `(doc/parity-review/01-calc.md)` and
/// `` `doc/parity-review/08-records-string.md` ``.
fn parity_review_refs(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for (idx, _) in text.match_indices(".md") {
        let end = idx + ".md".len();
        let start = text[..idx]
            .rfind(|c: char| c.is_whitespace() || matches!(c, '`' | '(' | '"' | '\''))
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &text[start..end];
        if token.contains("parity-review/") {
            refs.push(token.to_string());
        }
    }
    refs
}

#[test]
fn parity_review_doc_references_resolve() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut files = Vec::new();
    for sub in ["src", "tests"] {
        collect_rs(&crate_root.join(sub), &mut files);
    }

    // This test file documents the `parity-review/….md` pattern with
    // illustrative examples; skip it so its own prose is not scanned.
    let self_name = Path::new(file!()).file_name();

    let mut checked = 0usize;
    let mut missing = Vec::new();
    for file in &files {
        if file.file_name() == self_name {
            continue;
        }
        let text = fs::read_to_string(file).expect("read source file");
        for reference in parity_review_refs(&text) {
            checked += 1;
            // References are written relative to the crate root.
            if !crate_root.join(&reference).exists() {
                let rel = file.strip_prefix(crate_root).unwrap_or(file);
                missing.push(format!("{}: {reference}", rel.display()));
            }
        }
    }

    assert!(
        checked > 0,
        "scanned zero parity-review/*.md references — the scanner is broken, \
         not the references"
    );
    assert!(
        missing.is_empty(),
        "unresolved parity-review doc references (a doc moved without updating \
         the reference?):\n  {}",
        missing.join("\n  ")
    );
}
