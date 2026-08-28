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
/// target exists, without anybody remembering this file.
pub fn ioc_program_tokens(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert("softioc".to_string());
    for dir in workspace_crates(root) {
        for bin in bin_targets(&dir) {
            let lower = bin.name.to_ascii_lowercase();
            if lower.ends_with("ioc") || lower.ends_with("ioc-rs") || lower.ends_with("ioc_rs") {
                out.insert(lower);
            }
        }
    }
    out
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

/// Close one `[[bin]]` block, applying cargo's default path.
fn flush_bin(
    block: &mut Option<(Option<String>, Option<String>)>,
    out: &mut Vec<BinTarget>,
    seen: &mut BTreeSet<String>,
    claimed: &mut BTreeSet<PathBuf>,
) {
    let Some((name, path)) = block.take() else {
        return;
    };
    let Some(name) = name else { return };
    let path = PathBuf::from(path.unwrap_or_else(|| format!("src/bin/{name}.rs")));
    if seen.insert(name.clone()) {
        claimed.insert(path.clone());
        out.push(BinTarget { name, path });
    }
}

/// Every bin target of one crate: `[[bin]]` blocks, then the `src/bin/*.rs`
/// and `src/main.rs` cargo infers.
fn bin_targets(crate_dir: &Path) -> Vec<BinTarget> {
    let manifest = read(&crate_dir.join("Cargo.toml"));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    // Paths an explicit `[[bin]]` already owns. `epics-oracle-rs` names one
    // `oracle-ioc` with `path = "src/bin/oracle_ioc.rs"`; inferring a second
    // target from that file's stem would invent a binary cargo never builds.
    let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();
    let mut block: Option<(Option<String>, Option<String>)> = None;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            flush_bin(&mut block, &mut out, &mut seen, &mut claimed);
            if t == "[[bin]]" {
                block = Some((None, None));
            }
            continue;
        }
        let Some((name, path)) = block.as_mut() else {
            continue;
        };
        for (key, slot) in [("name", &mut *name), ("path", &mut *path)] {
            if let Some(rest) = t.strip_prefix(key)
                && let Some(rest) = rest.trim_start().strip_prefix('=')
            {
                *slot = Some(rest.trim().trim_matches(['"', '\'']).to_string());
            }
        }
    }
    flush_bin(&mut block, &mut out, &mut seen, &mut claimed);

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
/// names, `"... clears UDF at init (softIoc: UDF 0)"` is a sentence about one.
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
    literals.iter().any(|lit| {
        !lit.chars().any(char::is_whitespace) && {
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
    /// The file and line that made it a spawner.
    pub evidence: String,
}

impl Spawner {
    /// How the failure text names it.
    pub fn label(&self) -> String {
        match &self.binary {
            Some(b) => format!("{}::{b}", self.package),
            None => format!("{} (lib tests)", self.package),
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

/// The files a `tests/<name>.rs` root pulls in, following `mod` and `#[path]`.
///
/// A test binary's spawn sites are usually in a shared helper module —
/// `tests/common/mod.rs`, `tests/parity/interop.rs` — so scanning only the
/// root file would attribute nothing to the binary that actually spawns.
fn module_closure(root: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![root.to_path_buf()];
    let mut out = Vec::new();
    while let Some(file) = queue.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        out.push(file.clone());
        let Some(dir) = file.parent().map(Path::to_path_buf) else {
            continue;
        };
        let text = read(&file);
        let mut pending_path: Option<String> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("#[path") {
                if let Some(open) = rest.find('"') {
                    let tail = &rest[open + 1..];
                    if let Some(close) = tail.find('"') {
                        pending_path = Some(tail[..close].to_string());
                    }
                }
                continue;
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
            let name = name.trim();
            let candidates = match pending_path.take() {
                Some(p) => vec![dir.join(p)],
                None => vec![
                    dir.join(format!("{name}.rs")),
                    dir.join(name).join("mod.rs"),
                ],
            };
            for c in candidates {
                if c.is_file() {
                    queue.push(c);
                    break;
                }
            }
        }
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
        let rel = file.strip_prefix(crate_dir).unwrap_or(file);
        Some(format!("{}:{ev}", rel.display()))
    })
}

/// Every `tests/<name>.rs` root of one crate, sorted.
fn test_roots(crate_dir: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in dir_entries(&crate_dir.join("tests")) {
        if path.is_file() && path.extension().is_some_and(|e| e == "rs") {
            roots.push(path);
        }
    }
    roots.sort();
    roots
}

/// Walk one crate and report every nextest binary in it that spawns an IOC.
///
/// Three kinds of binary, enumerated separately because nextest addresses them
/// separately: the library's unit tests (`package()` only), each bin target's
/// unit tests (`binary(<bin name>)`, id `package::bin/<name>`), and each
/// `tests/<name>.rs` (`binary(<name>)`).
///
/// A spawn site in the *library* additionally makes every binary in the
/// package a spawner, because the library is what they all call:
/// `tests/oracle.rs` spawns three IOCs per case through
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

    // A bin's own modules belong to the bin, not to the library beside it.
    let bins = bin_targets(crate_dir);
    let mut bin_closures: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut owned_by_a_bin: BTreeSet<PathBuf> = BTreeSet::new();
    for bin in &bins {
        let root = crate_dir.join(&bin.path);
        if !root.is_file() {
            continue;
        }
        let mut closure = module_closure(&root);
        closure.sort();
        owned_by_a_bin.extend(closure.iter().cloned());
        bin_closures.push((bin.name.clone(), closure));
    }

    let mut lib_files = Vec::new();
    rust_files(&crate_dir.join("src"), &mut lib_files);
    lib_files.retain(|f| !owned_by_a_bin.contains(f));
    lib_files.sort();
    let lib = first_evidence(&lib_files, crate_dir, tokens);

    let through_lib = |ev: &str| format!("{ev} (through the {package} library this binary links)");

    // Keyed by the nextest binary, because a `binary()` term selects by name
    // and `epics-oracle-rs` has both a bin target and a `tests/oracle.rs`
    // called `oracle` — one name, one row, one declaration. First-hand
    // evidence wins over the library fallback; otherwise the first found.
    let mut rows: BTreeMap<Option<String>, (String, bool)> = BTreeMap::new();
    let mut record = |binary: Option<String>, evidence: Option<String>, via_lib: bool| {
        let Some(evidence) = evidence else { return };
        match rows.entry(binary) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((evidence, via_lib));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) if slot.get().1 && !via_lib => {
                slot.insert((evidence, via_lib));
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    };

    record(None, lib.clone(), false);
    for (name, closure) in &bin_closures {
        match first_evidence(closure, crate_dir, tokens) {
            Some(ev) => record(Some(name.clone()), Some(ev), false),
            None => record(Some(name.clone()), lib.as_deref().map(through_lib), true),
        }
    }
    for root in test_roots(crate_dir) {
        let binary = root
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut closure = module_closure(&root);
        closure.sort();
        match first_evidence(&closure, crate_dir, tokens) {
            Some(ev) => record(Some(binary), Some(ev), false),
            None => record(Some(binary), lib.as_deref().map(through_lib), true),
        }
    }

    rows.into_iter()
        .map(|(binary, (evidence, _))| Spawner {
            package: package.clone(),
            binary,
            evidence,
        })
        .collect()
}

fn package_name(manifest: &Path) -> Option<String> {
    let text = fs::read_to_string(manifest).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if in_package && let Some(rest) = t.strip_prefix("name") {
            let rest = rest.trim_start().strip_prefix('=')?.trim();
            return Some(rest.trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

/// Every workspace crate that has a `Cargo.toml`, under the member globs.
pub fn workspace_crates(root: &Path) -> Vec<PathBuf> {
    let manifest = root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));
    let list = text
        .split_once("members")
        .and_then(|(_, rest)| rest.split_once('['))
        .and_then(|(_, rest)| rest.split_once(']'))
        .expect("the workspace manifest declares a `members` array")
        .0;

    let mut out = Vec::new();
    for pattern in list.split(',') {
        let pattern = pattern.trim().trim_matches('"');
        if pattern.is_empty() || pattern.starts_with('#') {
            continue;
        }
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
/// each `tests/<name>.rs` and each bin target. A bin target's unit tests run as
/// `package::bin/<name>` and `binary(<name>)` selects them, so a declaration
/// naming one is live, not a leftover.
pub fn all_declarable_binaries(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for dir in workspace_crates(root) {
        for root in test_roots(&dir) {
            if let Some(stem) = root.file_stem() {
                out.insert(stem.to_string_lossy().into_owned());
            }
        }
        for bin in bin_targets(&dir) {
            out.insert(bin.name);
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
    /// The argument is carried for the message and is never matched: whatever
    /// it names, the block cannot be a whole binary's declaration, and that is
    /// the only thing this gate asks a filter.
    Test(String),
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
    fn selects(&self, package: &str, binary: Option<&str>) -> Match {
        let yes = |b: bool| if b { Match::Yes } else { Match::No };
        match self {
            Term::Package(p) => yes(p == package),
            Term::Binary(b) => yes(binary == Some(b.as_str())),
            Term::BinaryContains(s) => yes(binary.is_some_and(|b| b.contains(s.as_str()))),
            Term::Test(_) => Match::Partial,
            Term::Unknown(_) => Match::Maybe,
        }
    }
}

/// A filterset as a disjunction of conjunctions — `a and b | c` — which is all
/// the shape this config uses.
pub type Filterset = Vec<Vec<Term>>;

/// Parse `binary(a) | package(b) and test(c)` into disjunctive normal form.
pub fn parse_filterset(filter: &str) -> Filterset {
    filter
        .split('|')
        .map(|clause| {
            clause
                .split(" and ")
                .map(|raw| {
                    let t = raw.trim();
                    let Some((kind, arg)) = t.split_once('(') else {
                        return Term::Unknown(t.to_string());
                    };
                    let Some(arg) = arg.strip_suffix(')') else {
                        return Term::Unknown(t.to_string());
                    };
                    if arg.starts_with('/') {
                        return Term::Unknown(t.to_string());
                    }
                    match kind.trim() {
                        "package" => Term::Package(arg.to_string()),
                        "test" => Term::Test(arg.to_string()),
                        "binary" => match arg.strip_prefix('~') {
                            Some(sub) => Term::BinaryContains(sub.to_string()),
                            None => Term::Binary(arg.to_string()),
                        },
                        _ => Term::Unknown(t.to_string()),
                    }
                })
                .collect()
        })
        .collect()
}

/// Evaluate a whole filterset against one binary.
///
/// A conjunction is as weak as its weakest term and a disjunction as strong as
/// its strongest clause, over `No` < `Partial` < `Maybe` < `Yes`. `Maybe`
/// outranks `Partial` in both directions on purpose: an unreadable term might
/// claim the whole binary, so it can hide a declaration, while a `test()` term
/// is known to claim only part of one.
pub fn evaluate(set: &Filterset, package: &str, binary: Option<&str>) -> Match {
    let mut best = Match::No;
    for clause in set {
        let mut clause_verdict = Match::Yes;
        for term in clause {
            match term.selects(package, binary) {
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
) -> Result<Option<&'a Override>, String> {
    let mut shadow: Option<&Override> = None;
    let mut narrowed: Vec<&Override> = Vec::new();
    let chain = blocks.iter().filter(|b| b.profile == profile).chain(
        blocks
            .iter()
            .filter(|b| profile != "default" && b.profile == "default"),
    );
    for b in chain {
        match evaluate(&b.terms, package, binary) {
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
        for term in b.terms.iter().flatten() {
            if let Term::Binary(name) = term
                && !known.contains(name)
            {
                problems.push(format!(
                    "profile.{}'s `{}` filter names `binary({name})`, which is no tests/*.rs \
                     in the workspace — a dead name left by a rename or a deletion",
                    b.profile, b.group
                ));
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
    let spawner_binaries: BTreeSet<&str> = spawners
        .iter()
        .filter_map(|s| s.binary.as_deref())
        .collect();
    for b in &blocks {
        if !IOC_GROUPS.contains(&b.group.as_str()) {
            continue;
        }
        for term in b.terms.iter().flatten() {
            let missing = match term {
                Term::Binary(name) if !spawner_binaries.contains(name.as_str()) => {
                    Some(format!("binary({name})"))
                }
                Term::Package(name) if !spawner_packages.contains(name.as_str()) => {
                    Some(format!("package({name})"))
                }
                _ => None,
            };
            if let Some(what) = missing {
                problems.push(format!(
                    "profile.{}'s `{}` filter declares `{what}` an IOC spawner, but the scan \
                     finds no spawn site under it. Either the spawn site went away and the \
                     declaration should too, or the scan stopped recognising it and the \
                     forward check above is now blind",
                    b.profile, b.group
                ));
            }
        }
    }

    for s in &spawners {
        let mut landed: BTreeMap<&str, String> = BTreeMap::new();
        let mut undeclared: Vec<&str> = Vec::new();
        for profile in PROFILES {
            match effective_group(&blocks, profile, &s.package, s.binary.as_deref()) {
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

    /// No `use` item at all. `epics-oracle-rs/tests/oracle.rs` spells the path
    /// in full, and a module probe that demanded nothing precede `process`
    /// would refuse the commonest spelling there is.
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
        let b = effective_group(&blocks, "default", "epics-pva-rs", Some("gated_suite"))
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
        let b = effective_group(&blocks, "default", "epics-ca-rs", Some("stress_load"))
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
        let r = effective_group(&blocks, "default", "epics-ca-rs", Some("stress_load"));
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
            evaluate(&blocks[0].terms, "ad-plugins-rs", Some("whatever")),
            Match::Partial
        );
        let r = effective_group(&blocks, "default", "ad-plugins-rs", Some("whatever"));
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
        let r = effective_group(&split, "default", "epics-ca-rs", Some("stress_load"));
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
        let b = effective_group(&same, "default", "epics-ca-rs", Some("stress_load"))
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
        let b = effective_group(&blocks, "interop", "epics-pva-rs", Some("gated_suite"))
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
            effective_group(&blocks, "default", "epics-oracle-rs", None)
                .expect("evaluable")
                .is_none()
        );
        let blocks = overrides(
            "[[profile.default.overrides]]\n\
             filter = 'package(epics-oracle-rs)'\n\
             test-group = 'ca_softioc'\n",
        );
        assert!(
            effective_group(&blocks, "default", "epics-oracle-rs", None)
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
            evaluate(&set, "epics-ca-rs", Some("realtime_ca_ioc_boots")),
            Match::Yes
        );
        assert_eq!(
            evaluate(&set, "epics-ca-rs", Some("stress_load")),
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
            evaluate(&blocks[0].terms, "whatever", Some("b")),
            Match::Yes
        );
    }
}
