//! The `profile.ci` rule for tests that read a reference tree.
//!
//! Not named `reference_*` and not in `reference_trees`: both are excluded from
//! `profile.ci`, so a guard there is never run on the runners it protects. This
//! one reads source only and needs no tree.
//!
//! A test binary that calls the resolver fails on a runner with no checkout.
//! Every such binary, or module of a merged `tests/<dir>/main.rs` binary, must
//! therefore be excluded by `profile.ci`'s `default-filter` (`binary(..)`,
//! `package(..)`, or `binary(B) & test(/^module/)`), or every test of it that
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

/// The roots of the workspace's `members = ["<root>/*", ..]`. Read rather
/// than listed: a hand-kept `crates` + `tools` list never scanned `examples/`,
/// and scope-ioc's `db_is_c_s` failed every CI run from there.
fn member_roots() -> Vec<PathBuf> {
    let toml =
        std::fs::read_to_string(workspace().join("Cargo.toml")).expect("read workspace Cargo.toml");
    let line = toml
        .lines()
        .find(|l| l.starts_with("members = ["))
        .expect("workspace members");
    line.split('"')
        .skip(1)
        .step_by(2)
        .map(|m| {
            let root = m
                .strip_suffix("/*")
                .unwrap_or_else(|| panic!("member {m:?} is not a `<root>/*` glob"));
            workspace().join(root)
        })
        .collect()
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
/// `binary(/^prefix/)`, `package(x)`, and `(binary(x) & test(/^prefix/))`,
/// which excludes every module of a merged binary whose name has the prefix.
struct CiExclusions {
    binaries: BTreeSet<String>,
    prefixes: Vec<String>,
    packages: BTreeSet<String>,
    units: Vec<(String, String)>,
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
        Self::parse(&body[..body.find("\"\"\"").expect("filter end")])
    }

    fn parse(body: &str) -> Self {
        let mut me = Self {
            binaries: BTreeSet::new(),
            prefixes: Vec::new(),
            packages: BTreeSet::new(),
            units: Vec::new(),
        };
        for line in body.lines() {
            let line = line.trim();
            // Any other `(binary(x) & test(..))` narrows to single tests and
            // excludes no whole binary or module.
            if line.contains('&') {
                if let Some(unit) = module_prefix_term(line) {
                    me.units.push(unit);
                }
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

    /// Whether every test of `module` in `binary` (the whole binary when
    /// `module` is `None`) is excluded.
    fn excludes(&self, package: &str, binary: &str, module: Option<&str>) -> bool {
        self.packages.contains(package)
            || self.binaries.contains(binary)
            || self.prefixes.iter().any(|p| binary.starts_with(p.as_str()))
            || module.is_some_and(|m| {
                let path = format!("{m}::");
                self.units
                    .iter()
                    .any(|(b, p)| b == binary && path.starts_with(p.as_str()))
            })
    }
}

/// `(binary(B) & test(/^P/))` as `(B, P)`.
fn module_prefix_term(line: &str) -> Option<(String, String)> {
    let t = line.trim_end_matches('|').trim();
    let t = t.strip_prefix('(')?.strip_suffix(')')?;
    let (b, p) = t.split_once(" & ")?;
    let b = b.strip_prefix("binary(")?.strip_suffix(')')?;
    let p = p.strip_prefix("test(/^")?.strip_suffix("/)")?;
    assert!(
        p.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':'),
        "unsupported test regex {p}"
    );
    Some((b.to_string(), p.to_string()))
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

/// The `(binary, module)` units that compile `file` (a path under `tests/`).
/// A top-level `tests/X.rs` is binary `X`. A merged `tests/D/main.rs` is one
/// binary `D`: `tests/D/X.rs` is its module `X`, and a file below `tests/D/`
/// belongs to the module `main.rs` declares over it. A subdirectory file is
/// also compiled by every top-level file that pulls its directory in with
/// `mod <dir>;` or names it in a `#[path]`.
fn units_of(tests_dir: &Path, file: &Path) -> Vec<(String, Option<String>)> {
    let rel = file.strip_prefix(tests_dir).unwrap();
    let stem = |p: &Path| p.file_stem().unwrap().to_string_lossy().into_owned();
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() == 1 {
        return vec![(stem(file), None)];
    }
    let dir = &parts[0];
    let mut units = Vec::new();
    let main = tests_dir.join(dir).join("main.rs");
    if main.is_file() {
        let inner = parts[1..].join("/");
        if inner == "main.rs" {
            units.push((dir.clone(), None));
        } else if parts.len() == 2 {
            units.push((dir.clone(), Some(stem(file))));
        } else {
            let src = std::fs::read_to_string(&main).unwrap();
            for (name, path) in module_decls(&src) {
                let owns = match path {
                    None => inner.starts_with(&format!("{name}/")),
                    Some(p) => {
                        p == inner
                            || p.strip_suffix("mod.rs")
                                .is_some_and(|d| inner.starts_with(d))
                    }
                };
                if owns {
                    units.push((dir.clone(), Some(name)));
                }
            }
        }
    }
    let rel_s = parts.join("/");
    for top in std::fs::read_dir(tests_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
    {
        if !top.is_file() || top.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&top).unwrap();
        let by_dir = src.lines().any(|l| {
            let l = l.trim();
            l == format!("mod {dir};") || l == format!("pub mod {dir};")
        });
        if by_dir || src.contains(&format!("\"{rel_s}\"")) {
            units.push((stem(&top), None));
        }
    }
    units
}

/// `mod name;` declarations of a merged `main.rs`, each with its `#[path]`.
fn module_decls(src: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut path = None;
    for line in src.lines() {
        let l = line.trim();
        if let Some(p) = l
            .strip_prefix("#[path = \"")
            .and_then(|p| p.strip_suffix("\"]"))
        {
            path = Some(p.to_string());
            continue;
        }
        if l.starts_with("#[") {
            continue;
        }
        if let Some(name) = l
            .strip_prefix("pub ")
            .unwrap_or(l)
            .strip_prefix("mod ")
            .and_then(|n| n.strip_suffix(';'))
        {
            out.push((name.to_string(), path.take()));
        } else {
            path = None;
        }
    }
    out
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
                for (b, m) in units_of(&tests_dir, &file)
                    .into_iter()
                    .filter(|(b, m)| !ex.excludes(&package, b, m.as_deref()))
                {
                    let unit = m.map_or(b.clone(), |m| format!("{b} & {m}::"));
                    bad.push(format!("{package}::{unit} ({}): {live:?}", file.display()));
                }
            }
        }
    }
    bad
}

#[test]
fn reference_consumers_are_excluded_from_profile_ci() {
    let bad = violations(&CiExclusions::load(), &member_roots());
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

#[test]
fn a_merged_module_is_excluded_by_its_name_not_its_binary() {
    let ex = CiExclusions::parse(
        "  binary(/^reference_/) |
  (binary(integration) & test(/^reference_/)) |
  (binary(other) & test(=reference_x::t))
",
    );
    assert!(ex.excludes("p", "integration", Some("reference_trees")));
    assert!(!ex.excludes("p", "integration", Some("trees_reference")));
    assert!(!ex.excludes("p", "integration", None));
    assert!(ex.excludes("p", "reference_trees", None));
    assert!(!ex.excludes("p", "other", Some("reference_x")));
}

#[test]
fn a_file_maps_to_the_units_that_compile_it() {
    let tmp = tempfile::tempdir().unwrap();
    let tests = tmp.path();
    for (rel, body) in [
        (
            "integration/main.rs",
            "mod helper;\n#[path = \"h2/probe.rs\"]\nmod h2_probe;\n\nmod a;\n",
        ),
        ("integration/a.rs", ""),
        ("integration/helper/mod.rs", ""),
        ("integration/h2/probe.rs", ""),
        (
            "kept.rs",
            "#[path = \"integration/helper/mod.rs\"]\nmod helper;\n",
        ),
    ] {
        let p = tests.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let units = |rel: &str| units_of(tests, &tests.join(rel));
    let unit = |b: &str, m: Option<&str>| (b.to_string(), m.map(str::to_string));
    assert_eq!(units("kept.rs"), [unit("kept", None)]);
    assert_eq!(units("integration/a.rs"), [unit("integration", Some("a"))]);
    assert_eq!(
        units("integration/helper/mod.rs"),
        [unit("integration", Some("helper")), unit("kept", None)]
    );
    assert_eq!(
        units("integration/h2/probe.rs"),
        [unit("integration", Some("h2_probe"))]
    );
    assert_eq!(units("integration/main.rs"), [unit("integration", None)]);
}
