//! Which nextest binaries spawn an EPICS IOC, derived from the sources.
//!
//! # The rule this crate enforces
//!
//! A test binary whose sources spawn an EPICS IOC process — the C `softIoc` /
//! `softIocPVX`, or one of our own IOC binaries — MUST be a member of a
//! declared nextest test-group, so that its scheduling is a decision somebody
//! made in `.config/nextest.toml` rather than a default nobody noticed.
//!
//! Membership is *derived from the sources*, never remembered. The four binary
//! names `ca_softioc` used to carry were maintained by whoever thought of it:
//! `epics-oracle-rs` spawns three kinds of IOC across eighty-odd tests and was
//! never in it, and neither were `interop_pvxs` or the six
//! `epics-ca-rs` / `epics-bridge-rs` / `qsrv-ioc` binaries that spawn
//! `softioc-rs` and friends. Nothing went red when they joined — they just
//! occasionally timed out under load, which reads as flakiness rather than as
//! a missing declaration. This gate turns that into a red test at the moment
//! the binary joins.
//!
//! # Why a group and not one cap
//!
//! "Spawns an IOC" and "needs serialising" are different claims, and only the
//! first can be derived from the source. Forcing every spawner into
//! `max-threads = 1` was measured on `epics-oracle-rs` and is worse, not
//! better; the numbers are recorded beside the filters in
//! `.config/nextest.toml`. So the gate insists that somebody *decided*, in the
//! file that owns test scheduling — `ca_softioc` (serialised) or
//! `ioc_unthrottled` (measured to need no cap) — not that the cap is applied.
//!
//! # Order matters
//!
//! nextest resolves per-test overrides first-match-wins, so a declaration in a
//! later block is not the effective one. The gate models that, and reports a
//! block it cannot evaluate only when that block could shadow a declaration —
//! `package(ad-plugins-rs) and test(file_magick::)` is unevaluable but can
//! never claim a binary outside `ad-plugins-rs`, so it shadows nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Printed at the top of every failure so the reader does not have to find
/// this file to learn what was breached.
pub const RULE: &str = "\
Every nextest binary whose sources spawn an EPICS IOC must be a member of a \
declared test-group in .config/nextest.toml -- `ca_softioc` when it needs \
serialising, `ioc_unthrottled` when it has been measured not to. Membership is \
derived from the sources by tools/ioc-spawn-gate, so a new IOC-spawning binary \
is a red test until somebody declares it, instead of a 17-second timeout under \
load six months later. Run `cargo run -p ioc-spawn-gate --bin ioc-spawn-census` \
to see what the sources say.";

// ---------------------------------------------------------------------------
// Spawn detection
// ---------------------------------------------------------------------------

/// The IOC executables a test may spawn.
///
/// Two of them are upstream and fixed: EPICS base ships `softIoc` and pvxs
/// ships `softIocPVX`, and there is no third. The rest are ours, and they are
/// *derived* from the workspace's own bin targets rather than listed here —
/// any `[[bin]]` whose name contains `ioc` counts. That is the half that would
/// otherwise rot: a new `foo-ioc` binary joins the token set the moment its
/// target exists, without anybody remembering this file. A bin whose root
/// source builds an `IocApplication` counts whatever its name: `qsrv-rs` boots
/// an IOC and has no `ioc` in it.
pub fn ioc_program_tokens(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert("softioc".to_string());
    for dir in workspace_crates(root) {
        for bin in bin_targets(&dir) {
            let lower = bin.name.to_ascii_lowercase();
            if lower.ends_with("ioc")
                || lower.ends_with("ioc-rs")
                || lower.ends_with("ioc_rs")
                || builds_an_ioc_application(&read(&dir.join(&bin.path)))
            {
                out.insert(lower);
            }
        }
    }
    out
}

fn builds_an_ioc_application(text: &str) -> bool {
    let mut in_block = false;
    text.lines()
        .any(|raw| code_only(raw, &mut in_block).contains("IocApplication"))
}

/// One bin target, and the source cargo compiles for it.
///
/// A bin target is a nextest binary in its own right — its `#[test]`s run as
/// `package::bin/<name>`, which `binary(<name>)` selects — so it is not part
/// of the library's unit-test binary and must never be attributed to it.
///
/// The path is carried because `[[bin]] path` may point anywhere and 48 of
/// this workspace's bin blocks set one; deriving `src/bin/<name>.rs` from the
/// name alone would scan the wrong file, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinTarget {
    name: String,
    /// Relative to the crate directory.
    path: PathBuf,
}

/// Every bin target of one crate: `[[bin]]` blocks, then the `src/bin/*.rs`
/// and `src/main.rs` cargo infers.
fn bin_targets(crate_dir: &Path) -> Vec<BinTarget> {
    let manifest = manifest(&crate_dir.join("Cargo.toml"));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    // Paths an explicit `[[bin]]` already owns. `epics-oracle-rs` names one
    // `oracle-ioc` with `path = "src/bin/oracle_ioc.rs"`; inferring a second
    // target from that file's stem would invent a binary cargo never builds.
    let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();
    let bins = manifest.get("bin").and_then(|b| b.as_array());
    for bin in bins.into_iter().flatten().filter_map(|b| b.as_table()) {
        let Some(name) = bin.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let path = bin.get("path").and_then(|p| p.as_str());
        let path = PathBuf::from(path.map_or_else(|| format!("src/bin/{name}.rs"), str::to_string));
        if seen.insert(name.to_string()) {
            claimed.insert(path.clone());
            out.push(BinTarget {
                name: name.to_string(),
                path,
            });
        }
    }

    let mut inferred: Vec<String> = Vec::new();
    for path in dir_entries(&crate_dir.join("src/bin")) {
        if path.extension().is_some_and(|e| e == "rs")
            && let Some(stem) = path.file_stem()
        {
            inferred.push(stem.to_string_lossy().into_owned());
        }
    }
    inferred.sort();
    for name in inferred {
        let path = PathBuf::from(format!("src/bin/{name}.rs"));
        if !claimed.contains(&path) && seen.insert(name.clone()) {
            out.push(BinTarget { name, path });
        }
    }
    let main = PathBuf::from("src/main.rs");
    if crate_dir.join(&main).is_file()
        && !claimed.contains(&main)
        && let Some(name) = package_name(&crate_dir.join("Cargo.toml"))
        && seen.insert(name.clone())
    {
        out.push(BinTarget { name, path: main });
    }
    out
}

/// Strip `//` line comments and `/* */` blocks, leaving string literals alone.
///
/// Without this the gate reports on its own doc comments, and on every file
/// that merely *mentions* softIoc in prose — `epics-ca-rs/src/cli.rs` does.
fn code_only(line: &str, in_block: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        if *in_block {
            if c == '*' && next == Some('/') {
                *in_block = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = next {
                    out.push(n);
                }
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '/' && next == Some('/') {
            break;
        }
        if c == '/' && next == Some('*') {
            *in_block = true;
            i += 2;
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Does this line of code name an IOC executable?
///
/// A token inside a string literal counts only when the literal holds no
/// whitespace: `which("softIoc")` and `"/opt/epics/bin/softIoc"` are program
/// names, `"... clears UDF at init (softIoc: UDF 0)"` is a sentence about one,
/// and `"x_softioc::child"` is the path of a test a binary re-runs itself for.
/// Outside a string literal the token is an identifier -- `spawn_softioc`,
/// `SOFT_IOC_PVX` -- and always counts.
fn names_ioc_program(code: &str, tokens: &BTreeSet<String>) -> bool {
    let mut outside = String::new();
    let mut literals: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_str = false;
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if c == '\\' {
                chars.next();
                current.push('x');
                continue;
            }
            if c == '"' {
                in_str = false;
                literals.push(std::mem::take(&mut current));
                continue;
            }
            current.push(c);
            continue;
        }
        if c == '"' {
            in_str = true;
            continue;
        }
        outside.push(c);
    }
    if in_str {
        literals.push(current);
    }
    let outside = outside.to_ascii_lowercase();
    if tokens.iter().any(|t| outside.contains(t.as_str())) {
        return true;
    }
    let rust_path = |lit: &str| {
        lit.contains("::")
            && lit
                .split("::")
                .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '_'))
    };
    literals.iter().any(|lit| {
        !lit.chars().any(char::is_whitespace) && !rust_path(lit) && {
            let lower = lit.to_ascii_lowercase();
            tokens.iter().any(|t| lower.contains(t.as_str()))
        }
    })
}

/// The `std::process` / `tokio::process` items that start or hold a child,
/// under every name one file may call them by.
///
/// Resolved from the file's own `use` items rather than matched as text.
/// Text-matching a type name is defeated by a module-renaming import — `use
/// std::process as p; p::Command::new(..)` names no `process::Command`
/// anywhere — and a real spawner that hides that way is a spawner the gate
/// reports green over, which is the one failure a coverage instrument may
/// not have.
///
/// `ExitCode`, `exit` and `abort` are deliberately absent: they end this
/// process, they do not start another.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ProcessNames {
    /// Identifiers that resolve to `process::Command` here.
    command: BTreeSet<String>,
    /// Identifiers that resolve to `process::Stdio` or `process::Child`.
    /// Neither can start a child on its own, but neither exists for any
    /// other purpose, so a call that mentions one is operating on somebody
    /// else's `Command` — which is how `interop_pvxs_mods/*.rs` spawns
    /// `softIocPVX` through a helper that hands the `Command` back.
    child_side: BTreeSet<String>,
    /// Names the `process` MODULE answers to. Seeded with `process` itself,
    /// which covers both the fully-qualified `std::process::Command::new`
    /// and a plain `use std::process;`, then extended by each `as` alias.
    module: BTreeSet<String>,
}

/// Collapse a source fragment to its token text: whitespace runs become one
/// space, and spaces touching path or list punctuation go away, so `use std ::
/// process :: { Command , Stdio }` and its rustfmt spelling parse alike.
/// ` as ` survives, because it is the only keyword these items carry.
fn squeeze_item(item: &str) -> String {
    let single: String = item.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(single.len());
    let chars: Vec<char> = single.chars().collect();
    let punct = |c: char| matches!(c, ':' | '{' | '}' | ',' | ';');
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let prev = out.chars().last();
            let next = chars.get(i + 1).copied();
            if prev.is_some_and(punct) || next.is_some_and(punct) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Every `use ...;` item in a de-commented file, squeezed.
fn use_items(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = code;
    while let Some(i) = rest.find("use ") {
        let before_is_boundary = rest[..i]
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != ':');
        let tail = &rest[i..];
        let end = tail.find(';').map(|e| e + 1).unwrap_or(tail.len());
        if before_is_boundary {
            out.push(squeeze_item(&tail[..end]));
        }
        rest = &tail[end.max(1)..];
    }
    out
}

/// Which of this file's names denote a process item.
fn resolve_process_names(code: &str) -> ProcessNames {
    let mut names = ProcessNames {
        module: ["process".to_string()].into_iter().collect(),
        ..Default::default()
    };
    for item in use_items(code) {
        let Some(at) = item.find("process") else {
            continue;
        };
        // `...::process` and not `...::preprocess`, `...::processing`.
        let before_ok = item[..at]
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if !before_ok {
            continue;
        }
        let tail = &item[at + "process".len()..];
        if let Some(alias) = tail.strip_prefix(" as ").and_then(|a| a.strip_suffix(';')) {
            names.module.insert(alias.to_string());
            continue;
        }
        let Some(tail) = tail.strip_prefix("::") else {
            continue; // plain `use std::process;` — the seeded name covers it
        };
        let entries: Vec<&str> = match tail.strip_prefix('{') {
            Some(group) => group
                .split_once('}')
                .map(|(inner, _)| inner.split(',').collect())
                .unwrap_or_default(),
            None => vec![tail.trim_end_matches(';')],
        };
        for entry in entries {
            let (item_name, alias) = match entry.split_once(" as ") {
                Some((n, a)) => (n.trim(), a.trim().trim_end_matches(';')),
                None => (entry.trim(), entry.trim()),
            };
            let alias = alias.trim_end_matches(';').to_string();
            match item_name {
                "Command" => names.command.insert(alias),
                "Stdio" | "Child" => names.child_side.insert(alias),
                _ => false,
            };
        }
    }
    names
}

/// Does `hay` contain `needle` as a whole path, with `rooted` deciding
/// whether anything may precede it?
///
/// The two callers want opposite answers about a leading `::`, and that
/// difference is the whole of resolving a path rather than matching a name.
/// A BARE name must be rooted — `Command::new(` names this file's imported
/// `Command`, and `clap::Command::new(` is a different type that happens to
/// share the spelling. A MODULE-QUALIFIED name must not be — the
/// fully-qualified `std::process::Command::new(` is the commonest spelling
/// there is, and demanding a rooted `process` would refuse it.
fn has_path_where(hay: &str, needle: &str, rooted: bool) -> bool {
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let at = from + i;
        let prev = hay[..at].chars().last();
        let before_ok =
            prev.is_none_or(|c| !c.is_alphanumeric() && c != '_' && (!rooted || c != ':'));
        let after = &hay[at + needle.len()..];
        let after_ok = needle.ends_with(|c: char| !c.is_alphanumeric() && c != '_')
            || after
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if before_ok && after_ok {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// A name this file introduced, so nothing may stand in front of it.
fn has_path(hay: &str, needle: &str) -> bool {
    has_path_where(hay, needle, true)
}

/// A path that may be reached through its module, `std::process::` included.
fn has_qualified(hay: &str, needle: &str) -> bool {
    has_path_where(hay, needle, false)
}

/// Is a resolved `process::Command` being constructed in this statement?
fn constructs_command(stmt: &str, names: &ProcessNames) -> bool {
    names
        .command
        .iter()
        .any(|c| has_path(stmt, &format!("{c}::new(")))
        || names
            .module
            .iter()
            .any(|m| has_qualified(stmt, &format!("{m}::Command::new(")))
}

/// Does this statement name a resolved `Stdio` / `Child`?
fn names_child_side(stmt: &str, names: &ProcessNames) -> bool {
    names
        .child_side
        .iter()
        .any(|t| has_path(stmt, &format!("{t}::")))
        || names.module.iter().any(|m| {
            has_qualified(stmt, &format!("{m}::Stdio::"))
                || has_qualified(stmt, &format!("{m}::Child::"))
        })
}

/// The bindings a statement introduces that hold a resolved `Command`.
///
/// Two shapes, both resolved rather than guessed: a `let` whose initialiser
/// constructs one, and any `name: [&][mut] <Command>` — a function parameter,
/// a struct field, an annotated `let`.
fn command_bindings(stmt: &str, names: &ProcessNames, out: &mut BTreeSet<String>) {
    if constructs_command(stmt, names)
        && let Some(rest) = stmt.split("let ").nth(1)
    {
        let rest = rest.strip_prefix("mut ").unwrap_or(rest);
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !ident.is_empty() {
            out.insert(ident);
        }
    }
    for ty in &names.command {
        let mut from = 0;
        while let Some(i) = stmt[from..].find(ty.as_str()) {
            let at = from + i;
            from = at + ty.len();
            let after = stmt[from..].chars().next();
            if after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':') {
                continue;
            }
            // Walk left over `&`, `mut`, spaces to the `:` that types it.
            let head = stmt[..at].trim_end();
            let head = head.trim_end_matches("mut").trim_end();
            let head = head.trim_end_matches('&').trim_end();
            let Some(head) = head.strip_suffix(':') else {
                continue;
            };
            if head.ends_with(':') {
                continue; // a `::` path segment, not a type annotation
            }
            let ident: String = head
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if !ident.is_empty() {
                out.insert(ident);
            }
        }
    }
}

/// Does this file start a child process, and where?
///
/// The judgement is per STATEMENT, not per file. A method marker on its own
/// is a name, not a spawn — `Failure::status`, `Response::status` and
/// `clap::Command::new` all wear one — so it counts only when the statement
/// it sits in also holds a resolved `Command` (built here, or bound to a
/// name typed as one) or a resolved `Stdio`/`Child`, which nothing but a
/// child process uses.
///
/// Constructing a resolved `Command` counts on its own, with no marker: a
/// builder that hands the `Command` back spawns through its caller, and the
/// caller is routinely a different file of the same closure.
fn spawn_site(code: &str) -> bool {
    // Assembled with `concat!` so the live scan cannot match this file on its
    // own marker table -- the same trick the sibling gate uses for its fixtures.
    const SPAWN_METHODS: [&str; 3] = [
        concat!(".spa", "wn()"),
        concat!(".out", "put()"),
        concat!(".sta", "tus()"),
    ];
    let names = resolve_process_names(code);
    let mut bound: BTreeSet<String> = BTreeSet::new();
    for stmt in code.split(';') {
        command_bindings(stmt, &names, &mut bound);
        if constructs_command(stmt, &names) {
            return true;
        }
        if !SPAWN_METHODS.iter().any(|m| stmt.contains(m)) {
            continue;
        }
        if names_child_side(stmt, &names) || bound.iter().any(|b| has_path(stmt, b)) {
            return true;
        }
    }
    false
}

/// Does this file both name an IOC executable and spawn a process?
///
/// Both halves are needed. A file that names `softIocPVX` without spawning is
/// a helper constant; a file that spawns without naming one runs `pvxget` or
/// `python3`. The two together is what puts an IOC on the host, and it is the
/// granularity of the verdict anyway — the whole binary is capped or not.
///
/// "Spawns" is `spawn_site`: a resolved `process::Command` reached in one
/// statement, never a method marker on its own.
pub fn spawn_evidence(text: &str, tokens: &BTreeSet<String>) -> Option<String> {
    let mut code = String::with_capacity(text.len());
    let mut lines: Vec<(usize, String, &str)> = Vec::new();
    let mut in_block = false;
    for (i, raw) in text.lines().enumerate() {
        let line = code_only(raw, &mut in_block);
        if line.trim().is_empty() {
            continue;
        }
        code.push_str(&line);
        code.push('\n');
        lines.push((i + 1, line, raw));
    }
    if !spawn_site(&code) {
        return None;
    }
    lines
        .iter()
        .find(|(_, line, _)| names_ioc_program(line, tokens))
        .map(|(n, _, raw)| format!("{n}: {}", raw.trim()))
}

// ---------------------------------------------------------------------------
// Attribution: source file -> nextest binary
// ---------------------------------------------------------------------------

/// A nextest binary that must be declared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spawner {
    /// Cargo package name, as `package()` spells it.
    pub package: String,
    /// nextest binary name, as `binary()` spells it. `None` for the package's
    /// library unit-test binary, whose name is a cargo-derived detail — those
    /// are required to be covered by a `package()` term instead.
    pub binary: Option<String>,
    /// The top-level module of a merged `tests/<dir>/main.rs` binary, whose
    /// tests nextest names `<module>::…` and `test(/^<module>::/)` selects;
    /// `None` for a binary that is one unit.
    pub module: Option<String>,
    /// The file and line that made it a spawner.
    pub evidence: String,
}

impl Spawner {
    /// How the failure text names it, in nextest's own `binary test` order.
    pub fn label(&self) -> String {
        match (&self.binary, &self.module) {
            (Some(b), Some(m)) => format!("{}::{b} {m}::", self.package),
            (Some(b), None) => format!("{}::{b}", self.package),
            (None, _) => format!("{} (lib tests)", self.package),
        }
    }
}

/// One file the census reads.
///
/// A file that is ABSENT is a legitimate answer — not every crate has a
/// `Cargo.toml` at every path this asks about. A file that exists and cannot
/// be read is not: `unwrap_or_default` turned it into empty text, and every
/// check the census then ran over that text passed because there was nothing
/// in it to fail on. A guard that cannot see its subject must say so.
fn read(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => panic!("ioc-spawn-gate: cannot read {}: {e}", path.display()),
    }
}

/// The entries of one directory the census walks, same rule as [`read`]: an
/// absent directory contributes nothing, an unreadable one is a fault. The
/// per-entry error is raised too — `entries.flatten()` dropped those silently,
/// which removes files from the walk one at a time.
fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(e) => panic!(
            "ioc-spawn-gate: cannot read directory {}: {e}",
            dir.display()
        ),
    };
    entries
        .map(|entry| {
            entry
                .unwrap_or_else(|e| {
                    panic!(
                        "ioc-spawn-gate: cannot read an entry of {}: {e}",
                        dir.display()
                    )
                })
                .path()
        })
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for path in dir_entries(dir) {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// One out-of-line `mod name;`, resolved to the file it loads and the
/// directory that file's own plain `mod`s resolve under.
struct ModDecl {
    name: String,
    file: PathBuf,
    children: PathBuf,
}

/// The out-of-line `mod name;` declarations of `file`, resolved as rustc
/// resolves them: a `#[path]` relative to `file`'s directory, a plain `mod`
/// under `children`. A crate root, a `mod.rs` and a `#[path]` file own their
/// own directory; any other module file `x.rs` owns `x/`, which is where a
/// merged binary's `tests/<dir>/x.rs` keeps its private modules. A
/// declaration whose file does not exist is dropped.
fn mod_decls(file: &Path, children: &Path) -> Vec<ModDecl> {
    let Some(dir) = file.parent() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut pending_path: Option<String> = None;
    for line in read(file).lines() {
        let mut trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#[path") {
            if let Some(open) = rest.find('"') {
                let tail = &rest[open + 1..];
                if let Some(close) = tail.find('"') {
                    pending_path = Some(tail[..close].to_string());
                }
            }
            // `#[path = ".."] mod x;` on one line carries its `mod` after `]`.
            trimmed = rest.split_once(']').map_or("", |(_, after)| after.trim());
            if trimmed.is_empty() {
                continue;
            }
        }
        let decl = trimmed.strip_prefix("pub ").unwrap_or(trimmed);
        let Some(rest) = decl.strip_prefix("mod ") else {
            if trimmed.starts_with("#[") || trimmed.is_empty() {
                continue; // cfg attributes sit between #[path] and mod
            }
            pending_path = None;
            continue;
        };
        let Some(name) = rest.strip_suffix(';') else {
            pending_path = None;
            continue;
        };
        let name = name.trim().to_string();
        let found = match pending_path.take() {
            Some(p) => Some(dir.join(p)).filter(|f| f.is_file()).map(|f| {
                let owns = f.parent().unwrap_or(dir).to_path_buf();
                (f, owns)
            }),
            None => {
                let owns = children.join(&name);
                [children.join(format!("{name}.rs")), owns.join("mod.rs")]
                    .into_iter()
                    .find(|c| c.is_file())
                    .map(|f| (f, owns))
            }
        };
        if let Some((file, children)) = found {
            out.push(ModDecl {
                name,
                file,
                children,
            });
        }
    }
    out
}

/// The files `root` pulls in, following `mod` and `#[path]`, sorted; its
/// plain `mod`s resolve under `children`.
///
/// A test binary's spawn sites are usually in a shared helper module —
/// `tests/integration/common/mod.rs`,
/// `tests/integration/interop_helpers/mod.rs` — so scanning only the
/// root file would attribute nothing to the binary that actually spawns.
fn module_closure(root: &Path, children: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![(root.to_path_buf(), children.to_path_buf())];
    let mut out = Vec::new();
    while let Some((file, children)) = queue.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        for d in mod_decls(&file, &children) {
            queue.push((d.file, d.children));
        }
        out.push(file);
    }
    out.sort();
    out
}

/// [`module_closure`] of a crate root, which owns its own directory.
fn crate_root_closure(root: &Path) -> Vec<PathBuf> {
    module_closure(root, root.parent().unwrap_or(root))
}

/// Whether `text` declares a test: an attribute whose last path segment says
/// `test` — `#[test]`, `#[tokio::test]`, `#[epics_test]`, `#[test_case(..)]`.
fn declares_a_test(text: &str) -> bool {
    let mut in_block = false;
    text.lines().any(|line| {
        let code = code_only(line, &mut in_block);
        code.match_indices("#[").any(|(at, _)| {
            let path: String = code[at + 2..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            path.rsplit("::")
                .next()
                .is_some_and(|last| last.contains("test"))
        })
    })
}

/// The first path segment of everything `text` names through `crate::`:
/// `crate::h::f()` and `use crate::{h, g as k};` both name `h` (and `g`).
fn crate_refs(text: &str) -> BTreeSet<String> {
    let mut in_block = false;
    let code: String = text
        .lines()
        .map(|l| code_only(l, &mut in_block) + "\n")
        .collect();
    let ident = |s: &str| -> String {
        s.trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    let mut out = BTreeSet::new();
    for (at, needle) in code.match_indices("crate::") {
        if code[..at]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let rest = &code[at + needle.len()..];
        let Some(group) = rest.strip_prefix('{') else {
            out.insert(ident(rest));
            continue;
        };
        // Split the group at its own commas, not a nested group's.
        let mut depth = 0usize;
        let mut seg = String::new();
        for c in group.chars() {
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => break,
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    out.insert(ident(&std::mem::take(&mut seg)));
                    continue;
                }
                _ => {}
            }
            seg.push(c);
        }
        out.insert(ident(&seg));
    }
    out.remove("");
    out
}

/// One test unit nextest can select. A `tests/<name>.rs` is a binary of its
/// own, `binary(<name>)`. A `tests/<dir>/main.rs` is one binary, `<dir>`,
/// built from every module it declares, and each top-level module `m` that
/// declares a test is a unit of it: its tests are named `m::<test>` and
/// `binary(<dir>) & test(/^m::/)` selects them.
struct TestUnit {
    binary: String,
    module: Option<String>,
    /// Every file the unit compiles: its module closure and, for a merged
    /// module, the closures of the sibling modules it reaches through
    /// `crate::` — the shared helpers `main.rs` declares once.
    files: Vec<PathBuf>,
}

/// Every test unit of one crate, sorted by binary then module.
fn test_units(crate_dir: &Path) -> Vec<TestUnit> {
    let mut out = Vec::new();
    for path in dir_entries(&crate_dir.join("tests")) {
        if path.is_file() && path.extension().is_some_and(|e| e == "rs") {
            let Some(stem) = path.file_stem() else {
                continue;
            };
            out.push(TestUnit {
                binary: stem.to_string_lossy().into_owned(),
                module: None,
                files: crate_root_closure(&path),
            });
        } else if path.join("main.rs").is_file()
            && let Some(name) = path.file_name()
        {
            out.extend(merged_units(&path.join("main.rs"), &name.to_string_lossy()));
        }
    }
    out.sort_by(|a, b| (&a.binary, &a.module).cmp(&(&b.binary, &b.module)));
    out
}

/// The units of one merged binary. A top-level module that declares no test
/// is a helper: it is no unit, and its spawn sites count for every module
/// that reaches it through `crate::`. `main.rs`'s own tests, if any, are a
/// unit without a module.
fn merged_units(main: &Path, binary: &str) -> Vec<TestUnit> {
    let dir = main.parent().unwrap_or(main);
    let closures: BTreeMap<String, Vec<PathBuf>> = mod_decls(main, dir)
        .into_iter()
        .map(|d| {
            let files = module_closure(&d.file, &d.children);
            (d.name, files)
        })
        .collect();
    let mut out = Vec::new();
    if declares_a_test(&read(main)) {
        out.push(TestUnit {
            binary: binary.to_string(),
            module: None,
            files: vec![main.to_path_buf()],
        });
    }
    for (name, own) in &closures {
        if !own.iter().any(|f| declares_a_test(&read(f))) {
            continue;
        }
        let mut files = own.clone();
        let mut at = 0;
        while at < files.len() {
            for used in crate_refs(&read(&files[at])) {
                for f in closures.get(&used).into_iter().flatten() {
                    if !files.contains(f) {
                        files.push(f.clone());
                    }
                }
            }
            at += 1;
        }
        files.sort();
        out.push(TestUnit {
            binary: binary.to_string(),
            module: Some(name.clone()),
            files,
        });
    }
    out
}

/// The first of `files` that spawns an IOC, as `path:line: source`.
fn first_evidence(
    files: &[PathBuf],
    crate_dir: &Path,
    tokens: &BTreeSet<String>,
) -> Option<String> {
    files.iter().find_map(|file| {
        let ev = spawn_evidence(&read(file), tokens)?;
        // `/`-joined so the evidence reads the same on every host.
        let rel = match file.strip_prefix(crate_dir) {
            Ok(rel) => rel
                .iter()
                .map(|c| c.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => file.display().to_string(),
        };
        Some(format!("{rel}:{ev}"))
    })
}

/// A crate's bin targets with their module closures, and the library files
/// none of those bins own.
fn split_targets(crate_dir: &Path) -> (Vec<(String, Vec<PathBuf>)>, Vec<PathBuf>) {
    // A bin's own modules belong to the bin, not to the library beside it.
    let mut bin_closures: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut owned_by_a_bin: BTreeSet<PathBuf> = BTreeSet::new();
    for bin in &bin_targets(crate_dir) {
        let root = crate_dir.join(&bin.path);
        if !root.is_file() {
            continue;
        }
        let closure = crate_root_closure(&root);
        owned_by_a_bin.extend(closure.iter().cloned());
        bin_closures.push((bin.name.clone(), closure));
    }
    let mut lib_files = Vec::new();
    rust_files(&crate_dir.join("src"), &mut lib_files);
    lib_files.retain(|f| !owned_by_a_bin.contains(f));
    lib_files.sort();
    (bin_closures, lib_files)
}

/// One manifest, parsed; a missing one is empty, the rule [`read`] applies.
/// A malformed one fails the census rather than dropping its crate from it.
fn manifest(path: &Path) -> toml::Table {
    read(path)
        .parse()
        .unwrap_or_else(|e| panic!("ioc-spawn-gate: cannot parse {}: {e}", path.display()))
}

/// The `path` of one dependency entry, inline (`name = { path = .. }`) or as
/// its own table (`[dependencies.name]`) — both parse to a table.
fn dep_path(entry: &toml::Value) -> Option<&str> {
    entry.get("path").and_then(|p| p.as_str())
}

/// `[workspace.dependencies]` entries that carry a `path`, by key.
fn workspace_dep_dirs(crate_dir: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let Some((root, workspace)) = crate_dir.ancestors().skip(1).find_map(|d| {
        let m = manifest(&d.join("Cargo.toml"));
        m.get("workspace").cloned().map(|w| (d.to_path_buf(), w))
    }) else {
        return out;
    };
    let deps = workspace.get("dependencies").and_then(|d| d.as_table());
    for (key, entry) in deps.into_iter().flatten() {
        if let Some(path) = dep_path(entry) {
            out.insert(key.clone(), root.join(path));
        }
    }
    out
}

/// A crate a binary of this package may link through `[dev-dependencies]`:
/// the identifier code names it by, and its library's spawn evidence.
struct DevDepSpawner {
    ident: String,
    evidence: String,
}

/// The `path` dev-dependencies of one crate whose library spawns an IOC,
/// `workspace = true` entries resolved through the workspace manifest.
///
/// A dev-dependency is linked into every test binary of the package, but only
/// a binary that names it calls it; so the promotion is per binary, by name,
/// where a spawning library of the package's own is promoted to every binary.
fn dev_dep_spawners(crate_dir: &Path, tokens: &BTreeSet<String>) -> Vec<DevDepSpawner> {
    let workspace = workspace_dep_dirs(crate_dir);
    let manifest = manifest(&crate_dir.join("Cargo.toml"));
    // `[dev-dependencies]` and every `[target.'cfg(..)'.dev-dependencies]`.
    let targets = manifest.get("target").and_then(|t| t.as_table());
    let tables = std::iter::once(&manifest)
        .chain(
            targets
                .into_iter()
                .flatten()
                .filter_map(|(_, t)| t.as_table()),
        )
        .filter_map(|t| t.get("dev-dependencies").and_then(|d| d.as_table()));
    let mut out = Vec::new();
    for (key, entry) in tables.flatten() {
        let key = key.as_str();
        let dir = match dep_path(entry) {
            Some(path) => crate_dir.join(path),
            None if entry.get("workspace").and_then(|w| w.as_bool()) == Some(true) => {
                match workspace.get(key) {
                    Some(dir) => dir.clone(),
                    None => continue,
                }
            }
            None => continue,
        };
        let (_, lib_files) = split_targets(&dir);
        let Some(ev) = first_evidence(&lib_files, &dir, tokens) else {
            continue;
        };
        let package = package_name(&dir.join("Cargo.toml")).unwrap_or_else(|| key.to_string());
        out.push(DevDepSpawner {
            ident: key.replace('-', "_"),
            evidence: format!(
                "{package}/{ev} (through the {package} dev-dependency this binary names)"
            ),
        });
    }
    out
}

/// The evidence of the first spawning dev-dependency that `files` name.
fn named_dev_dep_evidence(files: &[PathBuf], deps: &[DevDepSpawner]) -> Option<String> {
    if deps.is_empty() {
        return None;
    }
    files.iter().find_map(|file| {
        let mut in_block = false;
        let code: String = read(file)
            .lines()
            .map(|l| code_only(l, &mut in_block) + "\n")
            .collect();
        deps.iter()
            .find(|d| has_path(&code, &format!("{}::", d.ident)))
            .map(|d| d.evidence.clone())
    })
}

/// Walk one crate and report every nextest binary in it that spawns an IOC.
///
/// Three kinds of binary, enumerated separately because nextest addresses them
/// separately: the library's unit tests (`package()` only), each bin target's
/// unit tests (`binary(<bin name>)`, id `package::bin/<name>`), and each
/// `tests/<name>.rs` (`binary(<name>)`) — or, for a merged
/// `tests/<dir>/main.rs`, each top-level module that declares a test
/// (`binary(<dir>) & test(/^<module>::/)`).
///
/// A spawn site in the *library* additionally makes every binary in the
/// package a spawner, because the library is what they all call:
/// `tests/integration/oracle.rs` spawns three IOCs per case through
/// `epics-oracle-rs/src/ioc.rs` without containing the word `Command`. That
/// promotion ADDS rows. It must never replace them — a library match used to
/// return one package row and drop the crate's real per-binary census on the
/// floor, so introducing one spawn marker under `src/` silently deleted nine
/// declarations' worth of coverage and the gate went green over it. Whatever
/// this function decides, the row set only ever grows; `a_library_spawner_adds
/// _rows_and_removes_none` is the boundary that says so.
pub fn spawners_in_crate(crate_dir: &Path, tokens: &BTreeSet<String>) -> Vec<Spawner> {
    let package = match package_name(&crate_dir.join("Cargo.toml")) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let (bin_closures, lib_files) = split_targets(crate_dir);
    let lib = first_evidence(&lib_files, crate_dir, tokens);
    let dev_deps = dev_dep_spawners(crate_dir, tokens);

    let through_lib = |ev: &str| format!("{ev} (through the {package} library this binary links)");

    // Keyed by the nextest binary and module, because a `binary()` term
    // selects by name: a bin target and a `tests/<name>.rs` of one name are
    // one name, one row, one declaration.
    // First-hand evidence wins over the library fallback; otherwise the first
    // found.
    type Key = (Option<String>, Option<String>);
    let mut rows: BTreeMap<Key, (String, bool)> = BTreeMap::new();
    let mut record = |key: Key, evidence: Option<String>, via_lib: bool| {
        let Some(evidence) = evidence else { return };
        match rows.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((evidence, via_lib));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) if slot.get().1 && !via_lib => {
                slot.insert((evidence, via_lib));
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    };

    match lib.clone() {
        Some(ev) => record((None, None), Some(ev), false),
        None => record(
            (None, None),
            named_dev_dep_evidence(&lib_files, &dev_deps),
            true,
        ),
    }
    let units = bin_closures
        .into_iter()
        .map(|(name, files)| ((Some(name), None), files))
        .chain(
            test_units(crate_dir)
                .into_iter()
                .map(|u| ((Some(u.binary), u.module), u.files)),
        );
    for (key, files) in units {
        match first_evidence(&files, crate_dir, tokens) {
            Some(ev) => record(key, Some(ev), false),
            None => record(
                key,
                named_dev_dep_evidence(&files, &dev_deps).or(lib.as_deref().map(through_lib)),
                true,
            ),
        }
    }

    rows.into_iter()
        .map(|((binary, module), (evidence, _))| Spawner {
            package: package.clone(),
            binary,
            module,
            evidence,
        })
        .collect()
}

fn package_name(path: &Path) -> Option<String> {
    let name = manifest(path)
        .get("package")?
        .get("name")?
        .as_str()?
        .to_string();
    Some(name)
}

/// Every workspace crate that has a `Cargo.toml`, under the member globs.
pub fn workspace_crates(root: &Path) -> Vec<PathBuf> {
    let manifest = manifest(&root.join("Cargo.toml"));
    let list = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .expect("the workspace manifest declares a `members` array");

    let mut out = Vec::new();
    for pattern in list.iter().filter_map(|p| p.as_str()) {
        match pattern.strip_suffix("/*") {
            Some(group) => {
                for path in dir_entries(&root.join(group)) {
                    if path.join("Cargo.toml").is_file() {
                        out.push(path);
                    }
                }
            }
            // The expander knows two shapes. Anything else would match nothing
            // here and take its crates out of the scan silently, so it fails.
            None => {
                assert!(
                    !pattern.contains(['*', '?', '[']),
                    "workspace member glob `{pattern}` is a shape this scan \
                     cannot expand; teach `workspace_crates` about it rather \
                     than leaving its crates unscanned"
                );
                out.push(root.join(pattern));
            }
        }
    }
    out.sort();
    out
}

/// Every name a `binary()` term can legally select, for the dead-name check:
/// each `tests/<name>.rs`, each merged `tests/<dir>/main.rs` and each bin
/// target. A bin target's unit tests run as `package::bin/<name>` and
/// `binary(<name>)` selects them, so a declaration naming one is live, not a
/// leftover. Each name carries the test modules a `test(/^<module>::/)` beside
/// it can select — empty for a binary that is one unit.
pub fn all_declarable_binaries(root: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for dir in workspace_crates(root) {
        for unit in test_units(&dir) {
            out.entry(unit.binary).or_default().extend(unit.module);
        }
        for bin in bin_targets(&dir) {
            out.entry(bin.name).or_default();
        }
    }
    out
}
// ---------------------------------------------------------------------------
// The overrides in .config/nextest.toml
// ---------------------------------------------------------------------------

/// One term of a filterset. Anything outside the small grammar the gate
/// evaluates becomes `Unknown` rather than a parse error, because an
/// unevaluable term only matters when it could shadow an IOC declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Package(String),
    Binary(String),
    BinaryContains(String),
    /// `test(...)`, which selects INSIDE a binary rather than selecting one.
    ///
    /// Whatever it names, the block cannot be a whole binary's declaration,
    /// and that is the only thing this gate asks a filter of a binary. Of a
    /// merged binary's module it asks one more: an exact `test(=name)` outside
    /// the module takes none of it.
    Test(String),
    /// `test(/^P/)` with a literal `P`: the tests whose names start with `P`.
    /// A merged binary's module `m` is all named `m::…`, so `P` takes all of
    /// it when `m::` starts with `P`, part of it when `P` starts with `m::`,
    /// and none of it otherwise. Of any other binary it takes part.
    TestPrefix(String),
    Unknown(String),
}

/// What a filter says about one whole binary.
///
/// Four answers, not three: "this gate cannot tell" and "this claims part of
/// the binary" are different, and collapsing the second into the first is what
/// forced `.config/nextest.toml` to keep its `test()`-narrowed block last. A
/// narrowing block is perfectly readable — it simply cannot be a declaration,
/// and it shadows one only when it would pull some of the binary's tests into
/// a DIFFERENT group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    Yes,
    No,
    Maybe,
    /// Every other term in the clause selects this binary, and a `test()` term
    /// narrows the block to some of its tests.
    Partial,
}

impl Term {
    fn selects(&self, package: &str, binary: Option<&str>, module: Option<&str>) -> Match {
        let yes = |b: bool| if b { Match::Yes } else { Match::No };
        match self {
            Term::Package(p) => yes(p == package),
            Term::Binary(b) => yes(binary == Some(b.as_str())),
            Term::BinaryContains(s) => yes(binary.is_some_and(|b| b.contains(s.as_str()))),
            Term::Test(t) => match (module, t.strip_prefix('=')) {
                (Some(m), Some(name)) if !name.starts_with(&format!("{m}::")) => Match::No,
                _ => Match::Partial,
            },
            Term::TestPrefix(p) => module.map_or(Match::Partial, |m| prefix_selects(p, m)),
            Term::Unknown(_) => Match::Maybe,
        }
    }
}

/// What `test(/^prefix/)` takes of a merged binary's module `module`.
fn prefix_selects(prefix: &str, module: &str) -> Match {
    let whole = format!("{module}::");
    if whole.starts_with(prefix) {
        Match::Yes
    } else if prefix.starts_with(&whole) {
        Match::Partial
    } else {
        Match::No
    }
}

/// The `test(/^P/)` of one conjunction, which narrows its `binary()` terms.
fn clause_prefix(clause: &[Term]) -> Option<&str> {
    clause.iter().find_map(|t| match t {
        Term::TestPrefix(p) => Some(p.as_str()),
        _ => None,
    })
}

/// A filterset as a disjunction of conjunctions — `a & b | c`.
pub type Filterset = Vec<Vec<Term>>;

/// One token of a filterset expression.
enum Tok {
    Atom(Term),
    Open,
    Close,
    Or,
    And,
    Not,
    Minus,
}

/// Split a filterset into tokens; `None` when it holds something this
/// grammar does not read. A predicate's argument is read whole, so the `|`
/// and parentheses inside `test(/^a::(b|c)$/)` stay in it.
fn filter_tokens(filter: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = filter.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let op = match c {
            '(' => Some(Tok::Open),
            ')' => Some(Tok::Close),
            '|' | '+' => Some(Tok::Or),
            '&' => Some(Tok::And),
            '!' => Some(Tok::Not),
            '-' => Some(Tok::Minus),
            _ => None,
        };
        if let Some(op) = op {
            out.push(op);
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if !(c.is_alphabetic() || c == '_') {
            return None;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        match word.as_str() {
            "or" => out.push(Tok::Or),
            "and" => out.push(Tok::And),
            "not" => out.push(Tok::Not),
            _ => {
                if chars.get(i) != Some(&'(') {
                    return None;
                }
                i += 1;
                let arg_start = i;
                if chars.get(i) == Some(&'/') {
                    i += 1;
                    while i < chars.len() && chars[i] != '/' {
                        i += if chars[i] == '\\' { 2 } else { 1 };
                    }
                    i += 1;
                }
                while i < chars.len() && chars[i] != ')' {
                    i += 1;
                }
                if i >= chars.len() {
                    return None;
                }
                let arg: String = chars[arg_start..i].iter().collect();
                i += 1;
                out.push(Tok::Atom(atom(&word, arg.trim())));
            }
        }
    }
    Some(out)
}

/// One predicate, `kind(arg)`.
fn atom(kind: &str, arg: &str) -> Term {
    let literal_prefix = arg
        .strip_prefix("/^")
        .and_then(|p| p.strip_suffix('/'))
        .filter(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
        });
    match (kind, literal_prefix) {
        ("test", Some(p)) => Term::TestPrefix(p.to_string()),
        _ if arg.starts_with('/') => Term::Unknown(format!("{kind}({arg})")),
        ("package", _) => Term::Package(arg.to_string()),
        ("binary", _) => match arg.strip_prefix('~') {
            Some(sub) => Term::BinaryContains(sub.to_string()),
            None => Term::Binary(arg.to_string()),
        },
        ("test", _) => Term::Test(arg.to_string()),
        _ => Term::Unknown(format!("{kind}({arg})")),
    }
}

/// Every conjunction of `a` joined with every conjunction of `b`.
fn product(a: &Filterset, b: &Filterset) -> Filterset {
    a.iter()
        .flat_map(|x| b.iter().map(move |y| x.iter().chain(y).cloned().collect()))
        .collect()
}

/// Recursive descent over [`Tok`], building disjunctive normal form as it
/// goes. A negated operand — `not x`, `!x`, the `x` of `a - x` — is one term
/// the gate cannot evaluate, because the form has no negation.
struct FilterParser<'a> {
    toks: &'a [Tok],
    at: usize,
}

impl FilterParser<'_> {
    fn or(&mut self) -> Option<Filterset> {
        let mut set = self.and()?;
        while matches!(self.toks.get(self.at), Some(Tok::Or)) {
            self.at += 1;
            set.extend(self.and()?);
        }
        Some(set)
    }

    fn and(&mut self) -> Option<Filterset> {
        let mut set = self.unary()?;
        loop {
            let negate = match self.toks.get(self.at) {
                Some(Tok::And) => false,
                Some(Tok::Minus) => true,
                _ => return Some(set),
            };
            self.at += 1;
            let rhs = self.unary()?;
            let rhs = if negate { Self::negated(&rhs) } else { rhs };
            set = product(&set, &rhs);
        }
    }

    fn unary(&mut self) -> Option<Filterset> {
        let tok = self.toks.get(self.at)?;
        self.at += 1;
        match tok {
            Tok::Atom(t) => Some(vec![vec![t.clone()]]),
            Tok::Not => {
                let inner = self.unary()?;
                Some(Self::negated(&inner))
            }
            Tok::Open => {
                let set = self.or()?;
                matches!(self.toks.get(self.at), Some(Tok::Close)).then_some(())?;
                self.at += 1;
                Some(set)
            }
            Tok::Close | Tok::Or | Tok::And | Tok::Minus => None,
        }
    }

    fn negated(inner: &Filterset) -> Filterset {
        vec![vec![Term::Unknown(format!("not {inner:?}"))]]
    }
}

/// Parse `binary(a) | package(b) & (test(c) or test(d))` into disjunctive
/// normal form: `|`, `or` and `+` join, `&` and `and` bind tighter, and
/// parentheses group. A filter this grammar cannot read is one `Unknown`.
pub fn parse_filterset(filter: &str) -> Filterset {
    let unknown = || vec![vec![Term::Unknown(filter.trim().to_string())]];
    let Some(toks) = filter_tokens(filter) else {
        return unknown();
    };
    let mut parser = FilterParser { toks: &toks, at: 0 };
    match parser.or() {
        Some(set) if parser.at == toks.len() => set,
        _ => unknown(),
    }
}

/// Evaluate a whole filterset against one binary, or one module of a merged
/// binary.
///
/// A conjunction is as weak as its weakest term and a disjunction as strong as
/// its strongest clause, over `No` < `Partial` < `Maybe` < `Yes`. `Maybe`
/// outranks `Partial` in both directions on purpose: an unreadable term might
/// claim the whole binary, so it can hide a declaration, while a `test()` term
/// is known to claim only part of one.
pub fn evaluate(
    set: &Filterset,
    package: &str,
    binary: Option<&str>,
    module: Option<&str>,
) -> Match {
    let mut best = Match::No;
    for clause in set {
        let mut clause_verdict = Match::Yes;
        for term in clause {
            match term.selects(package, binary, module) {
                Match::No => {
                    clause_verdict = Match::No;
                    break;
                }
                Match::Maybe => clause_verdict = Match::Maybe,
                Match::Partial => {
                    if clause_verdict == Match::Yes {
                        clause_verdict = Match::Partial;
                    }
                }
                Match::Yes => {}
            }
        }
        match clause_verdict {
            Match::Yes => return Match::Yes,
            Match::Maybe => best = Match::Maybe,
            Match::Partial => {
                if best == Match::No {
                    best = Match::Partial;
                }
            }
            Match::No => {}
        }
    }
    best
}

/// One `[[profile.P.overrides]]` block, in file order.
#[derive(Debug, Clone)]
pub struct Override {
    pub profile: String,
    pub group: String,
    pub filter: String,
    /// The filter in disjunctive normal form.
    pub terms: Filterset,
}

/// Every override block that assigns a test-group, in the order nextest reads
/// them. Order is the whole point: when several blocks match one test, the
/// first one wins, so a later declaration is not the effective one.
pub fn overrides(config: &str) -> Vec<Override> {
    let mut out = Vec::new();
    let mut profile: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut lines = config.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("[[profile.")
            && let Some(name) = rest.strip_suffix(".overrides]]")
        {
            profile = Some(name.to_string());
            filter = None;
            continue;
        }
        if t.starts_with('[') {
            profile = None;
            filter = None;
            continue;
        }
        let Some(p) = profile.clone() else { continue };
        if let Some(rest) = t.strip_prefix("filter") {
            let rest = rest.trim_start().trim_start_matches('=').trim();
            if let Some(body) = rest.strip_prefix("\"\"\"") {
                let mut acc = body.to_string();
                if !acc.contains("\"\"\"") {
                    for more in lines.by_ref() {
                        acc.push(' ');
                        acc.push_str(more);
                        if more.contains("\"\"\"") {
                            break;
                        }
                    }
                }
                filter = Some(
                    acc.replace("\"\"\"", "")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            } else {
                filter = Some(rest.trim_matches(['\'', '"']).to_string());
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("test-group") {
            let group = rest
                .trim_start()
                .trim_start_matches('=')
                .trim()
                .trim_matches(['\'', '"'])
                .to_string();
            if let Some(f) = filter.clone() {
                out.push(Override {
                    profile: p,
                    group,
                    terms: parse_filterset(&f),
                    filter: f,
                });
            }
        }
    }
    out
}

/// The names declared in `[test-groups]`.
pub fn declared_groups(config: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut inside = false;
    for line in config.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == "[test-groups]";
            continue;
        }
        if inside
            && !t.starts_with('#')
            && let Some((name, _)) = t.split_once('=')
        {
            let name = name.trim();
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// The profiles an IOC spawner's declaration must agree across.
///
/// `default` is the dev loop, `ci` is what every workflow runs, `interop` is
/// where the gated interop suites actually execute — `interop_pvxs` runs
/// *only* there, so a declaration that did not reach `interop` would be
/// declaring nothing at all for it.
///
/// nextest applies a profile's own overrides first and then the default
/// profile's, which is why `cargo nextest show-config test-groups --profile
/// interop` attributes a gated binary to an "override for default profile".
/// `effective_group` models that chain rather than assuming each profile
/// carries its own copy.
pub const PROFILES: [&str; 3] = ["default", "ci", "interop"];

/// Which test-group a binary effectively lands in under one profile, modelling
/// nextest's first-match-wins rule.
///
/// A block the gate cannot evaluate, encountered *before* a match, makes the
/// answer unknowable rather than wrong: the unevaluable block might have
/// claimed the binary first, so the declaration found later would not be the
/// effective one. That is reported, not assumed away.
///
/// A `test()`-narrowed block is a different case and is not unknowable. It
/// cannot itself be the declaration, so it is never returned; it breaks the
/// declaration below it only when its group differs, because first-match-wins
/// is per TEST — the tests it names would then run in one group and the rest
/// of the binary in another, which is a cap with a hole in it rather than a
/// cap. Narrowing within the same group takes nothing out of it and is fine.
fn effective_group<'a>(
    blocks: &'a [Override],
    profile: &str,
    package: &str,
    binary: Option<&str>,
    module: Option<&str>,
) -> Result<Option<&'a Override>, String> {
    let mut shadow: Option<&Override> = None;
    let mut narrowed: Vec<&Override> = Vec::new();
    let chain = blocks.iter().filter(|b| b.profile == profile).chain(
        blocks
            .iter()
            .filter(|b| profile != "default" && b.profile == "default"),
    );
    for b in chain {
        match evaluate(&b.terms, package, binary, module) {
            Match::Yes => {
                if let Some(s) = shadow {
                    return Err(format!(
                        "profile.{profile} declares it in `{}`, but the earlier `{}` override \
                         (`{}`) has a filter this gate cannot evaluate and may claim it first",
                        b.group, s.group, s.filter
                    ));
                }
                if let Some(n) = narrowed.iter().find(|n| n.group != b.group) {
                    return Err(format!(
                        "profile.{profile} declares it in `{}`, but the earlier `{}` override \
                         (`{}`) narrows with `test()` and takes part of this binary into a \
                         different group",
                        b.group, n.group, n.filter
                    ));
                }
                return Ok(Some(b));
            }
            Match::Maybe => shadow = Some(b),
            Match::Partial => narrowed.push(b),
            Match::No => {}
        }
    }
    match shadow {
        Some(s) => Err(format!(
            "profile.{profile} puts it in no group the gate can read; the `{}` override (`{}`) \
             is unevaluable and may or may not claim it",
            s.group, s.filter
        )),
        // Narrowing blocks with no declaration under them leave the binary as
        // a whole ungrouped, which is exactly what `Ok(None)` means.
        None => Ok(None),
    }
}

/// The two test-groups that mean "this binary spawns an IOC". `pva_listener`
/// is accepted as a declaration but says nothing about spawning, so it is not
/// one of these.
const IOC_GROUPS: [&str; 2] = ["ca_softioc", "ioc_unthrottled"];

/// Run the whole check and return the failure text, or `None` when it holds.
///
/// Takes no floors. It used to take a `min_crates` and a `min_spawners`, both
/// guarding "the scan found nothing because a directory moved or the markers
/// stopped matching" — and a floor under a growing count goes inert rather than
/// stale: 30 against 37 crates and 15 against 19 spawners, with the doc
/// comment still saying 16. Both are now structural instead. The crate set
/// comes from the workspace manifest's own `members` globs, so a moved
/// directory moves the manifest with it and an unexpandable glob shape panics;
/// and the spawner scan is checked against the declarations rather than against
/// a number, which is the subject of `every_ioc_group_declaration_is_a_spawner`
/// below.
pub fn check(root: &Path) -> Option<String> {
    let mut problems: Vec<String> = Vec::new();

    let crates = workspace_crates(root);
    if crates.is_empty() {
        return Some(format!(
            "{RULE}\n\nthe manifest's `members` expanded to no crate under {}; \
             it is not scanning the workspace",
            root.display()
        ));
    }

    let config_path = root.join(".config/nextest.toml");
    let config = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => return Some(format!("{RULE}\n\nread {}: {e}", config_path.display())),
    };

    let blocks = overrides(&config);
    let groups = declared_groups(&config);
    for b in &blocks {
        if !groups.contains(&b.group) {
            problems.push(format!(
                "profile.{} assigns test-group `{}`, which [test-groups] does not declare",
                b.profile, b.group
            ));
        }
    }

    let known = all_declarable_binaries(root);
    for b in &blocks {
        for clause in &b.terms {
            let prefix = clause_prefix(clause);
            for term in clause {
                let Term::Binary(name) = term else { continue };
                match known.get(name) {
                    None => problems.push(format!(
                        "profile.{}'s `{}` filter names `binary({name})`, which is no test \
                         binary or bin target in the workspace — a dead name left by a rename \
                         or a deletion",
                        b.profile, b.group
                    )),
                    Some(modules) => {
                        if let Some(p) = prefix
                            && !modules.is_empty()
                            && modules.iter().all(|m| prefix_selects(p, m) == Match::No)
                        {
                            problems.push(format!(
                                "profile.{}'s `{}` filter narrows `binary({name})` with \
                                 `test(/^{p}/)`, which is none of its modules — a dead name \
                                 left by a rename or a deletion",
                                b.profile, b.group
                            ));
                        }
                    }
                }
            }
        }
    }

    let tokens = ioc_program_tokens(root);
    let mut spawners: Vec<Spawner> = Vec::new();
    for dir in &crates {
        spawners.extend(spawners_in_crate(dir, &tokens));
    }
    spawners.sort();

    // The reverse direction of the rule, and what replaces the old floor: the
    // two IOC groups are a list somebody wrote after deciding a binary spawns
    // an IOC, so every name in them must still scan as a spawner. If
    // `ioc_program_tokens` stops matching — a renamed executable, a rewritten
    // spawn site — the forward check goes quiet because there is nothing left
    // to demand a declaration for, and this one goes red naming exactly which
    // declarations lost their evidence. A floor could only say "fewer than
    // before".
    let spawner_packages: BTreeSet<&str> = spawners.iter().map(|s| s.package.as_str()).collect();
    for b in &blocks {
        if !IOC_GROUPS.contains(&b.group.as_str()) {
            continue;
        }
        for clause in &b.terms {
            // A `test(/^m::/)` beside a `binary()` declares that module, not
            // the binary, so the module is what must still scan as a spawner.
            let prefix = clause_prefix(clause);
            for term in clause {
                let missing = match term {
                    Term::Binary(name)
                        if !spawners.iter().any(|s| {
                            s.binary.as_deref() == Some(name.as_str())
                                && prefix.is_none_or(|p| {
                                    s.module
                                        .as_deref()
                                        .is_none_or(|m| prefix_selects(p, m) != Match::No)
                                })
                        }) =>
                    {
                        Some(match prefix {
                            Some(p) => format!("binary({name}) & test(/^{p}/)"),
                            None => format!("binary({name})"),
                        })
                    }
                    Term::Package(name) if !spawner_packages.contains(name.as_str()) => {
                        Some(format!("package({name})"))
                    }
                    _ => None,
                };
                if let Some(what) = missing {
                    problems.push(format!(
                        "profile.{}'s `{}` filter declares `{what}` an IOC spawner, but the \
                         scan finds no spawn site under it. Either the spawn site went away \
                         and the declaration should too, or the scan stopped recognising it \
                         and the forward check above is now blind",
                        b.profile, b.group
                    ));
                }
            }
        }
    }

    for s in &spawners {
        let mut landed: BTreeMap<&str, String> = BTreeMap::new();
        let mut undeclared: Vec<&str> = Vec::new();
        for profile in PROFILES {
            match effective_group(
                &blocks,
                profile,
                &s.package,
                s.binary.as_deref(),
                s.module.as_deref(),
            ) {
                Ok(Some(b)) => {
                    landed.insert(profile, b.group.clone());
                }
                Ok(None) => undeclared.push(profile),
                Err(why) => problems.push(format!("{}: {why}", s.label())),
            }
        }
        if !undeclared.is_empty() {
            problems.push(format!(
                "{} spawns an IOC ({}) and is in no test-group under profile {}",
                s.label(),
                s.evidence,
                undeclared.join(", profile ")
            ));
        }
        let distinct: BTreeSet<&String> = landed.values().collect();
        if distinct.len() > 1 {
            problems.push(format!(
                "{} lands in a different test-group per profile: {landed:?}",
                s.label()
            ));
        }
    }

    if problems.is_empty() {
        return None;
    }
    problems.sort();
    problems.dedup();
    let mut msg = String::new();
    let _ = write!(msg, "{RULE}\n\n{} breach(es):", problems.len());
    for p in problems {
        let _ = write!(msg, "\n  - {p}");
    }
    Some(msg)
}

/// Panic, naming the rule, unless every IOC-spawning binary is declared.
pub fn assert_every_ioc_spawner_is_declared(root: &Path) {
    if let Some(msg) = check(root) {
        panic!("{msg}");
    }
}

/// The census, for the report and for `cargo run --bin ioc-spawn-census`.
pub fn census(root: &Path) -> Vec<Spawner> {
    let tokens = ioc_program_tokens(root);
    let mut out = Vec::new();
    for dir in workspace_crates(root) {
        out.extend(spawners_in_crate(&dir, &tokens));
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// The gate's own tests — one per boundary of the two judgements it makes:
// "is this file a spawner" and "which group claims this binary". Fixtures are
// assembled with `concat!` so this file cannot match itself during the live
// scan above.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> BTreeSet<String> {
        ["softioc", "qsrv-ioc"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// A bin is an IOC by what its root builds, not by a comment naming it.
    #[test]
    fn a_bin_building_an_ioc_application_is_an_ioc_program() {
        assert!(builds_an_ioc_application(
            "use epics_base_rs::server::ioc_app::IocApplication;\n"
        ));
        assert!(!builds_an_ioc_application(
            "// unlike an IocApplication, this only forwards\nfn main() {}\n"
        ));
    }

    fn spawn_call() -> &'static str {
        concat!("Comm", "and::new")
    }

    /// Every fixture that means "this spawns a process" has to say where its
    /// `Command` comes from, because that is now half of the judgement — see
    /// [`ProcessNames`]. Real spawn sites all carry the import; the fixtures
    /// used not to, which is exactly the gap this hid.
    fn process_import() -> &'static str {
        concat!("use std::proc", "ess::Command;\n")
    }

    /// Build a throwaway crate on disk. The census reads files, so these
    /// boundaries are exercised against a real directory layout.
    fn write_crate(crate_dir: &Path, manifest: &str, files: &[(&str, &str)]) {
        fs::create_dir_all(crate_dir).unwrap();
        fs::write(crate_dir.join("Cargo.toml"), manifest).unwrap();
        for (rel, body) in files {
            let path = crate_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }

    fn labels(rows: &[Spawner]) -> Vec<String> {
        rows.iter().map(Spawner::label).collect()
    }

    /// The base case: a program name in a string literal, handed to a spawn.
    #[test]
    fn a_literal_program_name_next_to_a_spawn_is_a_spawner() {
        let src = format!(
            "{}    let mut cmd = {}(\"softIoc\");\n",
            process_import(),
            spawn_call()
        );
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// The name may be several lines from the spawn — it usually is, because
    /// the path is resolved into a local first.
    #[test]
    fn a_name_bound_to_a_local_above_the_spawn_still_counts() {
        let src = format!(
            "{}    let exe = env!(\"CARGO_BIN_EXE_softioc-rs\");\n    let c = {}(&exe);\n",
            process_import(),
            spawn_call()
        );
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// Prose about an IOC is not an IOC. This is the false positive that made
    /// `epics-base-rs` look like a spawner: a test-name string mentioning
    /// softIoc in a crate that shells out for something else entirely.
    #[test]
    fn a_program_name_inside_a_sentence_is_not_a_spawner() {
        let src = format!(
            "    let msg = \"histogram: clears UDF at init (softIoc: UDF 0)\";\n    let c = {}(\"python3\");\n",
            spawn_call()
        );
        assert_eq!(spawn_evidence(&src, &tokens()), None);
    }

    /// Nor is the path of a test a merged binary re-runs itself for, whose
    /// module is named after the file and so may hold a program's name.
    #[test]
    fn a_test_path_naming_a_program_is_not_a_spawner() {
        let reexec = |arg: &str| {
            format!(
                "{}    let c = {}(std::env::current_exe().unwrap()).arg(\"{arg}\");\n",
                process_import(),
                spawn_call()
            )
        };
        let src = reexec("posts_like_softioc::child_cooldown");
        assert_eq!(spawn_evidence(&src, &tokens()), None);
        let src = reexec("bin/softIoc");
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// Nor is a doc comment. Every parity-review file in this workspace names
    /// softIoc somewhere in prose.
    #[test]
    fn a_program_name_in_a_comment_is_not_a_spawner() {
        let src = format!(
            "    /// Spawn softIoc from the workspace target dir.\n    let c = {}(\"python3\");\n",
            spawn_call()
        );
        assert_eq!(spawn_evidence(&src, &tokens()), None);
    }

    /// A file that spawns `pvxget` or `camonitor` is not putting an IOC on the
    /// host, and capping it would cost wall time for nothing.
    #[test]
    fn spawning_something_that_is_not_an_ioc_is_not_a_spawner() {
        let src = format!("    let c = {}(\"camonitor\");\n", spawn_call());
        assert_eq!(spawn_evidence(&src, &tokens()), None);
    }

    /// Naming one without spawning it is a helper constant, not a spawner.
    #[test]
    fn naming_an_ioc_without_spawning_is_not_a_spawner() {
        let src = "pub const SOFT_IOC_PVX: &str = \"softIocPVX\";\n";
        assert_eq!(spawn_evidence(src, &tokens()), None);
    }

    /// The false positive that made this gate under-report: `.status()` is a
    /// method name, not a spawn. `Failure::status` in
    /// `epics-ca-rs/src/bin/softioc-rs.rs` — a file that names `softioc`
    /// because that is its own clap command name — matched the bare marker,
    /// and the binary it took down with it was the crate's whole test census.
    #[test]
    fn a_bare_status_call_is_not_a_spawn_without_a_process_command() {
        let src = concat!(
            "#[command(name = \"softioc\")]\n",
            "struct Args;\n",
            "fn report(f: &Failure) -> ExitCode {\n",
            "    ExitCode::from(f.sta",
            "tus())\n}\n"
        );
        assert_eq!(spawn_evidence(src, &tokens()), None);
    }

    /// But a file that only ever *configures* someone else's `Command` still
    /// spawns: `interop_pvxs_mods/pipeline_r1.rs` builds `softIocPVX` through
    /// a helper and names nothing but `Stdio`.
    #[test]
    fn configuring_a_spawn_with_stdio_alone_counts() {
        let src = concat!(
            "use std::proc",
            "ess::Stdio;\n",
            "fn go() {\n",
            "    let mut cmd = pvxs_command(\"softIocPVX\");\n",
            "    cmd.stdout(Stdio::piped()).spa",
            "wn().unwrap();\n}\n"
        );
        assert!(spawn_evidence(src, &tokens()).is_some());
    }

    /// `use std::process::{Command, Stdio};` names the type in a group, which
    /// a plain `process::Command` substring never sees.
    #[test]
    fn a_process_type_named_only_in_a_group_import_counts() {
        let src = format!(
            "{}fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            concat!("use std::proc", "ess::{Stdio, Command};\n"),
            spawn_call()
        );
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// The miss that made a green run meaningless: renaming the MODULE hides
    /// every process type behind a name no substring search knows. This is
    /// `cli_tcp_port_fallback.rs` rewritten — a real spawner in this tree that
    /// vanished from the census under `use std::process as p`.
    #[test]
    fn a_module_renaming_import_still_resolves() {
        let src = format!(
            "{}fn go() {{ let _ = p::Comm{}(\"softIoc\"); }}\n",
            concat!("use std::proc", "ess as p;\n"),
            "and::new"
        );
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// The same trick one level down: the TYPE renamed by `as`.
    #[test]
    fn a_renamed_command_type_still_resolves() {
        let src = format!(
            "{}fn go() {{ let _ = Cmd::new(\"softIoc\"); }}\n",
            concat!("use std::proc", "ess::Command as Cmd;\n")
        );
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// No `use` item at all. `epics-oracle-rs/tests/integration/oracle.rs`
    /// spells the path in full, and a module probe that demanded nothing
    /// precede `process` would refuse the commonest spelling there is.
    #[test]
    fn a_fully_qualified_path_needs_no_import() {
        let src = format!(
            "fn go() {{ let _ = std::proc{}(\"softIoc\"); }}\n",
            concat!("ess::Comm", "and::new")
        );
        assert!(spawn_evidence(&src, &tokens()).is_some());
    }

    /// The other half of resolving rather than matching: a `Command` that is
    /// somebody else's type. `clap::Command::new("softioc")` is every one of
    /// this workspace's IOC binaries describing its own CLI.
    #[test]
    fn another_crate_s_command_type_is_not_a_process_command() {
        let src = format!(
            "use clap::Command;\nfn go() {{ let _ = {}(\"softIoc\").get_matches(); }}\n",
            spawn_call()
        );
        assert_eq!(spawn_evidence(&src, &tokens()), None);
    }

    /// And a module whose name merely ends in `process`.
    #[test]
    fn a_module_whose_name_ends_in_process_is_not_the_process_module() {
        let src = format!(
            "fn go() {{ let _ = preproc{}(\"softIoc\"); }}\n",
            concat!("ess::Comm", "and::new")
        );
        assert_eq!(spawn_evidence(&src, &tokens()), None);
    }

    /// A spawn marker on a binding this file never built, typed as a
    /// `Command` by its parameter list. Resolving the type is the only way to
    /// tell it from `Failure::exit_status`.
    #[test]
    fn a_spawn_on_a_parameter_typed_as_a_command_counts() {
        let src = concat!(
            "use std::proc",
            "ess::Command;\n",
            "fn run(cmd: &mut Command) {\n",
            "    let _ = cmd.sta",
            "tus();\n}\n",
            "const EXE: &str = \"softIoc\";\n"
        );
        assert!(spawn_evidence(src, &tokens()).is_some());
    }

    /// THE regression. A spawn marker appearing under `src/` used to make
    /// `spawners_in_crate` return one package row and discard every test
    /// binary it had already found — nine of them, in `epics-ca-rs` — so the
    /// gate reported green while covering nothing. Adding a spawner may only
    /// ever add rows.
    #[test]
    fn a_library_spawner_adds_rows_and_removes_none() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("fixture-ioc-rs");
        let spawns = format!(
            "{}fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture-ioc-rs\"\n",
            &[
                ("src/lib.rs", "pub fn nothing() {}\n"),
                ("tests/a.rs", &spawns),
                ("tests/b.rs", &spawns),
            ],
        );

        let before = spawners_in_crate(&crate_dir, &tokens());
        assert_eq!(labels(&before), ["fixture-ioc-rs::a", "fixture-ioc-rs::b"]);

        fs::write(crate_dir.join("src/lib.rs"), &spawns).unwrap();
        let after = spawners_in_crate(&crate_dir, &tokens());
        for row in &before {
            assert!(
                after.iter().any(|a| a.label() == row.label()),
                "`{}` vanished when the library became a spawner; after = {:?}",
                row.label(),
                labels(&after)
            );
        }
        assert!(
            after.len() > before.len(),
            "the library's own unit tests are a binary too; after = {:?}",
            labels(&after)
        );
    }

    /// A spawner moved out of `tests/common/` into a dev-dependency keeps its
    /// callers in the census: `mod` and `#[path]` no longer reach it, so only
    /// following the dev-dependency does. A binary that never names the crate
    /// links it and still spawns nothing, and naming it in a comment is not
    /// naming it.
    #[test]
    fn a_binary_naming_a_spawning_dev_dependency_is_a_spawner() {
        let tmp = tempfile::tempdir().unwrap();
        let spawns = format!(
            "{}pub fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        write_crate(
            &tmp.path().join("fixture-rig"),
            "[package]\nname = \"fixture-rig\"\n",
            &[("src/lib.rs", &spawns)],
        );
        let crate_dir = tmp.path().join("fixture-ca");
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture-ca\"\n\n\
             [dev-dependencies]\nfixture-rig = { path = \"../fixture-rig\" }\n",
            &[
                ("src/lib.rs", "pub fn nothing() {}\n"),
                ("tests/calls.rs", "#[test]\nfn t() { fixture_rig::go(); }\n"),
                ("tests/imports.rs", "use fixture_rig::go;\n"),
                ("tests/silent.rs", "#[test]\nfn t() {}\n"),
                (
                    "tests/mentions.rs",
                    "// fixture_rig::go() is not called here\n",
                ),
            ],
        );
        let rows = spawners_in_crate(&crate_dir, &tokens());
        assert_eq!(labels(&rows), ["fixture-ca::calls", "fixture-ca::imports"]);
        assert!(
            rows[0].evidence.starts_with("fixture-rig/src/lib.rs:"),
            "{:?}",
            rows[0].evidence
        );
    }

    /// `[dev-dependencies.<name>]` is the same dependency as the inline
    /// `name = { path = .. }`, spelled as its own table; the line matcher
    /// that preceded `toml` read its `path` line as a dependency named
    /// `path` and lost it.
    #[test]
    fn a_dev_dependency_table_is_followed_like_an_inline_one() {
        let tmp = tempfile::tempdir().unwrap();
        let spawns = format!(
            "{}pub fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        write_crate(
            &tmp.path().join("fixture-rig"),
            "[package]\nname = \"fixture-rig\"\n",
            &[("src/lib.rs", &spawns)],
        );
        let crate_dir = tmp.path().join("fixture-ca");
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture-ca\"\n\n\
             [dev-dependencies.fixture-rig]\npath = \"../fixture-rig\"\n",
            &[("tests/calls.rs", "#[test]\nfn t() { fixture_rig::go(); }\n")],
        );
        assert_eq!(
            labels(&spawners_in_crate(&crate_dir, &tokens())),
            ["fixture-ca::calls"]
        );
    }

    /// `name = { workspace = true }` is a path dependency too, by way of the
    /// workspace manifest.
    #[test]
    fn a_workspace_dev_dependency_is_followed_through_the_workspace_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let spawns = format!(
            "{}pub fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n\n\
             [workspace.dependencies]\nfixture-rig = { path = \"crates/fixture-rig\" }\n",
        )
        .unwrap();
        write_crate(
            &tmp.path().join("crates/fixture-rig"),
            "[package]\nname = \"fixture-rig\"\n",
            &[("src/lib.rs", &spawns)],
        );
        let crate_dir = tmp.path().join("crates/fixture-ca");
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture-ca\"\n\n\
             [dev-dependencies]\nfixture-rig = { workspace = true }\n",
            &[("tests/calls.rs", "#[test]\nfn t() { fixture_rig::go(); }\n")],
        );
        assert_eq!(
            labels(&spawners_in_crate(&crate_dir, &tokens())),
            ["fixture-ca::calls"]
        );
    }

    /// A bin target's `#[test]`s run as `package::bin/<name>`, which
    /// `binary(<name>)` selects — measured against nextest, not assumed. It is
    /// not part of the library's unit-test binary, and attributing it there
    /// asks for a `package()` declaration that would cap unrelated tests.
    #[test]
    fn a_bin_target_is_a_binary_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("fixture-ioc-rs");
        let spawns = format!(
            "{}fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture-ioc-rs\"\n",
            &[
                ("src/lib.rs", "pub fn nothing() {}\n"),
                ("src/bin/boot-ioc.rs", &spawns),
            ],
        );
        let rows = spawners_in_crate(&crate_dir, &tokens());
        assert_eq!(labels(&rows), ["fixture-ioc-rs::boot-ioc"]);
        assert!(
            rows[0].evidence.starts_with("src/bin/boot-ioc.rs:"),
            "{:?}",
            rows[0].evidence
        );
    }

    /// `[[bin]] name = "oracle-ioc"` with `path = "src/bin/oracle_ioc.rs"` is
    /// one target, not two: inferring a second from the file stem invents a
    /// binary cargo never builds and a declaration nobody can satisfy.
    #[test]
    fn an_explicit_bin_path_does_not_also_infer_the_file_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("fixture-ioc-rs");
        let spawns = format!(
            "{}fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture-ioc-rs\"\n\n\
             [[bin]]\nname = \"boot-ioc\"\npath = \"src/bin/boot_ioc.rs\"\n",
            &[("src/bin/boot_ioc.rs", &spawns)],
        );
        assert_eq!(
            labels(&spawners_in_crate(&crate_dir, &tokens())),
            ["fixture-ioc-rs::boot-ioc"]
        );
    }

    fn filters() -> Vec<Override> {
        overrides(
            "[[profile.default.overrides]]\n\
             filter = 'binary(gated_suite)'\n\
             test-group = 'pva_listener'\n\
             \n\
             [[profile.default.overrides]]\n\
             filter = 'binary(gated_suite) | binary(stress_load)'\n\
             test-group = 'ca_softioc'\n\
             \n\
             [[profile.default.overrides]]\n\
             filter = 'package(ad-plugins-rs) and test(file_magick::)'\n\
             test-group = 'ndarray_io'\n",
        )
    }

    /// nextest resolves overrides first-match-wins, so a binary named twice
    /// belongs to the earlier block. A gate that reported the later one would
    /// certify a cap that is not applied.
    #[test]
    fn the_first_matching_block_wins() {
        let blocks = filters();
        let b = effective_group(
            &blocks,
            "default",
            "epics-pva-rs",
            Some("gated_suite"),
            None,
        )
        .expect("evaluable")
        .expect("declared");
        assert_eq!(b.group, "pva_listener");
    }

    /// A block the gate cannot read, sitting where it cannot match, is not a
    /// reason to fail: `package(ad-plugins-rs) and test(...)` can never claim
    /// a binary in another package.
    #[test]
    fn an_unevaluable_block_that_cannot_match_shadows_nothing() {
        let blocks = filters();
        let b = effective_group(&blocks, "default", "epics-ca-rs", Some("stress_load"), None)
            .expect("evaluable")
            .expect("declared");
        assert_eq!(b.group, "ca_softioc");
    }

    /// But one that *could* match makes the answer unknowable rather than
    /// wrong, and the gate says so instead of guessing. `test(/re/)` is the
    /// real shape: a regex could name anything, so unlike a plain `test()`
    /// the gate cannot say it only narrows.
    #[test]
    fn an_unevaluable_block_that_could_match_is_reported_not_assumed() {
        let blocks = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'test(/boot_.*/)'\n\
             test-group = 'ndarray_io'\n\
             \n\
             [[profile.default.overrides]]\n\
             filter = 'binary(stress_load)'\n\
             test-group = 'ca_softioc'\n",
        );
        let r = effective_group(&blocks, "default", "epics-ca-rs", Some("stress_load"), None);
        assert!(r.is_err(), "{r:?}");
    }

    /// A `test()`-narrowed block is READ, not shrugged at. It can never be a
    /// binary's declaration — it claims some of the binary's tests, not the
    /// binary — so a spawner under one alone is undeclared, and the gate says
    /// undeclared rather than unknowable.
    #[test]
    fn a_test_narrowed_block_is_not_a_declaration() {
        let blocks = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'package(ad-plugins-rs) and test(file_magick::)'\n\
             test-group = 'ndarray_io'\n",
        );
        assert_eq!(
            evaluate(&blocks[0].terms, "ad-plugins-rs", Some("whatever"), None),
            Match::Partial
        );
        let r = effective_group(&blocks, "default", "ad-plugins-rs", Some("whatever"), None);
        assert!(matches!(r, Ok(None)), "{r:?}");
    }

    /// nextest resolves first-match-wins per TEST, so a narrowing block above
    /// a declaration pulls the tests it names into ITS group and leaves the
    /// rest in the declared one. That is a cap with a hole, and the whole
    /// reason `.config/nextest.toml` used to keep such a block last.
    #[test]
    fn a_narrowing_block_above_a_declaration_breaks_it_only_across_groups() {
        let split = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'binary(stress_load) and test(boots::)'\n\
             test-group = 'ndarray_io'\n\
             \n\
             [[profile.default.overrides]]\n\
             filter = 'binary(stress_load)'\n\
             test-group = 'ca_softioc'\n",
        );
        let r = effective_group(&split, "default", "epics-ca-rs", Some("stress_load"), None);
        assert!(r.is_err(), "{r:?}");

        // Narrowing WITHIN the declared group takes nothing out of it.
        let same = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'binary(stress_load) and test(boots::)'\n\
             test-group = 'ca_softioc'\n\
             \n\
             [[profile.default.overrides]]\n\
             filter = 'binary(stress_load)'\n\
             test-group = 'ca_softioc'\n",
        );
        let b = effective_group(&same, "default", "epics-ca-rs", Some("stress_load"), None)
            .expect("evaluable")
            .expect("declared");
        assert_eq!(b.group, "ca_softioc");
    }

    /// A spawner nothing claims is the whole point of the gate.
    #[test]
    fn a_binary_no_block_names_is_in_no_group() {
        let blocks = filters();
        let r = effective_group(
            &blocks,
            "default",
            "epics-ca-rs",
            Some("brand_new_ioc_test"),
            None,
        )
        .expect("evaluable");
        assert!(r.is_none());
    }

    /// A profile with no overrides of its own falls back to the default
    /// profile's — which is what caps a binary that runs only under
    /// `--profile interop` at all.
    #[test]
    fn a_profile_without_its_own_overrides_inherits_the_default_profile() {
        let blocks = filters();
        let b = effective_group(
            &blocks,
            "interop",
            "epics-pva-rs",
            Some("gated_suite"),
            None,
        )
        .expect("evaluable")
        .expect("declared");
        assert_eq!(b.group, "pva_listener");
    }

    /// The library's unit tests are not addressable by `binary()` — nextest
    /// reports `epics-oracle-rs` as the binary id but `binary(epics-oracle-rs)`
    /// matches nothing — so a spawner under `src/` must be covered by
    /// `package()`, and the gate models it as a binary-less spawner.
    #[test]
    fn a_src_spawner_needs_package_coverage_not_binary_coverage() {
        let blocks = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'binary(epics-oracle-rs)'\n\
             test-group = 'ca_softioc'\n",
        );
        assert!(
            effective_group(&blocks, "default", "epics-oracle-rs", None, None)
                .expect("evaluable")
                .is_none()
        );
        let blocks = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'package(epics-oracle-rs)'\n\
             test-group = 'ca_softioc'\n",
        );
        assert!(
            effective_group(&blocks, "default", "epics-oracle-rs", None, None)
                .expect("evaluable")
                .is_some()
        );
    }

    /// `binary(~substring)` is how a filter covers a family, and the gate has
    /// to agree with nextest about what it selects.
    #[test]
    fn a_substring_term_selects_by_substring() {
        let set = parse_filterset("binary(~_ioc_boots)");
        assert_eq!(
            evaluate(&set, "epics-ca-rs", Some("realtime_ca_ioc_boots"), None),
            Match::Yes
        );
        assert_eq!(
            evaluate(&set, "epics-ca-rs", Some("stress_load"), None),
            Match::No
        );
    }

    /// A multi-line `filter = """..."""` is normalised to one line, or every
    /// term after the first would parse as `Unknown` and shadow everything.
    #[test]
    fn a_triple_quoted_filter_is_read_as_one_expression() {
        let blocks = overrides(
            "[[profile.ci.overrides]]\n\
             filter = \"\"\"\n  binary(a) |\n  binary(b)\n\"\"\"\n\
             test-group = 'ca_softioc'\n",
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            evaluate(&blocks[0].terms, "whatever", Some("b"), None),
            Match::Yes
        );
    }

    /// A merged `tests/<dir>/main.rs` is one binary whose test modules are
    /// the units: each spawns through its own files, through a private
    /// `x/sub.rs`, or through a helper `main.rs` declares and it reaches by
    /// `crate::`. The helper, declaring no test, is no unit; a module that
    /// reaches nothing that spawns is no spawner.
    #[test]
    fn a_merged_binary_s_test_modules_are_its_units() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("fixture");
        let spawns = format!(
            "{}pub fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        let spawns_test = format!("#[test]\nfn t() {{}}\n{spawns}");
        write_crate(
            &crate_dir,
            "[package]\nname = \"fixture\"\n",
            &[
                (
                    "tests/integration/main.rs",
                    "mod helper;\nmod direct;\nmod via_path;\nmod via_group;\n\
                     mod nested;\nmod silent;\n#[path = \"deep/probe.rs\"] mod deep_probe;\n",
                ),
                ("tests/integration/helper.rs", &spawns),
                ("tests/integration/deep/probe.rs", &spawns),
                ("tests/integration/direct.rs", &spawns_test),
                (
                    "tests/integration/via_path.rs",
                    "#[tokio::test]\nasync fn t() { crate::helper::go(); }\n",
                ),
                (
                    "tests/integration/via_group.rs",
                    "use crate::{silent, deep_probe as p};\n#[test]\nfn t() { p::go(); }\n",
                ),
                (
                    "tests/integration/nested.rs",
                    "mod sub;\n#[test]\nfn t() {}\n",
                ),
                ("tests/integration/nested/sub.rs", &spawns),
                (
                    "tests/integration/silent.rs",
                    "// crate::helper::go() is not called here\n#[test]\nfn t() {}\n",
                ),
                ("tests/kept.rs", &spawns_test),
            ],
        );
        assert_eq!(
            labels(&spawners_in_crate(&crate_dir, &tokens())),
            [
                "fixture::integration direct::",
                "fixture::integration nested::",
                "fixture::integration via_group::",
                "fixture::integration via_path::",
                "fixture::kept",
            ]
        );
    }

    /// `binary(B) & test(/^m::/)` declares module `m` of a merged binary and
    /// no other, whatever the clause around it; a prefix inside `m` narrows
    /// it, and an exact name outside `m` takes nothing from it.
    #[test]
    fn a_module_prefix_declares_that_module_and_no_other() {
        let set = parse_filterset("(binary(integration) & test(/^boots/)) | binary(other)");
        let at = |binary, module| evaluate(&set, "p", Some(binary), module);
        assert_eq!(at("integration", Some("boots")), Match::Yes);
        assert_eq!(at("integration", Some("boots_twice")), Match::Yes);
        assert_eq!(at("integration", Some("silent")), Match::No);
        assert_eq!(at("other", None), Match::Yes);
        assert_eq!(at("third", None), Match::No);

        let deeper = parse_filterset("binary(integration) & test(/^boots::slow::/)");
        assert_eq!(
            evaluate(&deeper, "p", Some("integration"), Some("boots")),
            Match::Partial
        );
        let exact = parse_filterset("binary(integration) and test(=boots::t)");
        assert_eq!(
            evaluate(&exact, "p", Some("integration"), Some("boots")),
            Match::Partial
        );
        assert_eq!(
            evaluate(&exact, "p", Some("integration"), Some("silent")),
            Match::No
        );
    }

    /// `&` binds tighter than `|`, parentheses distribute, a negation is a
    /// term the gate cannot evaluate, and a filter it cannot read at all is
    /// one `Unknown` rather than a partial parse.
    #[test]
    fn a_filter_reads_ands_ors_and_parentheses() {
        let set = parse_filterset("package(p) & (binary(a) or binary(b)) | binary(c)");
        let at = |package, binary| evaluate(&set, package, Some(binary), None);
        assert_eq!(at("p", "a"), Match::Yes);
        assert_eq!(at("p", "b"), Match::Yes);
        assert_eq!(at("q", "a"), Match::No);
        assert_eq!(at("q", "c"), Match::Yes);

        let negated = parse_filterset("binary(a) - test(=t)");
        assert_eq!(evaluate(&negated, "p", Some("a"), None), Match::Maybe);
        assert_eq!(evaluate(&negated, "p", Some("b"), None), Match::No);

        let regex = parse_filterset("binary(a) & test(/^x::(y|z)$/)");
        assert_eq!(regex.len(), 1, "{regex:?}");
        assert_eq!(evaluate(&regex, "p", Some("a"), None), Match::Maybe);

        assert!(matches!(
            parse_filterset("binary(a").as_slice(),
            [clause] if matches!(clause.as_slice(), [Term::Unknown(_)])
        ));
    }

    /// A workspace with one merged binary whose `boots` module spawns
    /// through a helper and whose `silent` module does not, declared by
    /// `filter`; the `check` verdict.
    fn check_merged(filter: &str) -> Option<String> {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        let spawns = format!(
            "{}pub fn go() {{ let _ = {}(\"softIoc\"); }}\n",
            process_import(),
            spawn_call()
        );
        write_crate(
            &tmp.path().join("crates/fixture"),
            "[package]\nname = \"fixture\"\n",
            &[
                ("src/lib.rs", "pub fn nothing() {}\n"),
                (
                    "tests/integration/main.rs",
                    "mod helper;\nmod boots;\nmod silent;\n",
                ),
                ("tests/integration/helper.rs", &spawns),
                (
                    "tests/integration/boots.rs",
                    "use crate::helper;\n#[test]\nfn t() { helper::go(); }\n",
                ),
                ("tests/integration/silent.rs", "#[test]\nfn t() {}\n"),
            ],
        );
        fs::create_dir_all(tmp.path().join(".config")).unwrap();
        fs::write(
            tmp.path().join(".config/nextest.toml"),
            format!(
                "[[profile.default.overrides]]\nfilter = '{filter}'\n\
                 test-group = 'ca_softioc'\n\n[test-groups]\n\
                 ca_softioc = {{ max-threads = 1 }}\n\
                 ioc_unthrottled = {{ max-threads = 4 }}\n"
            ),
        )
        .unwrap();
        check(tmp.path())
    }

    /// The gate's three judgements on a merged binary: the spawning module
    /// is declared by its prefix or by the whole binary; a prefix naming no
    /// module is a dead name; a prefix naming a module that does not spawn
    /// is a declaration that lost its evidence.
    #[test]
    fn the_gate_judges_a_merged_binary_by_module() {
        assert_eq!(check_merged("binary(integration) & test(/^boots::/)"), None);
        assert_eq!(check_merged("binary(integration)"), None);

        let dead = check_merged("binary(integration) & test(/^gone::/)").unwrap();
        assert!(
            dead.contains("`test(/^gone::/)`, which is none of its modules"),
            "{dead}"
        );
        assert!(dead.contains("fixture::integration boots::"), "{dead}");

        let lost = check_merged("binary(integration) & test(/^silent::/)").unwrap();
        assert!(
            lost.contains("declares `binary(integration) & test(/^silent::/)` an IOC spawner"),
            "{lost}"
        );
    }
}
