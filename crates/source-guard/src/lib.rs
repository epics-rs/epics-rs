//! The one production-slice rule the workspace's mechanical guards share.
//!
//! A *source guard* is a `#[test]` that reads its own crate's source with
//! [`include_str!`] and asserts something about the code that ships: that no
//! thread is created outside its owner, that no timer is taken from `tokio`
//! directly on a path the RTEMS backend runs, that a blocking driver names no
//! async socket type. The assertion is only as good as the slice it runs on,
//! and every one of those guards had grown its own slicer.
//!
//! Fourteen call sites carried three different rules. Eleven truncated at the
//! first `\n#[cfg(test)]`, which makes the covered set a property of *where a
//! test item happens to be written*: a `#[cfg(test)]` helper placed high in a
//! file silently removes everything below it from the guard's view, and the
//! guard stays green while checking a fraction of its subject. Two walked
//! `#[cfg(test)]` items by brace balance and stripped comments inline; one
//! excluded by attribute and stripped comments by a different rule again.
//!
//! # What the rule is
//!
//! A file's production slice is the file with every item whose `cfg` predicate
//! **implies `test`** blanked out, wherever in the file it sits and at whatever
//! depth — a `#[cfg(test)] mod tests` nested inside `mod ioc` is test code
//! exactly as a top-level one is.
//! "Implies test" is decided by reading the predicate, not by matching the
//! string: `#[cfg(all(test, unix))]` is test-only, `#[cfg(not(test))]` is not,
//! and `#[cfg(any(target_os = "rtems", test))]` is not — those seven items in
//! `epics-libcom-rs`'s task seam ship on RTEMS, and a rule that matched the
//! substring `test` would have quietly dropped them from the census that
//! exists to cover them.
//!
//! Excluded lines are **blanked, not deleted**, so a 1-based line number taken
//! from the slice still names the same line of the original file. Guards that
//! report offending line numbers (`epics-base-rs`'s thread census) were
//! relying on truncation for that; blanking keeps the property under a rule
//! that removes items from the middle.
//!
//! # `include_str!` and where this crate lives
//!
//! [`include_str!`] cannot cross a crate boundary, so each guard keeps its own
//! `include_str!` and passes the text here — this crate never names a file of
//! its own. It is `publish = false` and is taken as a `path`-only
//! dev-dependency, which `cargo package` drops from the uploaded manifest, so
//! no published crate gains a dependency that is not on crates.io. The
//! consequence, and it is the intended one: the tests inside a published
//! `.crate` do not compile against a registry checkout. They are guards over
//! this workspace's source, and this workspace is where they run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Slices already computed, keyed by (source address, length, policy).
type SliceCache = OnceLock<Mutex<HashMap<(usize, usize, Comments), &'static str>>>;

/// Directories already walked, keyed by path.
type SourceCache = OnceLock<Mutex<HashMap<PathBuf, Vec<(&'static str, &'static str)>>>>;

/// Whether [`production`] also removes comments.
///
/// Guards forbid *code* from naming something, and the prose next to that code
/// names it constantly — explaining why a shape is banned is the point of the
/// comment. A guard matching raw source punishes documentation: the
/// `epics-libcom-rs` `try_clone` guard once failed on five prose hits and zero
/// code hits.
///
/// [`Comments::Strip`] removes line comments, block comments (nested, as Rust
/// allows) and trailing comments, and it reads string and character literals
/// so that `"https://x"` keeps its text and `&'a str` keeps its lifetime. That
/// is what lets it be one rule instead of the two that were in the tree: a
/// whole-line rule that missed trailing comments, and a truncate-at-`//` rule
/// that could not tell a comment from a URL inside a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Comments {
    /// Leave comments in the slice.
    Keep,
    /// Remove every comment, keeping line structure.
    Strip,
}

/// The production slice of one Rust source file, as owned text.
///
/// See the module docs for the rule. Line count is preserved, so
/// `slice.lines().nth(n)` is line `n` of `src`. Guards over `include_str!`
/// source want [`production`] instead; this is the rule itself, for text that
/// is not already `'static`.
pub fn production_str(src: &str, comments: Comments) -> String {
    let stripped;
    let src = match comments {
        Comments::Keep => src,
        Comments::Strip => {
            stripped = strip_comments(src);
            &stripped
        }
    };
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    while i < lines.len() {
        match test_only_item_at(&lines, i) {
            Some(past) => {
                // An item's documentation is part of the item. Left in, a
                // test helper's doc comment is production text under
                // `Comments::Keep`, and it is the text most likely to quote
                // the very needle the guard forbids — `tcp.rs` documents four
                // test-only helpers with prose naming `#[cfg(test)]` and
                // `tokio`, eleven lines of it, directly above them.
                for k in (0..out.len()).rev() {
                    let prev = out[k].trim_start();
                    if prev.starts_with("//!")
                        || !(prev.starts_with("//") || prev.starts_with("#["))
                    {
                        break;
                    }
                    out[k] = "";
                }
                out.extend(std::iter::repeat_n("", past - i));
                i = past;
            }
            None => {
                out.push(lines[i]);
                i += 1;
            }
        }
    }
    out.join("\n")
}

/// The production slice of a file that came from [`include_str!`].
///
/// Guards bind a slice once and then borrow substrings out of it across
/// several statements — the offending line, the body after an anchor — so a
/// freshly allocated `String` per call would have to be bound at every site
/// before it could be used. The input is already `'static`, so the answer can
/// be too. Slices are memoised per (file, policy): a guard that asks four
/// times for the same file computes it once, and a test binary that ends after
/// a few files holds a few slices.
pub fn production(src: &'static str, comments: Comments) -> &'static str {
    static SLICES: SliceCache = OnceLock::new();
    let key = (src.as_ptr() as usize, src.len(), comments);
    let mut slices = SLICES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("source-guard slice cache");
    slices
        .entry(key)
        .or_insert_with(|| String::leak(production_str(src, comments)))
}

/// The index just past the column-0 test-only item beginning at `i`, or `None`
/// if no such item begins there.
///
/// An item this cannot close is left in the slice, so an unparsable shape
/// raises a false alarm in the guard rather than silencing it.
fn test_only_item_at(lines: &[&str], i: usize) -> Option<usize> {
    if !cfg_is_test_only(lines[i].trim_start()) {
        return None;
    }
    // The item's own indentation. The rule is the same at every depth: a
    // `#[cfg(test)] mod tests` nested inside `mod ioc` is test code exactly as
    // a top-level one is, and treating only column 0 made the boundary a
    // special case that guards then worked around by hand — two in
    // `realtime-pva-ioc.rs` truncated at `"\n    #[cfg(test)]"` to reach the
    // nested module the column-0 spelling could not see.
    let indent = &lines[i][..lines[i].len() - lines[i].trim_start().len()];
    // Further attributes may sit between the `cfg` and the item it applies to.
    let mut item = i + 1;
    while lines.get(item)?.trim_start().starts_with('#') {
        item += 1;
    }
    let head = lines.get(item)?;
    let opens = head.matches('{').count();
    let closes = head.matches('}').count();
    if opens > 0 && opens == closes {
        // `#[cfg(test)] use foo::{a, b};`, or an empty `mod tests {}`.
        return Some(item + 1);
    }
    let tail = head.trim_end();
    if opens == 0 && (tail.ends_with(';') || tail.ends_with(',')) {
        // A `use`, a `let`, a struct field or an enum variant: one line.
        return Some(item + 1);
    }
    // Anything else runs to the closing brace at its own indentation, which is
    // where `rustfmt` puts it and `cargo fmt --check` keeps it. Matching the
    // whole line rather than counting braces is what makes a `"{"` inside a
    // test fixture harmless.
    let close_marker = format!("{indent}}}");
    let close = (item..lines.len()).find(|&k| lines[k] == close_marker)?;
    Some(close + 1)
}

/// Does this line carry a column-0 `cfg` attribute whose predicate implies
/// `test`?
fn cfg_is_test_only(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#[cfg(") else {
        return false;
    };
    let Some(pred) = rest.strip_suffix(")]") else {
        return false;
    };
    pred_is_test_only(pred)
}

/// `cfg` predicate implication, read rather than matched.
///
/// `all(..)` implies `test` when any argument does; `any(..)` only when every
/// argument does. `not(..)` never does — `#[cfg(not(test))]` is code that
/// ships. Anything else is a plain option and does not.
fn pred_is_test_only(pred: &str) -> bool {
    let pred = pred.trim();
    if pred == "test" {
        return true;
    }
    if let Some(args) = strip_call(pred, "all") {
        return split_top(args).iter().any(|a| pred_is_test_only(a));
    }
    if let Some(args) = strip_call(pred, "any") {
        let parts = split_top(args);
        return !parts.is_empty() && parts.iter().all(|a| pred_is_test_only(a));
    }
    false
}

fn strip_call<'a>(pred: &'a str, name: &str) -> Option<&'a str> {
    pred.strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// Split on commas at paren depth zero, ignoring commas inside string
/// literals.
fn split_top(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut start = 0usize;
    for (i, c) in args.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth = depth.saturating_sub(1),
            ',' if !in_str && depth == 0 => {
                out.push(args[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Remove every comment, keeping line structure and literal text intact.
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        // A raw string opener has to be recognised before its `r`/`b` is taken
        // for an identifier character, because `\` is not an escape inside it.
        if let Some((hashes, quote)) = raw_string_at(b, i) {
            let end = raw_string_end(b, quote + 1, hashes);
            out.extend_from_slice(&b[i..end]);
            i = end;
            continue;
        }
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        if b[i] == b'\n' {
                            out.push(b'\n');
                        }
                        i += 1;
                    }
                }
            }
            b'"' => {
                let end = string_end(b, i + 1);
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            b'\'' => {
                let end = quote_end(b, i);
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Only ever split at ASCII delimiters, so the bytes are still valid UTF-8.
    String::from_utf8(out).expect("comment stripping splits only at ASCII")
}

/// `(hash count, index of the opening quote)` if a raw string starts at `i`.
fn raw_string_at(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    let mut j = i;
    if b.get(j) == Some(&b'b') {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes_at = j;
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    if b.get(j) == Some(&b'"') {
        Some((j - hashes_at, j))
    } else {
        None
    }
}

fn raw_string_end(b: &[u8], mut i: usize, hashes: usize) -> usize {
    while i < b.len() {
        if b[i] == b'"' && b[i + 1..].iter().take(hashes).all(|&c| c == b'#') {
            return (i + 1 + hashes).min(b.len());
        }
        i += 1;
    }
    b.len()
}

/// Index just past the closing quote of a string literal opened before `i`.
fn string_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// Index just past a `'`-introduced token: a character literal, or the `'` of
/// a lifetime, which is emitted alone so the name after it stays code.
fn quote_end(b: &[u8], i: usize) -> usize {
    if b.get(i + 1) == Some(&b'\\') {
        let mut j = i + 2;
        while j < b.len() && b[j] != b'\'' {
            j += 1;
        }
        return (j + 1).min(b.len());
    }
    // One character, then a closing quote — otherwise it is a lifetime.
    let len = char_len(b, i + 1);
    if b.get(i + 1 + len) == Some(&b'\'') {
        i + 2 + len
    } else {
        i + 1
    }
}

fn char_len(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        None => 0,
        Some(&c) if c < 0x80 => 1,
        Some(&c) if c >> 5 == 0b110 => 2,
        Some(&c) if c >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// The path of `rel` inside the calling crate, for [`sweep`].
#[macro_export]
macro_rules! module_dir {
    ($rel:expr) => {
        ::std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join($rel)
    };
}

/// Every `.rs` file under `dir`, recursively, as (path relative to `dir`,
/// contents), sorted by path.
///
/// Both halves are `'static` so a derived set drops straight into a guard that
/// used to carry `[(&str, &str); N]` by hand. Directories are read once per
/// test binary.
///
/// # Panics
///
/// If `dir` holds no `.rs` file at all. The private `collect` recursion already
/// panics on a directory it cannot read, but a directory that exists and is
/// *empty* — a module whose files moved up a level, leaving the directory
/// behind — returned an empty set, and a guard that iterates an empty set
/// passes without checking anything. That is the same failure as an unreadable directory wearing a
/// different hat, so it fails the same way.
pub fn rust_sources(dir: impl AsRef<Path>) -> Vec<(&'static str, &'static str)> {
    static READ: SourceCache = OnceLock::new();
    let dir = dir.as_ref();
    let mut cache = READ
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("source-guard directory cache");
    let found = cache
        .entry(dir.to_path_buf())
        .or_insert_with(|| {
            let mut out = Vec::new();
            collect(dir, dir, &mut out);
            out.sort_by_key(|(label, _)| *label);
            out
        })
        .clone();
    assert!(
        !found.is_empty(),
        "source-guard: {} holds no .rs file; a guard sweeping it would check \
         nothing and report green",
        dir.display()
    );
    found
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(&'static str, &'static str)>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("source-guard: cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path: PathBuf = entry
            .unwrap_or_else(|e| panic!("source-guard: cannot read {}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let label = path
                .strip_prefix(root)
                .expect("walked below root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("source-guard: cannot read {}: {e}", path.display()));
            out.push((String::leak(label), String::leak(text)));
        }
    }
}

/// The files a guard must sweep: everything under `dir` except the exemptions.
///
/// This is the point of the crate's second half. A guard that carries its
/// subjects as a hand-written list is default-out — a file added to the module
/// is invisible to it, and nothing says so. `epics-pva-rs`'s two
/// `client_native` guards asked one question with two lists that disagreed,
/// between them naming 7 of 11 files, and two live seam violations sat in the
/// gap. Deriving the set makes it default-in: a new file is swept on the
/// commit that adds it, and a file that genuinely cannot be swept has to be
/// named here with the reason beside it.
///
/// # Panics
///
/// If an exemption names no file under `dir` — an exemption list goes stale
/// the same way a subject list does, and a stale one silently removes nothing
/// while reading as though it removes something.
///
/// If nothing survives the exemptions. The exemption list is a narrowing of
/// the swept set, so it needs a floor on its own axis: a list that has grown
/// to cover the whole module leaves the guard iterating an empty `Vec` and
/// passing, which is the same green-over-nothing the derived set exists to
/// prevent. [`rust_sources`] refuses an empty directory for the same reason.
pub fn sweep(dir: impl AsRef<Path>, exempt: &[&str]) -> Vec<(&'static str, &'static str)> {
    let dir = dir.as_ref();
    let all = rust_sources(dir);
    for name in exempt {
        assert!(
            all.iter().any(|(label, _)| label == name),
            "source-guard: exemption `{name}` names no file under {}. \
             Present: {:?}",
            dir.display(),
            all.iter().map(|(l, _)| *l).collect::<Vec<_>>()
        );
    }
    let swept: Vec<_> = all
        .into_iter()
        .filter(|(label, _)| !exempt.contains(label))
        .collect();
    assert!(
        !swept.is_empty(),
        "source-guard: every .rs file under {} is exempted; the guard would \
         check nothing and report green",
        dir.display()
    );
    swept
}
