//! The `profile.ci` rule for tests that read a reference tree.
//!
//! Not named `reference_*` and not in `reference_trees`: both are excluded from
//! `profile.ci`, so a guard there is never run on the runners it protects. This
//! one reads source only and needs no tree.
//!
//! A test binary that calls the resolver fails on a runner with no checkout.
//! Every such binary must therefore be excluded by `profile.ci`'s
//! `default-filter` (`binary(..)` / `package(..)`), or every test of it that
//! reaches the resolver must be `#[ignore]`d individually.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const READERS: [&str; 4] = [
    "reference_path(",
    "reference_root(",
    "try_reference_root(",
    "ReferenceTree",
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `.rs` files under `dir`, recursively.
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.map(|e| e.expect("dir entry").path()) {
        if e.is_dir() {
            rs_files(&e, out);
        } else if e.extension().is_some_and(|x| x == "rs") {
            out.push(e);
        }
    }
}

/// The terms `profile.ci`'s `default-filter` excludes: `binary(x)`,
/// `binary(/^prefix/)` and `package(x)`.
struct CiExclusions {
    binaries: BTreeSet<String>,
    prefixes: Vec<String>,
    packages: BTreeSet<String>,
}

impl CiExclusions {
    fn load() -> Self {
        let toml = std::fs::read_to_string(workspace().join(".config/nextest.toml"))
            .expect("read nextest.toml");
        let ci = &toml[toml.find("[profile.ci]").expect("profile.ci")..];
        let start = ci
            .find("default-filter = \"\"\"")
            .expect("ci default-filter");
        let body = &ci[start + 20..];
        let body = &body[..body.find("\"\"\"").expect("filter end")];
        let mut me = Self {
            binaries: BTreeSet::new(),
            prefixes: Vec::new(),
            packages: BTreeSet::new(),
        };
        for line in body.lines() {
            let line = line.trim();
            // `(binary(x) & test(..))` narrows rather than excludes the binary.
            if line.contains('&') {
                continue;
            }
            for term in line.split('|') {
                let term = term.trim().trim_start_matches(['(', '"']);
                if let Some(arg) = term
                    .strip_prefix("binary(")
                    .and_then(|t| t.strip_suffix(')'))
                {
                    match arg.strip_prefix("/^").and_then(|r| r.strip_suffix('/')) {
                        Some(p) => {
                            assert!(
                                p.chars().all(|c| c.is_alphanumeric() || c == '_'),
                                "unsupported binary regex {arg}"
                            );
                            me.prefixes.push(p.to_string());
                        }
                        None => {
                            me.binaries.insert(arg.to_string());
                        }
                    }
                } else if let Some(arg) = term
                    .strip_prefix("package(")
                    .and_then(|t| t.strip_suffix(')'))
                {
                    me.packages.insert(arg.to_string());
                }
            }
        }
        me
    }

    fn excludes(&self, package: &str, binary: &str) -> bool {
        self.packages.contains(package)
            || self.binaries.contains(binary)
            || self.prefixes.iter().any(|p| binary.starts_with(p.as_str()))
    }
}

/// One `fn`: its name, whether it is a test, whether `#[ignore]`d, its text.
struct Func {
    name: String,
    test: bool,
    ignored: bool,
    body: String,
}

fn fn_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    let t = t.strip_prefix("pub ").unwrap_or(t);
    let t = t.strip_prefix("async ").unwrap_or(t);
    let t = t.strip_prefix("fn ")?;
    Some(
        t.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect(),
    )
}

fn functions(src: &str) -> Vec<Func> {
    let lines: Vec<&str> = src.lines().collect();
    let starts: Vec<usize> = (0..lines.len())
        .filter(|&i| fn_name(lines[i]).is_some())
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let end = starts.get(k + 1).copied().unwrap_or(lines.len());
            let mut attrs = String::new();
            let mut j = i;
            while j > 0 {
                let p = lines[j - 1].trim_start();
                if p.starts_with("#[") || p.starts_with("//") {
                    attrs.push_str(p);
                    attrs.push('\n');
                    j -= 1;
                } else {
                    break;
                }
            }
            Func {
                name: fn_name(lines[i]).unwrap(),
                test: attrs.contains("#[test]") || attrs.contains("::test"),
                ignored: attrs.contains("#[ignore"),
                body: lines[i..end].join("\n"),
            }
        })
        .collect()
}

/// Names of tests in `src` that reach a resolver call (directly, or through
/// the helper functions of the same file) and are not `#[ignore]`d.
fn live_readers(src: &str) -> Vec<String> {
    let fns = functions(src);
    let mut reach: BTreeSet<String> = fns
        .iter()
        .filter(|f| READERS.iter().any(|r| f.body.contains(r)))
        .map(|f| f.name.clone())
        .collect();
    loop {
        let grown: Vec<String> = fns
            .iter()
            .filter(|f| !reach.contains(&f.name))
            .filter(|f| reach.iter().any(|n| mentions(&f.body, n)))
            .map(|f| f.name.clone())
            .collect();
        if grown.is_empty() {
            break;
        }
        reach.extend(grown);
    }
    fns.iter()
        .filter(|f| f.test && !f.ignored && reach.contains(&f.name))
        .map(|f| f.name.clone())
        .collect()
}

fn mentions(body: &str, name: &str) -> bool {
    body.match_indices(name).any(|(i, _)| {
        let before = body[..i].chars().next_back();
        let after = body[i + name.len()..].chars().next();
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        !word(before) && !word(after)
    })
}

/// Test binaries that compile `file` (a path under `tests/`): the file itself
/// when top-level, else the top-level files that pull its directory in with
/// `mod <dir>;` or `#[path = "<dir>/.."]` naming it.
fn binaries_of(tests_dir: &Path, file: &Path) -> Vec<String> {
    let rel = file.strip_prefix(tests_dir).unwrap();
    let stem = |p: &Path| p.file_stem().unwrap().to_string_lossy().into_owned();
    let Some(dir) = rel
        .components()
        .next()
        .filter(|_| rel.components().count() > 1)
    else {
        return vec![stem(file)];
    };
    let dir = dir.as_os_str().to_string_lossy().into_owned();
    let rel_s = rel.to_string_lossy().replace('\\', "/");
    let mut tops = Vec::new();
    for top in std::fs::read_dir(tests_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
    {
        if top.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&top).unwrap();
        let by_dir = src.lines().any(|l| {
            let l = l.trim();
            l == format!("mod {dir};") || l == format!("pub mod {dir};")
        });
        if by_dir || src.contains(&format!("\"{rel_s}\"")) {
            tops.push(stem(&top));
        }
    }
    tops
}

/// Every `(package, binary, file, live reader tests)` in the workspace.
fn violations(ex: &CiExclusions, roots: &[PathBuf]) -> Vec<String> {
    let mut bad = Vec::new();
    for root in roots {
        for krate in std::fs::read_dir(root)
            .expect("read root")
            .map(|e| e.unwrap().path())
        {
            let package = krate.file_name().unwrap().to_string_lossy().into_owned();
            let tests_dir = krate.join("tests");
            let mut files = Vec::new();
            rs_files(&tests_dir, &mut files);
            for file in files {
                // This file names the patterns it looks for.
                if file
                    .file_name()
                    .is_some_and(|n| n == "ci_runs_no_unexcluded_reference_reader.rs")
                {
                    continue;
                }
                let src = std::fs::read_to_string(&file).expect("read test file");
                let live = live_readers(&src);
                if live.is_empty() {
                    continue;
                }
                let bins = binaries_of(&tests_dir, &file);
                if bins.is_empty() {
                    continue;
                }
                for b in bins.iter().filter(|b| !ex.excludes(&package, b)) {
                    bad.push(format!("{package}::{b} ({}): {live:?}", file.display()));
                }
            }
        }
    }
    bad
}

#[test]
fn reference_consumers_are_excluded_from_profile_ci() {
    let ws = workspace();
    let bad = violations(
        &CiExclusions::load(),
        &[ws.join("crates"), ws.join("tools")],
    );
    assert!(
        bad.is_empty(),
        "these tests reach a reference tree, run in profile.ci, and are not #[ignore]d: {bad:?}"
    );
}

#[test]
fn the_scan_sees_per_test_ignore_helpers_and_subdirectories() {
    let src = "\
fn base() { reference_path(1) }
#[test]
#[ignore = \"x\"]
fn a() { base(); }
#[test]
fn b() { base(); }
#[test]
fn c() { let _ = 1; }
";
    assert_eq!(live_readers(src), vec!["b".to_string()]);
    let all_ignored = src.replace("fn b()", "#[ignore]\nfn b()");
    assert!(live_readers(&all_ignored).is_empty());
}
