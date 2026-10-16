//! Every target declares the cfg its own imports require.
//!
//! # The rule this crate enforces
//!
//! A test, bench, example or bin target whose sources name a `cfg`-gated
//! module of its own crate, or an optional dependency of its own crate, MUST
//! itself be declared under a cfg that implies the thing's requirement --
//! either a file-level `#![cfg(...)]` or a `required-features` on its
//! manifest block.
//!
//! Undeclared, cargo compiles the target in configurations where the thing it
//! names does not exist. That is not a hypothetical: `--no-default-features
//! --all-targets` was red for `epics-ca-rs` and `epics-pva-rs` at 47 targets,
//! and `cargo nextest run --workspace` was green throughout, because the
//! default feature set supplied every module they named. The same shape took
//! three asyn-rs tests down under the empty feature selection
//! `scripts/rtems-check.sh` builds that crate with: 58 errors under a flag no
//! host row passes.
//!
//! The same holds for a binary the target runs: a bin that refuses to start
//! on one backend (it calls `runtime::task::refuse_backend` there, as
//! `softioc-rs` does on the exec backend) is a requirement of every target that
//! names it through `CARGO_BIN_EXE_<bin>` or passes its name to `Command::new`. That
//! dependency lives in a child process, so neither the compiler nor
//! `rtems-exec-gate`'s count of reactor-dependent test sites can see it.
//!
//! # Why a derivation and not a list
//!
//! `crates/asyn-rs/tests/epics_only_tests_declare_their_feature.rs` enforced
//! exactly this rule from a hand-written list of three needles, for one crate
//! and one feature. It is superseded by this crate because the list is the
//! part that rots: the workspace has 131 gated module paths and 98 optional
//! dependency declarations, and a needle list covers the ones somebody
//! thought of. Both halves here are read out of the sources -- the module
//! tree under `src/lib.rs` for the requirement, the target's own attributes
//! and manifest block for the declaration -- so a module gated tomorrow is
//! covered tomorrow.
//!
//! # The axes it reasons over
//!
//! Features, `epics_embedded_target`, and the backend cfgs `tokio_backend` /
//! `exec_backend`, which `build.rs` derives as `exec_backend = embedded ||
//! EPICS_RS_BUILD_EXEC_BACKEND=thread` and `tokio_backend = !exec_backend`. Those
//! are exactly the axes cargo and `scripts/rtems-check.sh` vary, so a gate on
//! one of them fails in a configuration no default row builds -- silently,
//! which is why this gate exists. Every other cfg atom (`unix`, `windows`,
//! `target_os`, a bespoke build-script cfg) is held true: a platform gate
//! fails loudly on the machine you are already on, and demanding a target
//! restate `any(unix, windows)` would be noise rather than a check.
//!
//! A target that has to define `main` -- a bin, an example, a `harness =
//! false` bench -- is checked on the feature axis alone. Not a carve-out but
//! the mechanism: cfg-ing such a file away leaves a crate with no `main` and
//! E0601 instead of a skipped target, so `required-features` is the only gate
//! it can carry, and `required-features` cannot name a build-script cfg.
//!
//! # What it does not see
//!
//! A path is what this gate can follow, so a `#[cfg]` on an *item* inside an
//! ungated module is invisible to it: `epics_pva_rs::server_native::PvaServer`
//! is ungated and its `client_config` method is not, and five `epics-pva-rs`
//! tests named it undeclared. Those were found by compiling every crate at
//! `--no-default-features --all-targets` until clean, which is the other half
//! of this rule and the half that is exhaustive. This gate is the half that
//! runs on every commit without a per-crate build matrix; neither closes the
//! family alone.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Printed at the top of every failure so the reader does not have to find
/// this file to learn what was breached.
pub const RULE: &str = "\
Every test, bench, example and bin target must declare the cfg its own \
imports require: a file-level `#![cfg(...)]`, or `required-features` on its \
manifest block, that implies the requirement of every gated module and \
optional dependency the target names, and of every binary it runs: a bin \
that calls `runtime::task::refuse_backend` under a cfg cannot run there, so a test spawning \
it there fails at startup (a target declared under that very cfg is pinning \
the refusal, and passes). Both sides are derived from the \
sources by tools/target-cfg-gate, so an unconditional import of a gated \
module is a red test on the commit that writes it, instead of a \
`--no-default-features` build somebody runs six months later. Run \
`cargo run -p target-cfg-gate --bin target-cfg-census` to see what the \
sources say.";

// ---------------------------------------------------------------------------
// cfg predicates
// ---------------------------------------------------------------------------

/// A `cfg(...)` predicate, parsed far enough to decide implication.
///
/// `Atom` keeps the source text of a leaf (`feature = "client-core"`,
/// `tokio_backend`, `unix`) with its whitespace normalised, because the atom
/// is also the identity: two spellings of one feature must compare equal or
/// the implication check invents a breach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pred {
    True,
    False,
    Atom(String),
    Not(Box<Pred>),
    All(Vec<Pred>),
    Any(Vec<Pred>),
}

impl Pred {
    /// Parse one predicate. Anything unrecognised becomes an `Atom` of its own
    /// text, which the axis projection then holds true -- an unparsed
    /// predicate must never manufacture a requirement.
    pub fn parse(src: &str) -> Pred {
        let s = src.trim();
        for kw in ["all", "any", "not"] {
            if let Some(inner) = strip_call(s, kw) {
                let subs: Vec<Pred> = split_top(inner).map(Pred::parse).collect();
                return match kw {
                    "all" => Pred::All(subs),
                    "any" => Pred::Any(subs),
                    _ => Pred::Not(Box::new(subs.into_iter().next().unwrap_or(Pred::True))),
                };
            }
        }
        if s.is_empty() {
            return Pred::True;
        }
        Pred::Atom(normalise_atom(s))
    }

    fn atoms(&self, out: &mut BTreeSet<String>) {
        match self {
            Pred::Atom(a) => {
                out.insert(a.clone());
            }
            Pred::Not(p) => p.atoms(out),
            Pred::All(v) | Pred::Any(v) => v.iter().for_each(|p| p.atoms(out)),
            Pred::True | Pred::False => {}
        }
    }

    fn eval(&self, env: &BTreeMap<String, bool>) -> bool {
        match self {
            Pred::True => true,
            Pred::False => false,
            Pred::Atom(a) => *env.get(a).unwrap_or(&false),
            Pred::Not(p) => !p.eval(env),
            Pred::All(v) => v.iter().all(|p| p.eval(env)),
            Pred::Any(v) => v.iter().any(|p| p.eval(env)),
        }
    }

    /// Render back to `cfg` syntax, for the failure text and the census.
    pub fn render(&self) -> String {
        match self {
            Pred::True => "true".into(),
            Pred::False => "false".into(),
            Pred::Atom(a) => a.clone(),
            Pred::Not(p) => format!("not({})", p.render()),
            Pred::All(v) => format!("all({})", join(v)),
            Pred::Any(v) => format!("any({})", join(v)),
        }
    }

    fn and(self, other: Pred) -> Pred {
        match (self, other) {
            (Pred::True, p) | (p, Pred::True) => p,
            (Pred::All(mut a), Pred::All(b)) => {
                a.extend(b);
                Pred::All(a)
            }
            (Pred::All(mut a), p) => {
                a.push(p);
                Pred::All(a)
            }
            (p, q) => Pred::All(vec![p, q]),
        }
    }
}

fn join(v: &[Pred]) -> String {
    v.iter().map(Pred::render).collect::<Vec<_>>().join(", ")
}

/// `kw(...)` -> the body, respecting nesting.
fn strip_call<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?.trim_start();
    let inner = rest.strip_prefix('(')?;
    let mut depth = 0i32;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => {
                return inner[i + 1..].trim().is_empty().then(|| &inner[..i]);
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Split a comma list at depth zero.
fn split_top(list: &str) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in list.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&list[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&list[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty())
}

/// `feature="x"` and `feature = "x"` are one atom.
fn normalise_atom(s: &str) -> String {
    match s.split_once('=') {
        Some((k, v)) => format!("{} = {}", k.trim(), v.trim()),
        None => s.trim().to_string(),
    }
}

/// Which cfg atoms the checker is allowed to reason from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Features, `epics_embedded_target`, and the two backend cfgs.
    Full,
    /// Features alone -- for a target that must define `main`.
    Features,
}

fn is_axis(atom: &str, axis: Axis) -> bool {
    if atom.starts_with("feature = ") {
        return true;
    }
    axis == Axis::Full
        && matches!(
            atom,
            "epics_embedded_target" | "tokio_backend" | "exec_backend"
        )
}

/// Hold every off-axis atom true and fold the result.
///
/// Folding rather than leaving `true` in place matters for the report: the
/// reader sees the requirement that is actually unmet, not the platform gate
/// that never was one.
pub fn project(p: &Pred, axis: Axis) -> Pred {
    match p {
        Pred::True | Pred::False => p.clone(),
        Pred::Atom(a) => {
            if is_axis(a, axis) {
                p.clone()
            } else {
                Pred::True
            }
        }
        Pred::Not(inner) => match project(inner, axis) {
            Pred::True => Pred::True,
            Pred::False => Pred::True,
            q => Pred::Not(Box::new(q)),
        },
        Pred::All(v) => {
            let subs: Vec<Pred> = v.iter().map(|q| project(q, axis)).collect();
            if subs.contains(&Pred::False) {
                return Pred::False;
            }
            let subs: Vec<Pred> = subs.into_iter().filter(|q| *q != Pred::True).collect();
            match subs.len() {
                0 => Pred::True,
                1 => subs.into_iter().next().unwrap(),
                _ => Pred::All(subs),
            }
        }
        Pred::Any(v) => {
            let subs: Vec<Pred> = v.iter().map(|q| project(q, axis)).collect();
            if subs.contains(&Pred::True) {
                return Pred::True;
            }
            let subs: Vec<Pred> = subs.into_iter().filter(|q| *q != Pred::False).collect();
            match subs.len() {
                0 => Pred::False,
                1 => subs.into_iter().next().unwrap(),
                _ => Pred::Any(subs),
            }
        }
    }
}

/// Replace the backend cfgs by the definition `build.rs` gives them.
///
/// `exec_backend = epics_embedded_target || EPICS_RS_BUILD_EXEC_BACKEND=thread`,
/// and `tokio_backend` is its negation. `derives_backend` is false for the
/// packages whose `build.rs` never emits the cfgs, where the second disjunct is
/// dead: in those, `tokio_backend` really does mean "not embedded", and a target
/// gated `not(epics_embedded_target)` satisfies a `tokio_backend` module.
/// Modelling the two as unrelated atoms would report that as a breach.
///
/// The env disjunct is deliberately spelled as an atom no cfg predicate in this
/// workspace can produce. While the backend was a cargo feature, a manifest
/// predicate naming that feature could discharge it; an environment variable
/// read at build time is invisible to every manifest, so nothing may.
fn desugar(p: &Pred, derives_backend: bool) -> Pred {
    let exec = || {
        if derives_backend {
            Pred::Any(vec![
                Pred::Atom("epics_embedded_target".into()),
                Pred::Atom(ENV_SELECTOR_ATOM.into()),
            ])
        } else {
            Pred::Atom("epics_embedded_target".into())
        }
    };
    match p {
        Pred::Atom(a) if a == "exec_backend" => exec(),
        Pred::Atom(a) if a == "tokio_backend" => Pred::Not(Box::new(exec())),
        Pred::Atom(_) | Pred::True | Pred::False => p.clone(),
        Pred::Not(q) => Pred::Not(Box::new(desugar(q, derives_backend))),
        Pred::All(v) => Pred::All(v.iter().map(|q| desugar(q, derives_backend)).collect()),
        Pred::Any(v) => Pred::Any(v.iter().map(|q| desugar(q, derives_backend)).collect()),
    }
}

/// The free atom standing for "this build asked for the reactor-free backend".
const ENV_SELECTOR_ATOM: &str = "env = \"EPICS_RS_BUILD_EXEC_BACKEND\"";

/// The transitive features one feature turns on, following bare names only.
///
/// `dep:x` and `x/y` items enable a dependency or a dependency's feature, not
/// a feature of this crate, so they are not edges here. `x?/y` is not one
/// either -- it enables nothing on its own.
fn feature_closure(seed: &str, feats: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![seed.to_string()];
    while let Some(f) = stack.pop() {
        for item in feats.get(&f).into_iter().flatten() {
            if item.contains('/') || item.starts_with("dep:") {
                continue;
            }
            if out.insert(item.clone()) {
                stack.push(item.clone());
            }
        }
    }
    out
}

/// Does `q` imply `p` over the axes, given this crate's feature graph?
///
/// Decided rather than approximated: the two predicates together name a
/// handful of atoms, so every assignment is enumerated and the ones cargo
/// cannot produce -- a feature on while a feature it enables is off -- are
/// dropped before the check. An approximation here would either invent
/// breaches on `any(...)` requirements or miss the `all(feature, backend)`
/// ones, which are the two shapes that actually occur.
pub fn implies(
    q: &Pred,
    p: &Pred,
    feats: &BTreeMap<String, Vec<String>>,
    derives_backend: bool,
) -> bool {
    let (q, p) = (desugar(q, derives_backend), desugar(p, derives_backend));
    let mut set = BTreeSet::new();
    q.atoms(&mut set);
    p.atoms(&mut set);
    let free: Vec<String> = set.into_iter().collect();
    // 2^20 assignments is a fifth of a second and no predicate in this
    // workspace comes near it; a wider one would be a design change worth
    // noticing rather than a slow test.
    assert!(
        free.len() <= 20,
        "{RULE}\n\ncfg predicate over {} atoms is too wide to decide: {} => {}",
        free.len(),
        q.render(),
        p.render()
    );
    for bits in 0u32..(1u32 << free.len()) {
        let env: BTreeMap<String, bool> = free
            .iter()
            .enumerate()
            .map(|(i, a)| (a.clone(), bits >> i & 1 == 1))
            .collect();
        if !feature_closed(&env, feats) {
            continue;
        }
        if q.eval(&env) && !p.eval(&env) {
            return false;
        }
    }
    true
}

/// Can cargo produce this assignment? Only if every enabled feature has the
/// features it enables enabled too.
fn feature_closed(env: &BTreeMap<String, bool>, feats: &BTreeMap<String, Vec<String>>) -> bool {
    for (atom, on) in env {
        if !on {
            continue;
        }
        let Some(name) = atom.strip_prefix("feature = ") else {
            continue;
        };
        let name = name.trim_matches('"');
        for implied in feature_closure(name, feats) {
            if env.get(&format!("feature = \"{implied}\"")) == Some(&false) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Reading the sources
// ---------------------------------------------------------------------------

/// Blank comments, and the inside of string literals outside attributes,
/// keeping every byte offset.
///
/// The literals matter as much as the comments and for a sharper reason: a
/// gate over source text that reads its own needles out of `&str` constants
/// reports itself, which is how the crate this one supersedes had to
/// special-case its own file name.
///
/// Attributes are the exception because their literals are the payload:
/// `#[cfg(feature = "tls")]` blanked to `#[cfg(feature = "   ")]` is a
/// requirement with no name, and every feature gate in the workspace reads as
/// the same one. Their braces never reach the depth counter -- `line_cfgs`
/// skips attribute lines before counting -- so keeping them costs nothing.
pub fn scrub(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut attr_depth = 0i32;
    for lexeme in lex(src) {
        let text = &src[lexeme.span.clone()];
        match lexeme.kind {
            // A literal inside an attribute is its payload; see above.
            Lexeme::Str if attr_depth > 0 => out.push_str(text),
            Lexeme::Str | Lexeme::Char | Lexeme::Comment => blank(&mut out, text),
            Lexeme::Code => {
                let b = text.as_bytes();
                for (k, &c) in b.iter().enumerate() {
                    let opens = c == b'['
                        && (attr_depth > 0
                            || src[..lexeme.span.start + k].ends_with('#')
                            || src[..lexeme.span.start + k].ends_with("#!"));
                    if opens {
                        attr_depth += 1;
                    } else if c == b']' && attr_depth > 0 {
                        attr_depth -= 1;
                    }
                }
                out.push_str(text);
            }
        }
    }
    out
}

/// What [`lex`] cuts source text into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lexeme {
    Code,
    Comment,
    /// A string literal, raw or not, with any `b`/`c` prefix outside it.
    Str,
    /// A char or byte literal: `'"'`, `b'"'`, `'\''`, `'\u{7f}'`.
    Char,
}

struct Lexed {
    kind: Lexeme,
    /// The whole lexeme, quotes and hash fences included.
    span: std::ops::Range<usize>,
    /// For a [`Lexeme::Str`], its contents between the quotes.
    body: std::ops::Range<usize>,
}

/// The one reading of Rust's comment, string and char syntax that [`scrub`]
/// and [`literals`] share, so they cannot disagree about where a literal ends.
fn lex(src: &str) -> Vec<Lexed> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut code = 0usize;
    let mut i = 0usize;
    let push =
        |out: &mut Vec<Lexed>, code: &mut usize, kind, span: std::ops::Range<usize>, body| {
            if *code < span.start {
                out.push(Lexed {
                    kind: Lexeme::Code,
                    span: *code..span.start,
                    body: 0..0,
                });
            }
            *code = span.end;
            out.push(Lexed { kind, span, body });
        };
    while i < b.len() {
        // A quote after an identifier closes nothing it opened: `x'` is not
        // Rust, but the `b` of `b'"'` is a prefix, not an identifier.
        let after_ident =
            i > 0 && is_ident(b[i - 1]) && !(b[i - 1] == b'b' && (i < 2 || !is_ident(b[i - 2])));
        if src[i..].starts_with("//") {
            let end = src[i..].find('\n').map_or(b.len(), |k| i + k);
            push(&mut out, &mut code, Lexeme::Comment, i..end, 0..0);
            i = end;
        } else if src[i..].starts_with("/*") {
            let end = src[i + 2..].find("*/").map_or(b.len(), |k| i + 2 + k + 2);
            push(&mut out, &mut code, Lexeme::Comment, i..end, 0..0);
            i = end;
        } else if b[i] == b'r' && raw_prefix_ok(b, i) {
            // r"..", r#".."#, br#".."#, cr#".."#: no escapes, a fence of hashes.
            let mut j = i + 1;
            while j < b.len() && b[j] == b'#' {
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                let close = format!("\"{}", "#".repeat(j - i - 1));
                let body_end = src[j + 1..].find(&close).map_or(b.len(), |k| j + 1 + k);
                let end = (body_end + close.len()).min(b.len());
                push(&mut out, &mut code, Lexeme::Str, i..end, j + 1..body_end);
                i = end;
            } else {
                i = j.max(i + 1);
            }
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let body_end = j.min(b.len());
            let end = (j + 1).min(b.len());
            push(&mut out, &mut code, Lexeme::Str, i..end, i + 1..body_end);
            i = end;
        } else if b[i] == b'\'' && !after_ident {
            // A char literal is one char or one escape between quotes; anything
            // else after a quote is a lifetime or a label.
            let close = if b.get(i + 1) == Some(&b'\\') {
                src[i + 2..]
                    .find('\'')
                    .map(|k| i + 2 + k)
                    .filter(|&c| c > i + 2)
            } else {
                src[i + 1..]
                    .chars()
                    .next()
                    .map(|c| i + 1 + c.len_utf8())
                    .filter(|&c| b.get(c) == Some(&b'\''))
            };
            match close {
                Some(c) => {
                    push(&mut out, &mut code, Lexeme::Char, i..c + 1, 0..0);
                    i = c + 1;
                }
                None => i += 1,
            }
        } else {
            i += src[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if code < b.len() {
        out.push(Lexed {
            kind: Lexeme::Code,
            span: code..b.len(),
            body: 0..0,
        });
    }
    out
}

/// Whether the `r` at `i` can open a raw string: it starts a token, or follows
/// a `b`/`c` prefix that does.
fn raw_prefix_ok(b: &[u8], i: usize) -> bool {
    match i.checked_sub(1).map(|k| b[k]) {
        None => true,
        Some(b'b' | b'c') => i < 2 || !is_ident(b[i - 2]),
        Some(c) => !is_ident(c),
    }
}

/// Every string literal outside comments, with the offset of its opening
/// quote (or its `r`): what `CARGO_BIN_EXE_<bin>` and a spawn by name are
/// written in, and what [`scrub`] blanks.
pub fn literals(src: &str) -> Vec<(usize, String)> {
    lex(src)
        .into_iter()
        .filter(|l| l.kind == Lexeme::Str)
        .map(|l| (l.span.start, src[l.body].to_string()))
        .collect()
}

/// Every binary `src` runs, with the offset it is named at: the bin of a
/// `CARGO_BIN_EXE_<bin>` literal, and the file name of a literal passed to
/// `Command::new`. Any other string that happens to equal a bin's name is
/// not a spawn.
pub fn binaries_run(src: &str) -> Vec<(String, usize)> {
    literals(src)
        .into_iter()
        .filter_map(|(off, lit)| {
            let named = match lit.strip_prefix("CARGO_BIN_EXE_") {
                Some(bin) => bin,
                None if src[..off].trim_end().ends_with("Command::new(") => {
                    lit.rsplit('/').next().unwrap_or(&lit)
                }
                None => return None,
            };
            Some((named.to_string(), off))
        })
        .collect()
}

/// The cfg under which a bin refuses to start: the cfg in force at its call
/// of `runtime::task::refuse_backend`, the one owner of that refusal (any of
/// them, if it has several).
pub fn refused_under(src: &str) -> Option<Pred> {
    const CALL: &str = "refuse_backend(";
    let text = scrub(src);
    let cfgs = line_cfgs(&text, &Pred::True);
    let starts = line_index(&text);
    let arms: Vec<Pred> = text
        .match_indices(CALL)
        .filter(|&(at, _)| {
            let before = text[..at].trim_end();
            !before.ends_with("fn") && !text[..at].ends_with(|c: char| is_ident(c as u8))
        })
        .map(|(at, _)| {
            let line = starts.partition_point(|&s| s <= at) - 1;
            cfgs.get(line).cloned().unwrap_or(Pred::True)
        })
        .filter(|arm| *arm != Pred::True)
        .collect();
    match arms.len() {
        0 => None,
        1 => arms.into_iter().next(),
        _ => Some(Pred::Any(arms)),
    }
}

/// What a bin refusing under `arm` needs of a target that runs it.
fn runnable_under(arm: Pred) -> Pred {
    match arm {
        // `build.rs` defines one as the other's negation; say it the way the
        // targets are gated.
        Pred::Atom(a) if a == "exec_backend" => Pred::Atom("tokio_backend".into()),
        arm => Pred::Not(Box::new(arm)),
    }
}

/// Replace a span by spaces, keeping its newlines so line numbers survive.
fn blank(out: &mut String, span: &str) {
    for c in span.chars() {
        if c == '\n' {
            out.push('\n');
        } else {
            // One space per byte, so offsets into the scrubbed text are
            // offsets into the source.
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
        }
    }
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Every path this text names below `crate_snake`, with `use` trees expanded,
/// each paired with the byte offset it was named at.
///
/// `use epics_ca_rs::{calink::CaLinkResolver, client::CaClient}` names two
/// modules, and a check that matched `epics_ca_rs::calink` as a substring
/// would see neither. The same walk handles an inline
/// `epics_ca_rs::cli::main()`, so there is one path expander and not one per
/// syntactic position. The offset is what lets a reference under an inner
/// `#[cfg]` be judged against that cfg rather than against the file's.
pub fn refs_below(text: &str, crate_snake: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut from = 0usize;
    while let Some(hit) = text[from..].find(crate_snake) {
        let at = from + hit;
        from = at + crate_snake.len();
        if at > 0 && is_ident(b[at - 1]) {
            continue;
        }
        let mut j = from;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if !text[j..].starts_with("::") {
            continue;
        }
        walk_tree(text, at, j + 2, &[], &mut out);
    }
    out
}

/// One node of a use tree: `{a, b::c}`, `a::b`, `a as x`, `*`.
fn walk_tree(
    text: &str,
    origin: usize,
    at: usize,
    prefix: &[String],
    out: &mut Vec<(usize, String)>,
) {
    let b = text.as_bytes();
    let mut i = at;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= b.len() {
        return;
    }
    if b[i] == b'{' {
        let mut depth = 0i32;
        let (mut start, mut j) = (i + 1, i + 1);
        let mut parts = Vec::new();
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' if depth == 0 => {
                    parts.push(start..j);
                    break;
                }
                b'}' => depth -= 1,
                b',' if depth == 0 => {
                    parts.push(start..j);
                    start = j + 1;
                }
                _ => {}
            }
            j += 1;
        }
        for part in parts {
            let seg = &text[part.clone()];
            let trimmed = seg.trim_start();
            let off = part.start + (seg.len() - trimmed.len());
            walk_tree(text, origin, off, prefix, out);
        }
        return;
    }
    let start = i;
    while i < b.len() && is_ident(b[i]) {
        i += 1;
    }
    if i == start {
        // `*` or something unparsed: the glob names the prefix itself.
        if !prefix.is_empty() {
            out.push((origin, prefix.join("::")));
        }
        return;
    }
    let mut next: Vec<String> = prefix.to_vec();
    next.push(text[start..i].to_string());
    let mut k = i;
    while k < b.len() && b[k].is_ascii_whitespace() {
        k += 1;
    }
    if text[k..].starts_with("::") {
        walk_tree(text, origin, k + 2, &next, out);
    } else {
        out.push((origin, next.join("::")));
    }
}

/// The cfg in force at the start of each line of one file.
///
/// Indexed by line number, so a reference's byte offset maps to the predicate
/// that actually guards it. Without this an `#[cfg(all(unix, feature =
/// "tls"))] mod tls_interop;` reads as an unconditional `rustls` import: 27
/// test files in this workspace carry an inner `#[cfg]`, and every one of
/// them would be a false breach that a developer closes by declaring a
/// requirement the target does not have.
pub fn line_cfgs(text: &str, base: &Pred) -> Vec<Pred> {
    let mut out = Vec::new();
    let mut stack: Vec<(i32, Pred)> = Vec::new();
    let mut pending: Vec<Pred> = Vec::new();
    let mut attr = String::new();
    let mut depth = 0i32;
    for line in text.lines() {
        let trimmed = line.trim();
        let mut effective = base.clone();
        for (_, p) in &stack {
            effective = effective.and(p.clone());
        }
        // An attribute in flight, or one starting here.
        if !attr.is_empty() || (trimmed.starts_with("#[") && !trimmed.starts_with("#![")) {
            attr.push_str(trimmed);
            if brackets_balanced(&attr) {
                if let Some(body) = attr
                    .strip_prefix("#[")
                    .and_then(|a| a.strip_suffix(']'))
                    .and_then(|a| strip_call(a.trim(), "cfg"))
                {
                    pending.push(Pred::parse(body));
                }
                attr.clear();
            }
            out.push(effective);
            continue;
        }
        for p in &pending {
            effective = effective.and(p.clone());
        }
        out.push(effective.clone());
        if trimmed.is_empty() || trimmed.starts_with("#![") {
            continue;
        }
        let before = depth;
        depth += brace_delta(line);
        if depth > before && !pending.is_empty() {
            let mut p = Pred::True;
            for q in pending.drain(..) {
                p = p.and(q);
            }
            stack.push((before, p));
        }
        pending.clear();
        while stack.last().is_some_and(|(d, _)| depth <= *d) {
            stack.pop();
        }
    }
    out
}

fn brackets_balanced(s: &str) -> bool {
    let (mut sq, mut par) = (0i32, 0i32);
    for c in s.chars() {
        match c {
            '[' => sq += 1,
            ']' => sq -= 1,
            '(' => par += 1,
            ')' => par -= 1,
            _ => {}
        }
    }
    sq == 0 && par == 0
}

fn brace_delta(line: &str) -> i32 {
    line.chars()
        .map(|c| match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        })
        .sum()
}

/// Byte offset -> line number, for the offsets `refs_below` reports.
fn line_index(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, c) in text.char_indices() {
        if c == '\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// One `mod` declaration and the attributes attached to it.
struct ModDecl {
    cfg: Pred,
    path: Option<String>,
    name: String,
}

/// The `mod name;` declarations of one file, with their `#[cfg]` and `#[path]`.
///
/// Inline `mod name { .. }` is deliberately not collected: every one in this
/// workspace's library roots is a `#[cfg(test)] mod tests`, which no target
/// can name, and descending into one would report a private module as a
/// public requirement.
fn mod_decls(text: &str) -> Vec<ModDecl> {
    let mut out = Vec::new();
    let mut cfgs: Vec<Pred> = Vec::new();
    let mut path: Option<String> = None;
    let mut attr = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !attr.is_empty() || (trimmed.starts_with("#[") && !trimmed.starts_with("#![")) {
            attr.push_str(trimmed);
            if brackets_balanced(&attr) {
                let body = attr
                    .strip_prefix("#[")
                    .and_then(|a| a.strip_suffix(']'))
                    .unwrap_or("");
                if let Some(inner) = strip_call(body.trim(), "cfg") {
                    cfgs.push(Pred::parse(inner));
                } else if let Some(rest) = body.trim().strip_prefix("path") {
                    path = rest
                        .trim_start()
                        .strip_prefix('=')
                        .map(|v| v.trim().trim_matches('"').to_string());
                }
                attr.clear();
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if let Some(name) = mod_name(trimmed) {
            let mut cfg = Pred::True;
            for c in cfgs.drain(..) {
                cfg = cfg.and(c);
            }
            out.push(ModDecl {
                cfg,
                path: path.take(),
                name,
            });
        } else {
            cfgs.clear();
            path = None;
        }
    }
    out
}

/// `pub mod x;` / `pub(crate) mod x;` / `mod x;` -> `x`.
fn mod_name(trimmed: &str) -> Option<String> {
    let rest = trimmed
        .strip_prefix("pub ")
        .or_else(|| {
            trimmed
                .strip_prefix("pub(")
                .and_then(|r| r.split_once(')'))
                .map(|(_, r)| r.trim_start())
        })
        .unwrap_or(trimmed);
    let rest = rest.trim_start().strip_prefix("mod ")?;
    let name = rest.trim().strip_suffix(';')?.trim();
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then(|| name.to_string())
}

/// Where a submodule of `file` lives.
fn child_paths(file: &Path, decl: &ModDecl) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new("."));
    if let Some(p) = &decl.path {
        return vec![dir.join(p)];
    }
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let sub = if matches!(stem, "lib" | "mod" | "main") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    vec![
        sub.join(format!("{}.rs", decl.name)),
        sub.join(&decl.name).join("mod.rs"),
    ]
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Inner `#![cfg(..)]` attributes of a file, conjoined.
fn file_gate(text: &str) -> Pred {
    let mut p = Pred::True;
    for line in text.lines() {
        let t = line.trim();
        if let Some(body) = t
            .strip_prefix("#![")
            .and_then(|a| a.strip_suffix(']'))
            .and_then(|a| strip_call(a.trim(), "cfg"))
        {
            p = p.and(Pred::parse(body));
        }
    }
    p
}

// ---------------------------------------------------------------------------
// Reading the manifests
// ---------------------------------------------------------------------------

/// Drop a `#` comment, respecting quotes -- a dependency's `description`
/// may hold one.
fn strip_toml_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"') | (None, '\'') => quote = Some(c),
            (Some(q), c) if q == c => quote = None,
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

/// `[header]` / `[[header]]` -> body, in source order.
fn sections(manifest: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur = (String::new(), String::new());
    for line in manifest.lines() {
        let t = strip_toml_comment(line).trim();
        if t.starts_with('[') && t.ends_with(']') {
            out.push(std::mem::take(&mut cur));
            cur.0 = t.trim_matches(['[', ']']).to_string();
        } else {
            cur.1.push_str(line);
            cur.1.push('\n');
        }
    }
    out.push(cur);
    out.into_iter().filter(|(h, _)| !h.is_empty()).collect()
}

/// `key = [ ... ]`, tolerating the array spanning lines.
fn toml_arrays(body: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in body.lines() {
        let t = strip_toml_comment(line);
        if buf.is_empty() && !t.contains('=') {
            continue;
        }
        buf.push(' ');
        buf.push_str(t.trim());
        if buf.matches('[').count() == 0 {
            buf.clear();
            continue;
        }
        if buf.matches('[').count() > buf.matches(']').count() {
            continue;
        }
        if let Some((k, v)) = buf.split_once('=') {
            let items = v
                .trim()
                .trim_matches(['[', ']'])
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            out.push((k.trim().to_string(), items));
        }
        buf.clear();
    }
    out
}

/// `name = "1.0"` / `name = { .. }` -> `(name, is_optional)`, tolerating a
/// table spanning lines.
fn toml_deps(body: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in body.lines() {
        let t = strip_toml_comment(line);
        if buf.is_empty() && !t.contains('=') {
            continue;
        }
        buf.push(' ');
        buf.push_str(t.trim());
        if buf.matches('{').count() > buf.matches('}').count() {
            continue;
        }
        if let Some((k, v)) = buf.split_once('=') {
            let optional = v
                .split_once("optional")
                .and_then(|(_, r)| r.split_once('='))
                .is_some_and(|(_, r)| r.trim_start().starts_with("true"));
            out.push((k.trim().to_string(), optional));
        }
        buf.clear();
    }
    out
}

/// One compiled target of a crate.
pub struct TargetDecl {
    pub kind: &'static str,
    pub name: String,
    pub rel_path: PathBuf,
    pub required_features: Vec<String>,
    /// A bin, an example, or a `harness = false` bench or test: cargo does not
    /// generate its `main`, so cfg-ing the file away is E0601 and
    /// `required-features` is the only gate it can carry.
    pub main_defining: bool,
}

/// Everything about one crate the rule needs.
pub struct CrateInfo {
    pub name: String,
    pub dir: PathBuf,
    pub features: BTreeMap<String, Vec<String>>,
    /// Optional dependency -> the `[target.'cfg(..)']` it was declared under.
    pub optional_deps: BTreeMap<String, Pred>,
    pub dev_nonoptional: BTreeSet<String>,
    pub targets: Vec<TargetDecl>,
    /// Module path below the crate root -> the cfg it is declared under.
    pub gated: BTreeMap<String, Pred>,
    /// Whether this crate's `build.rs` derives the backend cfgs at all. Read
    /// from the build script rather than the manifest because the backend is
    /// no longer a feature; `rtems-exec-gate` owns that predicate.
    pub derives_backend: bool,
}

const TARGET_DIRS: [(&str, &str); 4] = [
    ("test", "tests"),
    ("bench", "benches"),
    ("example", "examples"),
    ("bin", "src/bin"),
];

fn read_crate(dir: &Path) -> Option<CrateInfo> {
    let manifest_path = dir.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).ok()?;
    let secs = sections(&manifest);
    let name = secs
        .iter()
        .find(|(h, _)| h == "package")
        .and_then(|(_, b)| {
            b.lines().map(strip_toml_comment).find_map(|l| {
                l.trim()
                    .strip_prefix("name")?
                    .split_once('=')
                    .map(|(_, v)| v.trim().trim_matches('"').to_string())
            })
        })?;

    let mut features = BTreeMap::new();
    let mut optional_deps = BTreeMap::new();
    let mut dev_nonoptional = BTreeSet::new();
    let mut targets: Vec<TargetDecl> = Vec::new();
    let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();

    for (header, body) in &secs {
        if header == "features" {
            features.extend(toml_arrays(body));
            continue;
        }
        if let Some((kind, target_cfg)) = dep_section(header) {
            for (dep, optional) in toml_deps(body) {
                if optional {
                    optional_deps.insert(dep, target_cfg.clone());
                } else if kind == "dev-dependencies" {
                    dev_nonoptional.insert(dep);
                }
            }
            continue;
        }
        let Some(&(kind, dir_name)) = TARGET_DIRS.iter().find(|(k, _)| *k == header) else {
            continue;
        };
        let mut tname = None;
        let mut tpath = None;
        let mut harness = true;
        for line in body.lines().map(strip_toml_comment) {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("name").and_then(|r| r.split_once('=')) {
                tname = Some(v.1.trim().trim_matches('"').to_string());
            } else if let Some(v) = t.strip_prefix("path").and_then(|r| r.split_once('=')) {
                tpath = Some(v.1.trim().trim_matches('"').to_string());
            } else if t.starts_with("harness") && t.contains("false") {
                harness = false;
            }
        }
        let Some(tname) = tname else { continue };
        let required_features = toml_arrays(body)
            .into_iter()
            .find(|(k, _)| k == "required-features")
            .map(|(_, v)| v)
            .unwrap_or_default();
        let rel_path = PathBuf::from(tpath.unwrap_or_else(|| format!("{dir_name}/{tname}.rs")));
        claimed.insert(rel_path.clone());
        targets.push(TargetDecl {
            kind,
            name: tname,
            rel_path,
            required_features,
            main_defining: kind == "bin" || kind == "example" || !harness,
        });
    }

    // Auto-discovery, for every target cargo compiles without a block.
    for (kind, dir_name) in TARGET_DIRS {
        let Ok(entries) = std::fs::read_dir(dir.join(dir_name)) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .collect();
        found.sort();
        for path in found {
            let rel = PathBuf::from(dir_name).join(path.file_name().unwrap());
            if claimed.contains(&rel) {
                continue;
            }
            targets.push(TargetDecl {
                kind,
                name: path.file_stem().unwrap().to_string_lossy().into_owned(),
                rel_path: rel,
                required_features: Vec::new(),
                main_defining: kind == "bin" || kind == "example",
            });
        }
    }
    if dir.join("src/main.rs").is_file() && !targets.iter().any(|t| t.kind == "bin") {
        targets.push(TargetDecl {
            kind: "bin",
            name: name.clone(),
            rel_path: PathBuf::from("src/main.rs"),
            required_features: Vec::new(),
            main_defining: true,
        });
    }

    let derives_backend = rtems_exec_gate::derives_the_backend(&dir.join("build.rs"));
    let gated = gated_modules(&dir.join("src/lib.rs"));
    Some(CrateInfo {
        name,
        dir: dir.to_path_buf(),
        features,
        optional_deps,
        dev_nonoptional,
        targets,
        gated,
        derives_backend,
    })
}

/// `[dependencies]` / `[target.'cfg(..)'.dev-dependencies]` -> kind and cfg.
fn dep_section(header: &str) -> Option<(&'static str, Pred)> {
    let kinds = ["dependencies", "dev-dependencies", "build-dependencies"];
    for k in kinds {
        if header == k {
            let kind = if k == "dev-dependencies" {
                "dev-dependencies"
            } else {
                "dependencies"
            };
            return Some((kind, Pred::True));
        }
        if let Some(spec) = header
            .strip_prefix("target.")
            .and_then(|r| r.strip_suffix(&format!(".{k}")))
        {
            {
                let cfg = spec.trim_matches(['\'', '"']);
                let pred = strip_call(cfg, "cfg")
                    .map(Pred::parse)
                    .unwrap_or(Pred::True);
                let kind = if k == "dev-dependencies" {
                    "dev-dependencies"
                } else {
                    "dependencies"
                };
                return Some((kind, pred));
            }
        }
    }
    None
}

/// Every module path below the crate root, with the cfg it is declared under.
///
/// A path declared more than once -- the platform alternation shape, one
/// `mod` per target -- carries the disjunction, because the item exists in
/// every configuration some declaration covers.
fn gated_modules(lib: &Path) -> BTreeMap<String, Pred> {
    let mut acc: BTreeMap<String, Vec<Pred>> = BTreeMap::new();
    walk_modules(lib, &[], &Pred::True, &mut acc, 0);
    acc.into_iter()
        .filter_map(|(path, preds)| {
            if preds.contains(&Pred::True) {
                return None; // an ungated declaration means no requirement
            }
            let pred = if preds.len() == 1 {
                preds.into_iter().next().unwrap()
            } else {
                Pred::Any(preds)
            };
            Some((path, pred))
        })
        .collect()
}

fn walk_modules(
    file: &Path,
    prefix: &[String],
    acc: &Pred,
    out: &mut BTreeMap<String, Vec<Pred>>,
    depth: usize,
) {
    if depth > 8 || !file.is_file() {
        return;
    }
    let text = scrub(&read(file));
    for decl in mod_decls(&text) {
        let pred = acc.clone().and(decl.cfg.clone());
        let mut path = prefix.to_vec();
        path.push(decl.name.clone());
        out.entry(path.join("::")).or_default().push(pred.clone());
        for cand in child_paths(file, &decl) {
            if cand.is_file() {
                walk_modules(&cand, &path, &pred, out, depth + 1);
                break;
            }
        }
    }
}

/// Features that turn an optional dependency on.
///
/// `dep:x` and a bare `x` both do; `x/feat` does too, because naming a
/// dependency's feature enables the dependency. `x?/feat` does not, which is
/// why the `?` form is excluded rather than trimmed. When no `dep:x` appears
/// anywhere, cargo also synthesises a feature named `x`.
fn dep_enablers(dep: &str, features: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut direct = BTreeSet::new();
    let mut uses_dep_syntax = false;
    for (feat, items) in features {
        for item in items {
            if item == &format!("dep:{dep}") {
                uses_dep_syntax = true;
                direct.insert(feat.clone());
            } else if item == dep || item.starts_with(&format!("{dep}/")) {
                direct.insert(feat.clone());
            }
        }
    }
    loop {
        let mut grew = false;
        for (feat, items) in features {
            if direct.contains(feat) {
                continue;
            }
            if items
                .iter()
                .any(|i| !i.contains('/') && !i.starts_with("dep:") && direct.contains(i))
            {
                direct.insert(feat.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    if !uses_dep_syntax {
        direct.insert(dep.to_string());
    }
    direct
}

// ---------------------------------------------------------------------------
// The audit
// ---------------------------------------------------------------------------

/// One (target file, gated thing it names) pair, judged.
pub struct Pair {
    pub crate_name: String,
    pub target: String,
    /// Relative to the crate directory -- a `#[path]` submodule reports
    /// itself, not the target root, because that is the file to edit.
    pub file: String,
    /// `module`, `optional dep` or `binary`.
    pub shape: &'static str,
    pub named: String,
    pub needs: String,
    pub has: String,
    pub satisfied: bool,
}

/// What the sources say about the whole workspace.
pub struct Report {
    pub crates_scanned: usize,
    pub pairs: Vec<Pair>,
}

impl Report {
    pub fn breaches(&self) -> impl Iterator<Item = &Pair> {
        self.pairs.iter().filter(|p| !p.satisfied)
    }

    /// Every pair, breach or not -- what `target-cfg-census` prints.
    pub fn census(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} crates, {} (target file, gated thing) pairs, {} breaches\n",
            self.crates_scanned,
            self.pairs.len(),
            self.breaches().count()
        );
        for p in &self.pairs {
            let _ = writeln!(
                out,
                "{} {}/{} [{}] names {} `{}`\n    needs {}\n    has   {}",
                if p.satisfied { "ok  " } else { "BAD " },
                p.crate_name,
                p.file,
                p.target,
                p.shape,
                p.named,
                p.needs,
                p.has
            );
        }
        out
    }

    /// The failure text, or `None` when every target declares its cfg.
    pub fn failure(&self) -> Option<String> {
        self.breaches().next()?;
        let mut out = format!("{RULE}\n\n");
        for p in self.breaches() {
            let _ = writeln!(
                out,
                "{}/{} names {} `{}`, which needs `{}`, but the target is \
                 declared under `{}`",
                p.crate_name, p.file, p.shape, p.named, p.needs, p.has
            );
        }
        Some(out)
    }
}

/// The crate directories of the workspace, in `members` order.
fn workspace_crates(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for group in ["crates", "examples", "tools"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("Cargo.toml").is_file())
            .collect();
        dirs.sort();
        out.extend(dirs);
    }
    out
}

/// The target's root file plus every file it pulls in, each with the cfg that
/// reaches it.
fn target_files(root_file: &Path, base: &Pred) -> Vec<(PathBuf, Pred)> {
    let mut out = Vec::new();
    let mut queue = vec![(root_file.to_path_buf(), base.clone())];
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    while let Some((file, pred)) = queue.pop() {
        if !file.is_file() || !seen.insert(file.clone()) {
            continue;
        }
        let text = scrub(&read(&file));
        let here = pred.and(file_gate(&text));
        for decl in mod_decls(&text) {
            let child = here.clone().and(decl.cfg.clone());
            for cand in child_paths(&file, &decl) {
                if cand.is_file() {
                    queue.push((cand, child));
                    break;
                }
            }
        }
        out.push((file, here));
    }
    out
}

/// Read the workspace and judge every target against every gated thing it
/// names.
pub fn audit(root: &Path) -> Report {
    let mut pairs: Vec<Pair> = Vec::new();
    let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    let crates: Vec<CrateInfo> = workspace_crates(root)
        .iter()
        .filter_map(|dir| read_crate(dir))
        .collect();
    let scanned = crates.len();
    // Bin name -> the cfg it refuses to start under.
    let bins: BTreeMap<String, Pred> = crates
        .iter()
        .flat_map(|info| info.targets.iter().map(move |t| (info, t)))
        .filter(|(_, t)| t.kind == "bin")
        .filter_map(|(info, t)| {
            let arm = refused_under(&read(&info.dir.join(&t.rel_path)))?;
            Some((t.name.clone(), arm))
        })
        .collect();
    for info in crates {
        let snake = info.name.replace('-', "_");
        for target in &info.targets {
            let root_file = info.dir.join(&target.rel_path);
            if !root_file.is_file() {
                continue;
            }
            let axis = if target.main_defining {
                Axis::Features
            } else {
                Axis::Full
            };
            let mut base = Pred::True;
            for f in &target.required_features {
                base = base.and(Pred::Atom(format!("feature = \"{f}\"")));
            }
            for (file, pred) in target_files(&root_file, &base) {
                let text = scrub(&read(&file));
                let starts = line_index(&text);
                let lines = line_cfgs(&text, &pred);
                let rel = file
                    .strip_prefix(&info.dir)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                let at = |off: usize| -> Pred {
                    let line = starts.partition_point(|s| *s <= off).saturating_sub(1);
                    lines.get(line).cloned().unwrap_or_else(|| pred.clone())
                };
                // `refusal`, for a binary: the arm it refuses to start under. A
                // target declared under it runs that arm on purpose, to pin
                // the refusal; only one that builds on both arms is a breach.
                let mut push =
                    |shape, named: String, need: &Pred, have: &Pred, refusal: Option<&Pred>| {
                        let (needs, has) = (project(need, axis), project(have, axis));
                        if needs == Pred::True {
                            return;
                        }
                        let refusal = refusal.map(|r| project(r, axis));
                        let pins_refusal = refusal.as_ref().is_some_and(|r| {
                            implies(&has, r, &info.features, info.derives_backend)
                        });
                        let key = (info.name.clone(), rel.clone(), named.clone(), has.render());
                        if !seen.insert(key) {
                            return;
                        }
                        pairs.push(Pair {
                            crate_name: info.name.clone(),
                            target: format!("{} {}", target.kind, target.name),
                            file: rel.clone(),
                            shape,
                            named,
                            satisfied: pins_refusal
                                || implies(&has, &needs, &info.features, info.derives_backend),
                            needs: needs.render(),
                            has: has.render(),
                        });
                    };
                for (off, path) in refs_below(&text, &snake) {
                    let mut segs: Vec<&str> = path.split("::").collect();
                    while !segs.is_empty() {
                        let candidate = segs.join("::");
                        if let Some(need) = info.gated.get(&candidate) {
                            push("module", candidate, need, &at(off), None);
                            break;
                        }
                        segs.pop();
                    }
                }
                for (named, off) in binaries_run(&read(&file)) {
                    if let Some(arm) = bins.get(&named) {
                        let need = runnable_under(arm.clone());
                        push("binary", named.to_string(), &need, &at(off), Some(arm));
                    }
                }
                for (dep, target_cfg) in &info.optional_deps {
                    if target.kind != "bin" && info.dev_nonoptional.contains(dep) {
                        continue;
                    }
                    let enablers = minimal_enablers(dep, &info.features);
                    if enablers.is_empty() {
                        continue;
                    }
                    let need = target_cfg.clone().and(Pred::Any(
                        enablers
                            .iter()
                            .map(|f| Pred::Atom(format!("feature = \"{f}\"")))
                            .collect(),
                    ));
                    for (off, _) in refs_below(&text, &dep.replace('-', "_")) {
                        push("optional dep", dep.clone(), &need, &at(off), None);
                    }
                }
            }
        }
    }
    Report {
        crates_scanned: scanned,
        pairs,
    }
}

/// The enabler set with the redundant members dropped.
///
/// `default = ["epics"]` and `epics = ["dep:epics-base-rs"]` both enable the
/// dependency, but `default` implies `epics`, so reporting
/// `any(feature = "default", feature = "epics")` names a disjunct that can
/// never be the one a target relies on. The check is unaffected -- feature
/// closure already excludes those assignments -- so this is for the reader.
fn minimal_enablers(dep: &str, features: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let all = dep_enablers(dep, features);
    all.iter()
        .filter(|f| {
            feature_closure(f, features)
                .intersection(&all)
                .next()
                .is_none()
        })
        .cloned()
        .collect()
}

/// The workspace root, from this crate's own manifest directory.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/target-cfg-gate sits two levels below the workspace root")
        .to_path_buf()
}

/// The rule, over the whole workspace.
pub fn assert_workspace_targets_declare_their_cfg() {
    let report = audit(&workspace_root());
    // A derivation that stops finding files passes silently, which is the
    // failure this whole crate exists to prevent. The floor is loose on
    // purpose: it catches the shape where the walk breaks, and pinning it near
    // the current count would make every legitimate gate change a red test.
    assert!(
        report.pairs.len() > 100,
        "{RULE}\n\nonly {} pairs found -- the derivation stopped seeing the \
         sources, which is a silent pass",
        report.pairs.len()
    );
    if let Some(failure) = report.failure() {
        panic!("{failure}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feats(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn p(s: &str) -> Pred {
        Pred::parse(s)
    }

    #[test]
    fn an_undeclared_target_satisfies_nothing() {
        let f = feats(&[]);
        assert!(!implies(
            &Pred::True,
            &p(r#"feature = "client-core""#),
            &f,
            false
        ));
        assert!(implies(
            &p(r#"feature = "client-core""#),
            &p(r#"feature = "client-core""#),
            &f,
            false
        ));
    }

    #[test]
    fn a_feature_carries_the_features_it_enables() {
        let f = feats(&[
            ("qsrv", &["qsrv-core", "pvalink"]),
            ("qsrv-core", &["dep:epics-pva-rs"]),
        ]);
        assert!(implies(
            &p(r#"feature = "qsrv""#),
            &p(r#"feature = "qsrv-core""#),
            &f,
            false
        ));
        assert!(!implies(
            &p(r#"feature = "qsrv-core""#),
            &p(r#"feature = "qsrv""#),
            &f,
            false
        ));
    }

    #[test]
    fn the_backend_is_two_names_for_one_axis() {
        let f = feats(&[]);
        // Where `build.rs` does not derive the backend it is the target alone,
        // so excluding the embedded target is enough to have a reactor.
        assert!(implies(
            &p("not(epics_embedded_target)"),
            &p("tokio_backend"),
            &feats(&[]),
            false
        ));
        // Where it does, a host build can be reactor-free while not embedded.
        assert!(!implies(
            &p("not(epics_embedded_target)"),
            &p("tokio_backend"),
            &f,
            true
        ));
        assert!(implies(
            &p("tokio_backend"),
            &p("not(epics_embedded_target)"),
            &f,
            true
        ));
        assert!(implies(
            &p("epics_embedded_target"),
            &p("exec_backend"),
            &f,
            true
        ));
    }

    #[test]
    fn a_feature_gate_does_not_cover_a_backend_gate() {
        // The pva_gateway shape: the module needs both, the target declares one.
        let f = feats(&[("pva-gateway", &[])]);
        let need = p(r#"all(feature = "pva-gateway", tokio_backend)"#);
        assert!(!implies(&p(r#"feature = "pva-gateway""#), &need, &f, true));
        assert!(implies(
            &p(r#"all(feature = "pva-gateway", tokio_backend)"#),
            &need,
            &f,
            true
        ));
    }

    #[test]
    fn an_any_requirement_is_met_by_either_disjunct() {
        let f = feats(&[("qsrv-core", &[]), ("pvalink", &[])]);
        let need = p(r#"any(feature = "qsrv-core", feature = "pvalink")"#);
        assert!(implies(&p(r#"feature = "pvalink""#), &need, &f, false));
        assert!(!implies(&Pred::True, &need, &f, false));
    }

    #[test]
    fn platform_atoms_are_held_true_and_folded_away() {
        assert_eq!(project(&p("any(unix, windows)"), Axis::Full), Pred::True);
        assert_eq!(
            project(&p(r#"all(feature = "client-core", unix)"#), Axis::Full).render(),
            r#"feature = "client-core""#
        );
        assert_eq!(project(&p("not(unix)"), Axis::Full), Pred::True);
    }

    #[test]
    fn a_main_defining_target_is_judged_on_features_alone() {
        let need = p(r#"all(feature = "client-core", not(epics_embedded_target))"#);
        assert_eq!(
            project(&need, Axis::Features).render(),
            r#"feature = "client-core""#
        );
        assert_eq!(project(&need, Axis::Full).render(), need.render());
    }

    #[test]
    fn a_use_tree_names_every_leaf() {
        let got = refs_below(
            "use epics_ca_rs::{calink::CaLinkResolver, client::CaClient};",
            "epics_ca_rs",
        );
        let paths: BTreeSet<String> = got.into_iter().map(|(_, p)| p).collect();
        assert!(paths.contains("calink::CaLinkResolver"), "{paths:?}");
        assert!(paths.contains("client::CaClient"), "{paths:?}");
    }

    #[test]
    fn a_glob_names_the_module_it_globs() {
        let paths: BTreeSet<String> = refs_below("use epics_ca_rs::cli::*;", "epics_ca_rs")
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(paths.contains("cli"), "{paths:?}");
    }

    #[test]
    fn a_needle_inside_a_string_is_not_a_reference() {
        let text = scrub(r#"const N: [&str; 1] = ["epics_ca_rs::cli"]; fn f() {}"#);
        assert!(refs_below(&text, "epics_ca_rs").is_empty(), "{text:?}");
    }

    #[test]
    fn a_prefixed_identifier_is_not_the_crate() {
        assert!(refs_below("use my_epics_ca_rs::cli::X;", "epics_ca_rs").is_empty());
    }

    #[test]
    fn an_inner_cfg_guards_what_it_encloses() {
        let src = "#![cfg(feature = \"client\")]\n\
                   #[cfg(feature = \"tls\")]\n\
                   mod m {\n\
                   use rustls::X;\n\
                   }\n\
                   use plain::Y;\n";
        let text = scrub(src);
        let lines = line_cfgs(&text, &p(r#"feature = "client""#));
        let inside = lines[3].render();
        assert!(inside.contains(r#"feature = "tls""#), "{inside}");
        let outside = lines[5].render();
        assert!(!outside.contains("tls"), "{outside}");
    }

    #[test]
    fn a_gated_submodule_declaration_is_read_with_its_cfg() {
        let decls = mod_decls(&scrub(
            "#[cfg(all(unix, feature = \"tls\"))]\n\
             #[path = \"mods/tls_interop.rs\"]\n\
             mod tls_interop;\n\
             mod plain;\n",
        ));
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "tls_interop");
        assert_eq!(decls[0].path.as_deref(), Some("mods/tls_interop.rs"));
        assert!(decls[0].cfg.render().contains(r#"feature = "tls""#));
        assert_eq!(decls[1].cfg, Pred::True);
    }

    #[test]
    fn a_redundant_enabler_is_dropped_from_the_report() {
        let f = feats(&[("default", &["epics"]), ("epics", &["dep:epics-base-rs"])]);
        let got = minimal_enablers("epics-base-rs", &f);
        assert_eq!(got, BTreeSet::from(["epics".to_string()]));
    }

    #[test]
    fn a_dependencys_own_feature_enables_the_dependency() {
        let f = feats(&[("tls", &["rustls/std"])]);
        assert!(dep_enablers("rustls", &f).contains("tls"));
    }

    #[test]
    fn a_toml_comment_inside_a_string_is_not_a_comment() {
        let secs = sections("[package]\nname = \"x\"\ndescription = \"a # b\"\n");
        assert_eq!(secs.len(), 1);
        assert!(secs[0].1.contains("a # b"));
    }

    #[test]
    fn a_multi_line_feature_array_is_one_feature() {
        let got = toml_arrays("qsrv = [\n    \"qsrv-core\",\n    \"pvalink\",\n]\n");
        assert_eq!(
            got,
            vec![(
                "qsrv".to_string(),
                vec!["qsrv-core".to_string(), "pvalink".to_string()]
            )]
        );
    }

    /// A bin's two `main` arms, spelled so the backend scan of `rtems-exec-gate`
    /// does not read this file as gating on the backend.
    fn backend_arms(src: &str) -> String {
        src.replace("TOKIO", "tokio_backend")
            .replace("EXEC", "exec_backend")
    }

    #[test]
    fn a_refuse_backend_call_is_a_requirement() {
        let src = "#[cfg(TOKIO)]\n#[tokio::main]\nasync fn main() { run().await }\n\
                   #[cfg(EXEC)]\nfn main() -> std::process::ExitCode {\n    \
                   epics_base_rs::runtime::task::refuse_backend(\"x: needs a reactor\")\n}\n";
        let arm = refused_under(&backend_arms(src)).expect("the exec arm refuses");
        assert_eq!(arm, p("exec_backend"));
        assert_eq!(runnable_under(arm), p("tokio_backend"));
    }

    #[test]
    fn a_refusal_reached_after_main_takes_its_function_cfg() {
        let src = "fn main() { parse(); refuse(); }\n\
                   #[cfg(EXEC)]\nfn refuse() -> ! {\n    eprintln!(\"dry run\");\n    \
                   refuse_backend(\"y: no reactor\")\n}\n";
        assert_eq!(refused_under(&backend_arms(src)), Some(p("exec_backend")));
    }

    #[test]
    fn a_main_that_runs_on_both_backends_requires_nothing() {
        let src = "#[cfg(TOKIO)]\nasync fn main() -> CaResult<()> { a().await }\n\
                   #[cfg(EXEC)]\nfn main() -> CaResult<()> {\n    \
                   eprintln!(\"booting on threads\");\n    b()\n}\n\
                   // refuse_backend(\"in a comment\")\n\
                   pub fn refuse_backend(message: &str) -> ! { loop {} }\n";
        assert_eq!(refused_under(&backend_arms(src)), None);
    }

    #[test]
    fn a_bin_is_run_by_its_exe_variable_or_by_command_new() {
        let got = binaries_run(
            "Command::new(env!(\"CARGO_BIN_EXE_a\"));\nCommand::new(\"target/debug/b\");\n\
             assert!(out.contains(\"c\"));\nlet d = \"d\";",
        );
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    fn names(src: &str) -> Vec<String> {
        literals(src).into_iter().map(|(_, l)| l).collect()
    }

    #[test]
    fn a_quote_char_literal_does_not_open_a_string() {
        assert_eq!(
            names("s.trim_matches('\"'); let x = env!(\"CARGO_BIN_EXE_a\");"),
            ["CARGO_BIN_EXE_a"]
        );
    }

    #[test]
    fn a_quote_byte_literal_does_not_open_a_string() {
        assert_eq!(
            names("if c == b'\"' { run(\"CARGO_BIN_EXE_a\") }"),
            ["CARGO_BIN_EXE_a"]
        );
    }

    #[test]
    fn an_escaped_quote_char_literal_and_a_lifetime_are_not_strings() {
        assert_eq!(
            names("fn f<'a>(x: &'a str) { '\\''; 'l: loop {} g(\"CARGO_BIN_EXE_a\") }"),
            ["CARGO_BIN_EXE_a"]
        );
    }

    #[test]
    fn a_hash_fenced_raw_string_ends_at_its_fence() {
        assert_eq!(
            names("r#\"say \"hi\"\"#; Command::new(\"a\");"),
            ["say \"hi\"", "a"]
        );
    }

    #[test]
    fn a_hash_fenced_raw_byte_string_ends_at_its_fence() {
        assert_eq!(
            names("br#\"\"quoted\"\"#; Command::new(\"a\");"),
            ["\"quoted\"", "a"]
        );
    }

    #[test]
    fn scrub_keeps_code_after_a_quote_char_literal() {
        let src = "let q = '\"';\n#[cfg(EXEC)]\nfn main() { refuse_backend(\"m\") }\n";
        let text = scrub(&backend_arms(src));
        assert_eq!(text.len(), backend_arms(src).len());
        assert!(text.contains("fn main() { refuse_backend(   ) }"), "{text}");
        assert_eq!(refused_under(&backend_arms(src)), Some(p("exec_backend")));
    }

    #[test]
    fn scrub_keeps_byte_offsets_across_multibyte_text() {
        let src = "// — é\nlet s = \"→\"; x";
        let text = scrub(src);
        assert_eq!(text.len(), src.len());
        assert_eq!(&text[src.len() - 1..], "x");
    }

    #[test]
    fn a_bin_named_in_a_comment_is_not_run() {
        let got: Vec<String> = literals(
            "// env!(\"CARGO_BIN_EXE_a\")\n/* \"b\" */ let x = env!(\"CARGO_BIN_EXE_c\"); \
             let y = r#\"d\"#;",
        )
        .into_iter()
        .map(|(_, l)| l)
        .collect();
        assert_eq!(got, ["CARGO_BIN_EXE_c", "d"]);
    }
}
