//! Fails a change that adds a fixed wait to a test.
//!
//! A test that sleeps a fixed time, or bounds its wait for a fact by a
//! literal, bets on how much CPU the runner gives it, and a correct test fails
//! when the bet is lost. Test code held 2823 of them at 451531667, so the gate
//! compares what a change's test code holds at the merge base with what it
//! holds now, and fails only on a wait the change adds.
//!
//! Test code is every file under a `tests/` directory, and under `src/` what
//! [`source_guard::test_lines`] marks (a `cfg(test)` item, a function under a
//! test attribute) plus every file only such items declare as a module.
//!
//! A *fixed wait* is a call to `sleep` or `sleep_*`; a call whose name has a
//! `timeout`, `deadline`, `wait` or `until` segment and takes a duration
//! written `Duration::from_*(<number>)`; `now() + Duration::from_*(<number>)`;
//! and, in a `loop` or `while`, `elapsed()` compared with such a duration. A
//! sleep or a timeout on each attempt inside a `loop` or `while` body is its
//! poll interval, and the loop's bound is what decides the test, so the bound
//! (a `while` head included) is what gets checked. A timeout there whose
//! expiry leaves the loop (`Err(_) => break`, `?`, `continue 'outer`) or
//! fails the test (`.expect`, `panic!`, `assert!(r.is_ok())`, a helper in the
//! same file that does) is that bound, and is counted. A sleep that is a
//! `select!` arm's future is a timeout on the other arms, read by the same
//! rule. A `for` loop is bounded by its count, so its sleeps and timeouts are
//! counted: a bounded poll when the body breaks, returns or continues an outer
//! loop, pacing when it never does. A duration held in a named constant is not
//! seen.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

/// The comment that keeps a window whose only job is to prove nothing happens.
pub const ABSENCE_WINDOW: &str = "absence-window:";

/// The comment that keeps a duration that waits for nothing: a value under
/// test, or pacing between stimuli. It does not keep the interval of a `for`
/// loop that breaks or returns, or a timeout whose expiry ends its loop:
/// both wait for something.
pub const NOT_A_WAIT: &str = "not-a-wait:";

/// The rule, as the gate prints it under every failure.
pub const RULE: &str = "\
A fixed sleep or a literal timeout bets on how fast the runner is, not on the
code under test, and fails a correct test on a slow one. Bound a wait for
something to happen by fact_budget::FACT_BUDGET: a timeout of FACT_BUDGET, or a
loop to `Instant::now() + FACT_BUDGET` (a sleep or a timeout on each attempt
inside a `loop` or `while` body is its poll interval and is not counted; a
timeout whose expiry ends that loop, by break, return, `?`, `continue 'outer`
or a panic, is its bound and is, as is a `select!` sleep arm that does; so is
a `for` loop that breaks or returns, a poll bounded by count x interval).
Close a claim that something did not happen on fact_budget::barrier::until or
fact_budget::barrier::until_async (crates/fact-budget, a path
dev-dependency). A window that exists only to prove nothing happens may stay
if its line, or the comment directly above it, says `// absence-window:
<reason>`. A duration that waits for nothing (a value under test, pacing
between stimuli) may stay marked `// not-a-wait: <reason>`; that mark does not
keep a poll, which waits for its condition.
";

/// What a counted wait is, for the line the gate prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A sleep, or a wait bounded by a literal.
    Fixed,
    /// A sleep or timeout in a `for` loop that breaks or returns: a poll
    /// whose bound is count x interval.
    BoundedPoll,
    /// A sleep in a `for` loop that never breaks: pacing between stimuli.
    Pacing,
    /// A timeout in a `loop` or `while` body whose expiry leaves the loop or
    /// fails the test: the loop's bound, not its poll interval.
    LoopBound,
}

/// One fixed wait in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    /// 1-based line of the call's name.
    pub line: usize,
    /// What a wait is matched by across the merge base: its callee and its
    /// duration, so a re-indent or a move keeps it and a new number does not.
    pub key: String,
    pub kind: Kind,
}

/// A test source as it was at the merge base and as it is now; empty when
/// the file did not exist on that side.
#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub base: String,
    pub now: String,
}

/// A wait the change adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub path: String,
    pub line: usize,
    pub key: String,
    pub kind: Kind,
    /// How many waits with this key the changed files held at the merge base.
    pub at_base: usize,
    /// How many they hold now.
    pub now: usize,
}

/// Whether `path` (relative, `/`-separated) is a Rust file under a `tests/`
/// directory.
pub fn is_test_source(path: &str) -> bool {
    path.ends_with(".rs")
        && path
            .rsplit_once('/')
            .is_some_and(|(dir, _)| dir.split('/').any(|c| c == "tests"))
}

/// The test code of one file, with every other line blanked so line numbers
/// hold: all of it when it is under `tests/` or in `modules` (see
/// [`test_modules`]), and otherwise its test items, `source_guard`'s rule.
pub fn test_code(path: &str, src: &str, modules: &BTreeSet<String>) -> String {
    if is_test_source(path) || modules.contains(path) {
        return src.to_owned();
    }
    let test = source_guard::test_lines(src);
    let mut out: String = src
        .lines()
        .zip(test)
        .map(|(line, t)| if t { line } else { "" })
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

/// The files of `files` (path to source) that only test code declares: a
/// `mod` declared in a test item or in a file under `tests/`, and the modules
/// those declare in turn. A file some production `mod` also declares ships,
/// and is not one.
pub fn test_modules(files: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut declared: BTreeMap<String, Vec<(&str, bool)>> = BTreeMap::new();
    for (path, src) in files {
        let test = source_guard::test_lines(src);
        for (line, targets) in mod_decls(path, src) {
            let in_test = test.get(line).copied().unwrap_or(false);
            for t in targets {
                declared.entry(t).or_default().push((path, in_test));
            }
        }
    }
    let mut whole = BTreeSet::new();
    loop {
        let grew: Vec<String> = declared
            .iter()
            .filter(|(t, by)| {
                !whole.contains(*t)
                    && by
                        .iter()
                        .all(|(d, test)| *test || is_test_source(d) || whole.contains(*d))
            })
            .map(|(t, _)| t.clone())
            .collect();
        if grew.is_empty() {
            return whole;
        }
        whole.extend(grew);
    }
}

/// Each out-of-line `mod name;` in `src` (the file at `path`): its 0-based
/// line and the files it can name, `name.rs` and `name/mod.rs` under the
/// module's directory, or its `#[path]`.
fn mod_decls(path: &str, src: &str) -> Vec<(usize, Vec<String>)> {
    let m = mask(src);
    let raw: Vec<&str> = src.lines().collect();
    let dir = path.rfind('/').map_or("", |i| &path[..=i]);
    let file = &path[dir.len()..];
    let parent = dir.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    // A crate root and a `mod.rs` hold their modules beside them; any other
    // file holds them in a directory of its own name.
    let root = matches!(file, "mod.rs" | "lib.rs" | "main.rs" | "build.rs")
        || matches!(parent, "bin" | "examples" | "benches" | "tests");
    let module_dir = if root {
        dir.to_owned()
    } else {
        format!("{dir}{}/", file.trim_end_matches(".rs"))
    };
    let mut inline: Vec<(usize, String)> = Vec::new();
    let mut out = Vec::new();
    for l in 0..m.line_starts.len() {
        let code = String::from_utf8_lossy(m.line_code(l)).into_owned();
        let t = code.trim();
        let indent = code.len() - code.trim_start().len();
        if t == "}" {
            if inline.last().is_some_and(|&(i, _)| i == indent) {
                inline.pop();
            }
            continue;
        }
        let Some((name, rest)) = mod_head(t) else {
            continue;
        };
        if rest.starts_with('{') {
            if !rest.ends_with('}') {
                inline.push((indent, name.to_owned()));
            }
            continue;
        }
        if rest != ";" {
            continue;
        }
        let attr = (0..l)
            .rev()
            .map(|k| (k, String::from_utf8_lossy(m.line_code(k)).trim().to_owned()))
            .take_while(|(_, a)| a.starts_with("#["))
            .find(|(_, a)| a.starts_with("#[path"))
            .and_then(|(k, _)| raw.get(k)?.split('"').nth(1).map(str::to_owned));
        let nest: String = inline.iter().map(|(_, n)| format!("{n}/")).collect();
        let targets = match attr {
            // A top-level `#[path]` is relative to the declaring file's own
            // directory; one inside an inline module, to that module's.
            Some(p) if inline.is_empty() => vec![normalise(&format!("{dir}{p}"))],
            Some(p) => vec![normalise(&format!("{module_dir}{nest}{p}"))],
            None => vec![
                format!("{module_dir}{nest}{name}.rs"),
                format!("{module_dir}{nest}{name}/mod.rs"),
            ],
        };
        out.push((l, targets));
    }
    out
}

/// `mod name` after any visibility, and the rest of the line; `None` when the
/// line declares no module.
fn mod_head(t: &str) -> Option<(&str, &str)> {
    let mut t = t;
    if let Some(r) = t.strip_prefix("pub") {
        t = r.trim_start();
        if t.starts_with('(') {
            t = t[t.find(')')? + 1..].trim_start();
        }
    }
    let r = t.strip_prefix("mod")?;
    if !r.starts_with(char::is_whitespace) {
        return None;
    }
    let r = r.trim_start();
    let end = r
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(r.len());
    (end > 0).then(|| (&r[..end], r[end..].trim()))
}

/// `a/b/../c/./d.rs` as `a/c/d.rs`.
fn normalise(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// What a `{` opens, as far as a wait inside it is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    /// A `loop` or `while` body. It retries until its own bound, so a sleep
    /// or a per-attempt timeout in it is a poll interval, and the bound is
    /// what gets checked.
    Poll,
    /// A `for` body. Its bound is a count, so its sleeps add up to a fixed
    /// wait.
    Count,
    /// A `fn`, `impl`, `trait` or `mod` body, which no loop outside retries.
    Item,
    /// A closure or `async` block: it means what the block around it means,
    /// but a `return` in it leaves only it.
    Closure,
    /// Any other block (`if`, `match`, `unsafe`), which means what the block
    /// around it means.
    Plain,
}

struct Opened<'a> {
    kind: Block,
    label: Option<&'a str>,
    parent: Option<usize>,
    /// For a `for` body: a `break`, `return` or `continue 'outer` in it
    /// leaves the loop.
    exits: bool,
    /// Where the `{` is, when it opens a `select!` body.
    select: Option<usize>,
}

#[derive(Clone, Copy)]
enum Leave<'a> {
    Return,
    Break(Option<&'a str>),
    Continue(Option<&'a str>),
}

/// Where a call sits: in a `while` head, or under the innermost block a wait
/// is not transparent to.
#[derive(Clone, Copy)]
enum Site {
    Head,
    In(Option<usize>),
}

#[derive(Clone, Copy)]
enum Ctx {
    Straight,
    PollHead,
    /// `ends`: the call is a timeout whose expiry leaves the loop.
    PollBody {
        ends: bool,
    },
    For {
        exits: bool,
    },
}

/// The unmarked fixed waits in `src`.
pub fn waits(src: &str) -> Vec<Wait> {
    let m = mask(src);
    let c = &m.code;
    let mut blocks: Vec<Opened> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut leaves: Vec<(usize, Option<usize>, Leave)> = Vec::new();
    let mut calls: Vec<(usize, &str, usize, Site, Option<usize>)> = Vec::new();
    // `(` / `[` nesting, and the kind (and label) a keyword gave the next `{`
    // at its own nesting (a `while` head is inside the loop it bounds).
    let mut depth = 0usize;
    let mut announced: Option<(Block, usize)> = None;
    let mut label: Option<(&str, usize)> = None;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'{' => {
                let kind = match announced.take_if(|(_, d)| *d == depth) {
                    Some((b, _)) => b,
                    None if opens_closure(c, i) => Block::Closure,
                    None => Block::Plain,
                };
                blocks.push(Opened {
                    kind,
                    label: label.take_if(|(_, d)| *d == depth).map(|(l, _)| l),
                    parent: open.last().copied(),
                    exits: false,
                    select: opens_select(c, i).then_some(i),
                });
                open.push(blocks.len() - 1);
            }
            b'}' => {
                open.pop();
            }
            b';' => {
                announced.take_if(|(_, d)| *d >= depth);
                label.take_if(|(_, d)| *d >= depth);
            }
            b'\'' => {
                if let Some((l, after)) = block_label(c, src, i) {
                    label = Some((l, depth));
                    i = after;
                    continue;
                }
            }
            x if is_ident_start(x) && (i == 0 || !is_ident(c[i - 1])) => {
                let end = ident_end(c, i);
                let name = &src[i..end];
                let kind = match name {
                    "loop" | "while" => Some(Block::Poll),
                    "for" if c.get(skip_ws(c, end)) != Some(&b'<') => Some(Block::Count),
                    "fn" | "impl" | "trait" | "mod" => Some(Block::Item),
                    _ => None,
                };
                if let Some(kind) = kind
                    && announced.is_none()
                {
                    announced = Some((kind, depth));
                }
                let leave = match name {
                    "return" => Some(Leave::Return),
                    "break" => Some(Leave::Break(break_label(c, src, skip_ws(c, end)))),
                    "continue" => Some(Leave::Continue(break_label(c, src, skip_ws(c, end)))),
                    _ => None,
                };
                if let Some(leave) = leave {
                    leaves.push((i, open.last().copied(), leave));
                }
                if (is_sleep(name) || is_wait_name(name) || name == "now" || name == "elapsed")
                    && !after_fn(c, i)
                {
                    let site =
                        if announced.is_some_and(|(b, d)| b == Block::Poll && depth >= d) {
                            Site::Head
                        } else {
                            Site::In(open.iter().rev().copied().find(|&b| {
                                !matches!(blocks[b].kind, Block::Plain | Block::Closure)
                            }))
                        };
                    calls.push((i, name, end, site, open.last().copied()));
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    for &(_, from, leave) in &leaves {
        for b in left(&blocks, from, leave) {
            if blocks[b].kind == Block::Count {
                blocks[b].exits = true;
            }
        }
    }
    let tree = Tree {
        c,
        src,
        blocks,
        leaves,
    };
    let mut out = Vec::new();
    for (i, name, end, site, inner) in calls {
        // A sleep that is a `select!` arm's future is a timeout on the other
        // arms, and its arm's body is where its expiry goes.
        let arm = if is_sleep(name) {
            tree.select_arm(i)
        } else {
            None
        };
        let timer = arm.is_some() || (is_wait_name(name) && !is_sleep(name));
        let ctx = match site {
            Site::Head => Ctx::PollHead,
            Site::In(Some(b)) if tree.blocks[b].kind == Block::Poll => Ctx::PollBody {
                ends: match arm {
                    Some(body) => tree.runs_end(b, &[body]),
                    None => timer && tree.expiry_ends(b, inner, i, end),
                },
            },
            Site::In(Some(b)) if tree.blocks[b].kind == Block::Count => Ctx::For {
                exits: tree.blocks[b].exits,
            },
            Site::In(_) => Ctx::Straight,
        };
        let Some((key, kind)) = fixed_wait(c, name, end, ctx, timer) else {
            continue;
        };
        let line = m.line_of(i);
        // A poll waits for its condition, so only an absence window keeps it.
        let kept = m.marked(line, ABSENCE_WINDOW)
            || (!matches!(kind, Kind::BoundedPoll | Kind::LoopBound) && m.marked(line, NOT_A_WAIT));
        if !kept {
            out.push(Wait {
                line: line + 1,
                key,
                kind,
            });
        }
    }
    out
}

/// The blocks that `leave`, written in block `from`, jumps out of: a `break`
/// leaves its target loop, a `continue` stays in it.
fn left(blocks: &[Opened], from: Option<usize>, leave: Leave) -> Vec<usize> {
    let mut crossed = Vec::new();
    let mut at = from;
    while let Some(b) = at {
        let o = &blocks[b];
        if matches!(o.kind, Block::Item | Block::Closure) {
            break;
        }
        let target = match leave {
            Leave::Return => false,
            Leave::Break(None) | Leave::Continue(None) => {
                matches!(o.kind, Block::Poll | Block::Count)
            }
            Leave::Break(Some(l)) | Leave::Continue(Some(l)) => o.label == Some(l),
        };
        if target {
            if matches!(leave, Leave::Break(_)) {
                crossed.push(b);
            }
            return crossed;
        }
        crossed.push(b);
        at = o.parent;
    }
    // Only a `return` ends at a closure, an item or the top.
    if matches!(leave, Leave::Return) {
        crossed
    } else {
        Vec::new()
    }
}

/// The parsed block structure of one source, for following where a
/// timeout's expiry goes.
struct Tree<'a> {
    c: &'a [u8],
    src: &'a str,
    blocks: Vec<Opened<'a>>,
    /// Each `return`, `break` and `continue`: where, and in which block.
    leaves: Vec<(usize, Option<usize>, Leave<'a>)>,
}

/// How many `let` bindings and helpers a timeout's result is followed
/// through to where its expiry is decided.
const HOPS: u8 = 3;

/// A call or macro around an argument.
struct Call<'a> {
    /// Where its name starts.
    at: usize,
    name: &'a str,
    /// A macro: `name!(..)`.
    bang: bool,
    /// Its `)`.
    close: usize,
    /// Which argument, from 0.
    arg: usize,
}

/// Where the expiry of a timeout's result goes.
enum Expiry {
    /// Nowhere the code shows: it falls through, retries or is dropped.
    Retries,
    /// `?`: out of the enclosing fn or closure.
    Returns,
    /// `.expect`, `.unwrap`: the test fails.
    Panics,
    /// Into these spans: a match arm, an `else`, an `if` body.
    Runs(Vec<(usize, usize)>),
}

impl Tree<'_> {
    /// Whether the timeout called by the identifier at `i..end`, in loop
    /// `l` and block `inner`, leaves `l` or fails the test when it expires.
    fn expiry_ends(&self, l: usize, inner: Option<usize>, i: usize, end: usize) -> bool {
        let c = self.c;
        let name = &self.src[i..end];
        // A setter configures a later wait, and std's `Condvar::wait_timeout*`
        // returns a `LockResult`: neither result says the time ran out.
        if name.starts_with("set_") || name.starts_with("wait_timeout") {
            return false;
        }
        let open = skip_ws(c, end);
        if c.get(open) != Some(&b'(') {
            return false;
        }
        match self.expiry(chain_start(c, i), matching(c, open) + 1, HOPS) {
            Expiry::Retries => false,
            Expiry::Panics => true,
            Expiry::Returns => left(&self.blocks, inner, Leave::Return).contains(&l),
            Expiry::Runs(spans) => self.runs_end(l, &spans),
        }
    }

    /// Whether running any of `spans` leaves loop `l` or fails the test.
    fn runs_end(&self, l: usize, spans: &[(usize, usize)]) -> bool {
        spans.iter().any(|&(s, e)| {
            panics_in(&self.c[s..e])
                || self.leaves.iter().any(|&(at, from, leave)| {
                    (s..e).contains(&at) && left(&self.blocks, from, leave).contains(&l)
                })
        })
    }

    /// The body of the `select!` arm whose future the call at `i` is in.
    fn select_arm(&self, i: usize) -> Option<(usize, usize)> {
        let c = self.c;
        let open = self
            .blocks
            .iter()
            .filter_map(|b| b.select)
            .filter(|&o| o < i && i < matching(c, o))
            .max()?;
        arms(c, open)
            .into_iter()
            // The head without its guard: the call is the arm's future.
            .find(|&(at, head, _)| (at..at + head.len()).contains(&i))
            .map(|(_, _, body)| body)
    }

    /// Where the expiry of the result of the expression at `start..end` goes,
    /// following it through up to `hops` `let` bindings and helpers.
    fn expiry(&self, start: usize, end: usize, hops: u8) -> Expiry {
        let c = self.c;
        let mut t = skip_ws(c, end);
        if word_at(c, self.src, skip_dot(c, t)) == Some("await") {
            t = skip_ws(c, skip_dot(c, t) + 5);
        }
        // The methods on the result: adapters that keep an expiry an error,
        // then `?`, `.expect(..)`, `.is_err()`, ...
        let mut test: Option<bool> = None;
        loop {
            if c.get(t) == Some(&b'?') {
                return Expiry::Returns;
            }
            if test.is_some() || c.get(t) != Some(&b'.') {
                break;
            }
            let m = skip_dot(c, t);
            let Some(method) = word_at(c, self.src, m) else {
                return Expiry::Retries;
            };
            let args = skip_ws(c, m + method.len());
            if c.get(args) != Some(&b'(') {
                return Expiry::Retries;
            }
            let close = matching(c, args);
            match method {
                "expect" | "unwrap" => return Expiry::Panics,
                "unwrap_or_else" if panics_in(&c[args..close]) => return Expiry::Panics,
                "is_err" | "is_none" => test = Some(true),
                "is_ok" | "is_some" => test = Some(false),
                "map" | "map_err" | "ok" | "ok_or" | "ok_or_else" | "context" | "with_context" => {}
                _ => return Expiry::Retries,
            }
            t = skip_ws(c, close + 1);
        }
        let mut start = start;
        // `matches!(<expr>, PAT)`: a test that holds on expiry when PAT
        // takes it. A guard can only turn a match down, so a guarded match
        // is unknown.
        if test.is_none()
            && c.get(t) == Some(&b',')
            && let Some(call) = self.call_around(start, t)
            && call.bang
            && call.arg == 0
            && call.name == "matches"
        {
            let pat = c[t + 1..call.close].trim_ascii();
            let guard = find_word_depth0(pat, b"if");
            let takes = matches_expiry(guard.map_or(pat, |g| &pat[..g]));
            if takes && guard.is_some() {
                return Expiry::Retries;
            }
            test = Some(takes);
            start = call.at;
            t = skip_ws(c, call.close + 1);
        }
        let before = c[..start].trim_ascii_end();
        let prev = last_word(before);
        if test.is_none() && prev == b"match" && c.get(t) == Some(&b'{') {
            return Expiry::Runs(
                arms(c, t)
                    .into_iter()
                    .filter(|&(_, pat, _)| matches_expiry(pat))
                    .map(|(_, _, body)| body)
                    .collect(),
            );
        }
        // `if <expr>.is_err() {`, `if !<expr>.is_ok() {`, `assert!(<expr>.is_ok())`
        if let Some(on_expiry) = test {
            let (negated, rest) = match before.strip_suffix(b"!") {
                Some(r) => (true, r.trim_ascii_end()),
                None => (false, before),
            };
            let holds = on_expiry != negated;
            if last_word(rest) == b"if" {
                return self.branch(t, holds);
            }
            let asserted = self.call_around(rest.len(), t).is_some_and(|a| {
                a.bang && a.arg == 0 && matches!(a.name, "assert" | "debug_assert")
            });
            return if asserted && !holds {
                Expiry::Panics
            } else {
                Expiry::Retries
            };
        }
        // `check(<expr>)`
        if let Some(call) = self.call_around(start, t) {
            return match hops {
                0 => Expiry::Retries,
                _ => self.through_helper(&call, hops - 1),
            };
        }
        // `let PAT = <expr>`, `if let PAT = <expr>`
        let Some(lhs) = before.strip_suffix(b"=").filter(|l| {
            !l.ends_with(b"=") && !l.ends_with(b"!") && !l.ends_with(b"<") && !l.ends_with(b">")
        }) else {
            return Expiry::Retries;
        };
        let stmt = statement_start(lhs);
        let head = lhs[stmt..].trim_ascii();
        let head = strip_word(head, b"else").unwrap_or(head);
        if let Some(pat) = strip_word(head, b"if").and_then(|h| strip_word(h, b"let")) {
            return self.branch(t, matches_expiry(pat));
        }
        let Some(pat) = strip_word(head, b"let") else {
            return Expiry::Retries;
        };
        if word_at(c, self.src, t) == Some("else") {
            let open = skip_ws(c, t + 4);
            return match (c.get(open), matches_expiry(pat)) {
                (Some(b'{'), false) => Expiry::Runs(vec![(open, matching(c, open) + 1)]),
                _ => Expiry::Retries,
            };
        }
        let name = strip_word(pat, b"mut").unwrap_or(pat);
        // `let r: Result<..> = ..` binds `r`.
        let name = name
            .split(|&x| x == b':')
            .next()
            .unwrap_or(name)
            .trim_ascii();
        if hops > 0
            && c.get(t) == Some(&b';')
            && name
                .first()
                .is_some_and(|&x| x.is_ascii_lowercase() || x == b'_')
            && name != b"_"
            && name.iter().all(|&x| is_ident(x))
            && let Some(u) = next_use(c, t + 1, name)
        {
            return self.expiry(u, u + name.len(), hops - 1);
        }
        Expiry::Retries
    }

    /// The call or macro that takes the expression at `start..t` as one
    /// whole argument.
    fn call_around(&self, start: usize, t: usize) -> Option<Call<'_>> {
        let c = self.c;
        let pre = c[..start].trim_ascii_end();
        if !matches!(pre.last(), Some(b'(' | b',')) || !matches!(c.get(t), Some(b',' | b')')) {
            return None;
        }
        let (mut d, mut arg) = (0usize, 0);
        let mut open = pre.len();
        loop {
            open = open.checked_sub(1)?;
            match c[open] {
                b')' | b']' | b'}' => d += 1,
                b'(' if d == 0 => break,
                b'[' | b'{' | b';' if d == 0 => return None,
                b'(' | b'[' | b'{' => d -= 1,
                b',' if d == 0 => arg += 1,
                _ => {}
            }
        }
        let head = c[..open].trim_ascii_end();
        let (bang, head) = match head.strip_suffix(b"!") {
            Some(h) => (true, h.trim_ascii_end()),
            None => (false, head),
        };
        let at = head.len() - last_word(head).len();
        let name = word_at(c, self.src, at)?;
        (!matches!(name, "if" | "while" | "match" | "return" | "in")).then(|| Call {
            at,
            name,
            bang,
            close: matching(c, open),
            arg,
        })
    }

    /// Where the expiry goes in a helper this file defines once, which
    /// `call` passes it to: a panic there fails the test, and an error the
    /// helper returns comes back out of the call.
    fn through_helper(&self, call: &Call, hops: u8) -> Expiry {
        let c = self.c;
        let pre = c[..call.at].trim_ascii_end();
        // A method is followed only on `self`, whose impl is in this file.
        let method = match pre.strip_suffix(b".") {
            Some(r) if last_word(r.trim_ascii_end()) == b"self" => true,
            Some(_) => return Expiry::Retries,
            None if call.bang || pre.ends_with(b"::") => return Expiry::Retries,
            None => false,
        };
        let name = call.name.as_bytes();
        let defs: Vec<usize> = (0..c.len())
            .filter(|&j| {
                c[j..].starts_with(b"fn")
                    && (j == 0 || !is_ident(c[j - 1]))
                    && c.get(j + 2).is_some_and(u8::is_ascii_whitespace)
                    && c[skip_ws(c, j + 2)..].starts_with(name)
                    && !c
                        .get(skip_ws(c, j + 2) + name.len())
                        .copied()
                        .is_some_and(is_ident)
            })
            .collect();
        let [def] = defs[..] else {
            return Expiry::Retries;
        };
        let mut open = skip_ws(c, skip_ws(c, def + 2) + name.len());
        if c.get(open) == Some(&b'<') {
            open = skip_ws(c, generics_end(c, open));
        }
        if c.get(open) != Some(&b'(') {
            return Expiry::Retries;
        }
        let close = matching(c, open);
        let mut params: Vec<&[u8]> = split_top(&c[open + 1..close], b',')
            .into_iter()
            .map(<[u8]>::trim_ascii)
            .filter(|p| !p.is_empty())
            .collect();
        if params.first().is_some_and(|p| is_self_param(p)) != method {
            return Expiry::Retries;
        }
        if method {
            params.remove(0);
        }
        let Some(param) = params.get(call.arg) else {
            return Expiry::Retries;
        };
        let param = split_top(param, b':')[0].trim_ascii();
        let param = strip_word(param, b"mut").unwrap_or(param);
        if !is_binding(param) || param == b"_" || !param.iter().all(|&x| is_ident(x)) {
            return Expiry::Retries;
        }
        let Some(body) = brace_at_depth0(c, close + 1) else {
            return Expiry::Retries;
        };
        let Some(u) = next_use(c, body + 1, param) else {
            return Expiry::Retries;
        };
        match self.expiry(u, u + param.len(), hops) {
            Expiry::Panics => Expiry::Panics,
            Expiry::Runs(spans) if spans.iter().any(|&(s, e)| panics_in(&c[s..e])) => {
                Expiry::Panics
            }
            Expiry::Returns => self.expiry(chain_start(c, call.at), call.close + 1, hops),
            _ => Expiry::Retries,
        }
    }

    /// The span an `if` whose condition ends at `t` runs on expiry: its body
    /// when the condition holds then, else its `else` chain.
    fn branch(&self, t: usize, holds: bool) -> Expiry {
        let c = self.c;
        let Some(open) = brace_at_depth0(c, t) else {
            return Expiry::Retries;
        };
        let close = matching(c, open);
        if holds {
            return Expiry::Runs(vec![(open, close + 1)]);
        }
        let mut at = skip_ws(c, close + 1);
        if word_at(c, self.src, at) != Some("else") {
            return Expiry::Runs(Vec::new());
        }
        let start = at;
        loop {
            let Some(open) = brace_at_depth0(c, at + 4) else {
                return Expiry::Retries;
            };
            let close = matching(c, open);
            at = skip_ws(c, close + 1);
            if word_at(c, self.src, at) != Some("else") {
                return Expiry::Runs(vec![(start, close + 1)]);
            }
        }
    }
}

/// Whether some value of a timeout's expiry matches `pat`: `_`, a binding,
/// `None`, or `Err(..)` of anything but a named variant other than a timeout.
fn matches_expiry(pat: &[u8]) -> bool {
    split_top(pat, b'|').into_iter().any(|alt| {
        let alt = alt.trim_ascii();
        if alt == b"None" || is_binding(alt) {
            return true;
        }
        let Some(inner) = alt
            .strip_prefix(b"Err")
            .map(<[u8]>::trim_ascii_start)
            .and_then(|r| r.strip_prefix(b"("))
            .and_then(|r| r.strip_suffix(b")"))
        else {
            return false;
        };
        let inner = inner.trim_ascii();
        if inner == b".." || is_binding(inner) {
            return true;
        }
        let end = inner
            .iter()
            .position(|&x| !(is_ident(x) || x == b':'))
            .unwrap_or(inner.len());
        let last = inner[..end].rsplit(|&x| x == b':').next().unwrap_or(b"");
        [&b"Timeout"[..], b"Elapsed", b"TimedOut"]
            .iter()
            .any(|w| last.windows(w.len()).any(|x| x == *w))
    })
}

/// `_`, `x`, `_x`, `mut x`, `ref x`, `x @ ..`: a pattern that takes any value.
fn is_binding(p: &[u8]) -> bool {
    let p = strip_word(p, b"ref").unwrap_or(p);
    let p = strip_word(p, b"mut").unwrap_or(p);
    let end = p.iter().position(|&x| !is_ident(x)).unwrap_or(p.len());
    let rest = p[end..].trim_ascii();
    end > 0
        && (p[0].is_ascii_lowercase() || p[0] == b'_')
        && (rest.is_empty() || rest.starts_with(b"@"))
}

/// `panic!`, `unreachable!`, `todo!` or `unimplemented!` in `c`.
fn panics_in(c: &[u8]) -> bool {
    (0..c.len()).any(|i| {
        is_ident_start(c[i])
            && (i == 0 || !is_ident(c[i - 1]))
            && [&b"panic"[..], b"unreachable", b"todo", b"unimplemented"]
                .iter()
                .any(|w| c[i..].starts_with(w) && c.get(skip_ws(c, i + w.len())) == Some(&b'!'))
    })
}

/// Each arm of the `match` or `select!` body opened at `open`: where its
/// head starts, the head (guard dropped) and the span of its body.
fn arms(c: &[u8], open: usize) -> Vec<(usize, &[u8], (usize, usize))> {
    let close = matching(c, open);
    let mut out = Vec::new();
    let mut at = open + 1;
    while at < close {
        let Some(arrow) = (at..close.saturating_sub(1))
            .find(|&j| c[j] == b'=' && c[j + 1] == b'>' && depth0(&c[at..j]))
        else {
            break;
        };
        let pat = &c[at..arrow];
        let pat = match find_word_depth0(pat, b"if") {
            Some(g) => &pat[..g],
            None => pat,
        };
        let s = skip_ws(c, arrow + 2);
        let e = if c.get(s) == Some(&b'{') {
            matching(c, s) + 1
        } else {
            (s..close)
                .find(|&j| c[j] == b',' && depth0(&c[s..j]))
                .unwrap_or(close)
        };
        out.push((at, pat, (s, e)));
        at = skip_ws(c, e);
        if c.get(at) == Some(&b',') {
            at += 1;
        }
    }
    out
}

/// Past the `>` closing the generics opened at `open`.
fn generics_end(c: &[u8], open: usize) -> usize {
    let mut d = 0usize;
    for j in open..c.len() {
        match c[j] {
            b'<' => d += 1,
            // `->` in a `Fn() -> T` bound closes nothing.
            b'>' if c[j - 1] != b'-' => {
                d -= 1;
                if d == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
    }
    c.len()
}

/// `self`, `&self`, `&'a mut self`, `mut self`, `self: Box<Self>`.
fn is_self_param(p: &[u8]) -> bool {
    let p = p.strip_prefix(b"&").unwrap_or(p).trim_ascii_start();
    let p = match p.first() {
        Some(b'\'') => {
            p[1 + p[1..].iter().take_while(|&&x| is_ident(x)).count()..].trim_ascii_start()
        }
        _ => p,
    };
    let p = strip_word(p, b"mut").unwrap_or(p);
    p.starts_with(b"self") && !p.get(4).copied().is_some_and(is_ident)
}

/// Whether every bracket in `c` is closed.
fn depth0(c: &[u8]) -> bool {
    let mut d = 0i32;
    for &x in c {
        match x {
            b'(' | b'[' | b'{' => d += 1,
            b')' | b']' | b'}' => d -= 1,
            _ => {}
        }
    }
    d == 0
}

/// `c` split at each `sep` outside brackets.
fn split_top(c: &[u8], sep: u8) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let (mut d, mut from) = (0i32, 0);
    for (j, &x) in c.iter().enumerate() {
        match x {
            b'(' | b'[' | b'{' => d += 1,
            b')' | b']' | b'}' => d -= 1,
            _ if x == sep && d == 0 => {
                out.push(&c[from..j]);
                from = j + 1;
            }
            _ => {}
        }
    }
    out.push(&c[from..]);
    out
}

/// Where the word `w` starts in `c` outside brackets.
fn find_word_depth0(c: &[u8], w: &[u8]) -> Option<usize> {
    (0..c.len()).find(|&j| {
        c[j..].starts_with(w)
            && (j == 0 || !is_ident(c[j - 1]))
            && !c.get(j + w.len()).copied().is_some_and(is_ident)
            && depth0(&c[..j])
    })
}

/// The first `{` at or after `t` outside brackets.
fn brace_at_depth0(c: &[u8], t: usize) -> Option<usize> {
    let mut d = 0i32;
    for (j, &x) in c.iter().enumerate().skip(t) {
        match x {
            b'{' if d == 0 => return Some(j),
            b'(' | b'[' => d += 1,
            b')' | b']' => d -= 1,
            b';' | b'}' if d == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Where the receiver and path of the call named at `i` start:
/// `tokio::time::timeout`, `self.rx.lock().unwrap().recv_timeout`.
fn chain_start(c: &[u8], i: usize) -> usize {
    let mut s = i;
    loop {
        let before = c[..s].trim_ascii_end();
        let sep = if before.ends_with(b"::") {
            2
        } else if before.ends_with(b".") {
            1
        } else {
            return s;
        };
        let mut b = c[..before.len() - sep].trim_ascii_end();
        while let Some(&x @ (b')' | b']')) = b.last() {
            let (open, close) = if x == b')' { (b'(', x) } else { (b'[', x) };
            let mut d = 0usize;
            let mut k = b.len();
            while k > 0 {
                k -= 1;
                if b[k] == close {
                    d += 1;
                } else if b[k] == open {
                    d -= 1;
                    if d == 0 {
                        break;
                    }
                }
            }
            b = c[..k].trim_ascii_end();
        }
        let w = b.iter().rev().take_while(|&&x| is_ident(x)).count();
        if w == 0 {
            return s;
        }
        s = b.len() - w;
    }
}

/// Where the statement holding the end of `c` starts.
fn statement_start(c: &[u8]) -> usize {
    let mut d = 0i32;
    for j in (0..c.len()).rev() {
        match c[j] {
            b')' | b']' => d += 1,
            b'(' | b'[' => {
                d -= 1;
                if d < 0 {
                    return j + 1;
                }
            }
            b';' | b'{' | b'}' | b',' if d == 0 => return j + 1,
            b'>' if d == 0 && j > 0 && c[j - 1] == b'=' => return j + 1,
            _ => {}
        }
    }
    0
}

/// The first whole-word use of `name` from `from` to the end of its block,
/// not as a field.
fn next_use(c: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut d = 0usize;
    let mut j = from;
    while j < c.len() {
        match c[j] {
            b'{' => d += 1,
            b'}' if d == 0 => return None,
            b'}' => d -= 1,
            x if is_ident_start(x) && !is_ident(c[j - 1]) => {
                let e = ident_end(c, j);
                if &c[j..e] == name && c[..j].trim_ascii_end().last() != Some(&b'.') {
                    return Some(j);
                }
                j = e;
                continue;
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// The identifier starting at `i`, if one does.
fn word_at<'a>(c: &[u8], src: &'a str, i: usize) -> Option<&'a str> {
    c.get(i)
        .copied()
        .is_some_and(is_ident_start)
        .then(|| &src[i..ident_end(c, i)])
}

/// Past a `.` at `t` and the space after it, or `t`.
fn skip_dot(c: &[u8], t: usize) -> usize {
    if c.get(t) == Some(&b'.') {
        skip_ws(c, t + 1)
    } else {
        t
    }
}

/// The identifier `c` ends with.
fn last_word(c: &[u8]) -> &[u8] {
    let start = c.iter().rposition(|&x| !is_ident(x)).map_or(0, |p| p + 1);
    &c[start..]
}

/// `c` after a leading word `w` and the space after it.
fn strip_word<'a>(c: &'a [u8], w: &[u8]) -> Option<&'a [u8]> {
    c.strip_prefix(w)
        .filter(|r| !r.first().copied().is_some_and(is_ident))
        .map(<[u8]>::trim_ascii_start)
}

/// The key and kind of the fixed wait that the identifier `name`, ending at
/// `end`, calls in `ctx`, if it calls one; `timer` when the call bounds
/// another wait, as a timeout does.
fn fixed_wait(c: &[u8], name: &str, end: usize, ctx: Ctx, timer: bool) -> Option<(String, Kind)> {
    let open = skip_ws(c, end);
    if c.get(open) != Some(&b'(') {
        return None;
    }
    let close = matching(c, open);
    let args = &c[open + 1..close];
    let no_args = args.iter().all(u8::is_ascii_whitespace);
    let after = skip_ws(c, close + 1);
    let polling = matches!(ctx, Ctx::PollHead | Ctx::PollBody { .. });
    if is_sleep(name) && !timer {
        let kind = match ctx {
            Ctx::PollHead | Ctx::PollBody { .. } => return None,
            Ctx::For { exits: true } => Kind::BoundedPoll,
            Ctx::For { exits: false } => Kind::Pacing,
            Ctx::Straight => Kind::Fixed,
        };
        Some((format!("{name}({})", squeeze(args)), kind))
    } else if timer {
        let kind = match ctx {
            Ctx::PollBody { ends: false } => return None,
            Ctx::PollBody { ends: true } => Kind::LoopBound,
            Ctx::For { exits: true } => Kind::BoundedPoll,
            _ => Kind::Fixed,
        };
        let lits = literal_durations(args);
        (!lits.is_empty()).then(|| (format!("{name}({})", lits.join(",")), kind))
    } else if name == "now" && no_args && c.get(after) == Some(&b'+') {
        literal_duration_at(c, skip_ws(c, after + 1))
            .map(|(_, d)| (format!("now()+{d}"), Kind::Fixed))
    } else if name == "elapsed" && no_args && polling {
        let op = match c.get(after..after + 2)? {
            [x @ (b'<' | b'>'), b'='] => [*x, b'='].to_vec(),
            [x @ (b'<' | b'>'), _] => vec![*x],
            _ => return None,
        };
        literal_duration_at(c, skip_ws(c, after + op.len())).map(|(_, d)| {
            (
                format!("elapsed(){}{d}", String::from_utf8_lossy(&op)),
                Kind::Fixed,
            )
        })
    } else {
        None
    }
}

/// The waits `changes` add: every key the changed files hold more of now than
/// at the merge base, reported in each file whose own count of it rose.
pub fn added(changes: &[Change]) -> Vec<Added> {
    let per_file: Vec<(&str, Vec<Wait>, Vec<Wait>)> = changes
        .iter()
        .map(|c| (c.path.as_str(), waits(&c.base), waits(&c.now)))
        .collect();
    let mut at_base: HashMap<&str, usize> = HashMap::new();
    let mut now: HashMap<&str, usize> = HashMap::new();
    for (_, base, cur) in &per_file {
        for w in base {
            *at_base.entry(&w.key).or_default() += 1;
        }
        for w in cur {
            *now.entry(&w.key).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for (path, base, cur) in &per_file {
        for w in cur {
            let (b, n) = (
                at_base.get(w.key.as_str()).copied().unwrap_or(0),
                now[w.key.as_str()],
            );
            let count = |ws: &[Wait]| ws.iter().filter(|x| x.key == w.key).count();
            if n > b && count(cur) > count(base) {
                out.push(Added {
                    path: (*path).to_owned(),
                    line: w.line,
                    key: w.key.clone(),
                    kind: w.kind,
                    at_base: b,
                    now: n,
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    out
}

/// What the gate prints, and whether it passed.
pub struct Outcome {
    pub text: String,
    pub passed: bool,
}

/// Runs the gate in the repository holding the current directory, against
/// the merge base of `base` and `HEAD`, over the working tree (so a local run
/// sees uncommitted and untracked test files too).
pub fn check(base: &str) -> Result<Outcome, String> {
    let top = git(Path::new("."), &["rev-parse", "--show-toplevel"])?;
    let top = Path::new(top.trim());
    let mb = git(top, &["merge-base", base, "HEAD"])?;
    let mb = mb.trim();
    let mut paths: Vec<(String, bool)> = Vec::new();
    let diff = git(top, &["diff", "--no-renames", "--name-status", "-z", mb])?;
    let mut fields = diff.split('\0');
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        if path.ends_with(".rs") {
            paths.push((path.to_owned(), status.starts_with('A')));
        }
    }
    let untracked = git(top, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in untracked.split('\0').filter(|p| p.ends_with(".rs")) {
        paths.push((path.to_owned(), true));
    }
    // Which files are test modules depends on files the change may not touch,
    // so it is read from the whole tree, on each side.
    let (at_base, at_now) = if paths.iter().all(|(p, _)| is_test_source(p)) {
        Default::default()
    } else {
        (
            test_modules(&declaring_files(top, Some(mb))?),
            test_modules(&declaring_files(top, None)?),
        )
    };
    let mut changes = Vec::new();
    for (path, added) in paths {
        let base = if added {
            String::new()
        } else {
            git(top, &["show", &format!("{mb}:{path}")])?
        };
        let now = read(&top.join(&path))?;
        let change = Change {
            base: test_code(&path, &base, &at_base),
            now: test_code(&path, &now, &at_now),
            path,
        };
        if !(change.base.trim().is_empty() && change.now.trim().is_empty()) {
            changes.push(change);
        }
    }
    let short = &mb[..mb.len().min(9)];
    let added = added(&changes);
    let text = if added.is_empty() {
        format!(
            "fixed-wait-gate: no fixed wait added to a test since {short} ({} files with test code changed)\n",
            changes.len()
        )
    } else {
        let mut t = format!(
            "fixed-wait-gate: this change adds a fixed wait to a test (merge base {short}):\n"
        );
        for a in &added {
            let kind = match a.kind {
                Kind::Fixed => "",
                Kind::BoundedPoll => "; a for loop that breaks: a bounded poll",
                Kind::Pacing => "; a for loop that never breaks: pacing",
                Kind::LoopBound => "; its expiry ends the loop: the loop's bound",
            };
            t.push_str(&format!(
                "  {}:{}  {}  ({} at the merge base, {} now{kind})\n",
                a.path, a.line, a.key, a.at_base, a.now
            ));
        }
        t.push_str(RULE);
        t
    };
    Ok(Outcome {
        text,
        passed: added.is_empty(),
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every `.rs` file that may declare an out-of-line module, path to source:
/// at `rev`, or in the working tree (untracked files included) for `None`.
fn declaring_files(top: &Path, rev: Option<&str>) -> Result<BTreeMap<String, String>, String> {
    const DECL: &str = "mod[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*;";
    let args = match rev {
        Some(r) => vec!["grep", "-l", "-E", "-e", DECL, r, "--", "*.rs"],
        None => vec!["grep", "-l", "--untracked", "-E", "-e", DECL, "--", "*.rs"],
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(&args)
        .output()
        .map_err(|e| format!("git grep: {e}"))?;
    // 1 is "no match".
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(format!(
            "git grep: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let listed = String::from_utf8_lossy(&out.stdout);
    let paths: Vec<&str> = listed
        .lines()
        .map(|l| {
            rev.and_then(|r| l.strip_prefix(r)?.strip_prefix(':'))
                .unwrap_or(l)
        })
        .collect();
    match rev {
        None => paths
            .into_iter()
            .map(|p| Ok((p.to_owned(), read(&top.join(p))?)))
            .collect(),
        Some(r) => blobs(top, r, &paths),
    }
}

/// The files at `rev`, read through one `git cat-file --batch`.
fn blobs(top: &Path, rev: &str, paths: &[&str]) -> Result<BTreeMap<String, String>, String> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input: String = paths.iter().map(|p| format!("{rev}:{p}\n")).collect();
    // Written from its own thread: a large tree fills the output pipe before
    // the input is all sent.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_end(&mut out)
        .map_err(|e| format!("git cat-file: {e}"))?;
    writer
        .join()
        .expect("cat-file writer")
        .map_err(|e| format!("git cat-file: {e}"))?;
    child.wait().map_err(|e| format!("git cat-file: {e}"))?;
    let mut files = BTreeMap::new();
    let mut at = 0usize;
    for p in paths {
        let nl = at
            + out[at..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or("git cat-file: short output")?;
        let header = String::from_utf8_lossy(&out[at..nl]).into_owned();
        at = nl + 1;
        let Some(size) = header
            .split(' ')
            .nth(2)
            .and_then(|n| n.parse::<usize>().ok())
        else {
            continue;
        };
        let body = out.get(at..at + size).ok_or("git cat-file: short output")?;
        files.insert((*p).to_owned(), String::from_utf8_lossy(body).into_owned());
        at += size + 1;
    }
    Ok(files)
}

fn read(path: &Path) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        // A deleted file holds no wait now.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// `src` with comments, strings and character literals blanked (newlines
/// kept, so offsets and lines still match), and each line's `//` text.
struct Masked {
    code: Vec<u8>,
    line_starts: Vec<usize>,
    comments: Vec<String>,
}

impl Masked {
    fn line_of(&self, off: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= off) - 1
    }

    fn line_code(&self, line: usize) -> &[u8] {
        let end = self
            .line_starts
            .get(line + 1)
            .map_or(self.code.len(), |&s| s - 1);
        &self.code[self.line_starts[line]..end]
    }

    /// `marker`, with a reason, is on `line`, or on a comment-only line in the
    /// run directly above it.
    fn marked(&self, line: usize, marker: &str) -> bool {
        let has = |l: usize| {
            self.comments[l]
                .trim_start()
                .strip_prefix(marker)
                .is_some_and(|reason| !reason.trim().is_empty())
        };
        if has(line) {
            return true;
        }
        (0..line)
            .rev()
            .take_while(|&l| {
                !self.comments[l].is_empty()
                    && self.line_code(l).iter().all(u8::is_ascii_whitespace)
            })
            .any(has)
    }
}

fn mask(src: &str) -> Masked {
    let b = src.as_bytes();
    let mut code = b.to_vec();
    let mut line_starts = vec![0];
    line_starts.extend(
        b.iter()
            .enumerate()
            .filter(|(_, c)| **c == b'\n')
            .map(|(i, _)| i + 1),
    );
    let mut comments = vec![String::new(); line_starts.len()];
    let line_of = |off: usize| line_starts.partition_point(|&s| s <= off) - 1;
    let mut i = 0;
    while i < b.len() {
        let end = match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
                comments[line_of(i)].push_str(&src[i + 2..end]);
                end
            }
            b'/' if b.get(i + 1) == Some(&b'*') => block_comment_end(b, i),
            b'"' => string_end(b, i + 1),
            b'r' if raw_prefix_ok(b, i) => match raw_string_end(b, i + 1) {
                Some(end) => end,
                None => {
                    i += 1;
                    continue;
                }
            },
            b'\'' => match char_literal_end(b, i) {
                Some(end) => end,
                None => {
                    i += 1;
                    continue;
                }
            },
            _ => {
                i += 1;
                continue;
            }
        };
        for c in &mut code[i..end] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
        i = end;
    }
    Masked {
        code,
        line_starts,
        comments,
    }
}

fn block_comment_end(b: &[u8], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < b.len() {
        if b[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if b[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// Past the closing quote of a string whose body starts at `i`.
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

/// An `r` that starts a raw string: not inside an identifier, except as the
/// `br` / `cr` prefix.
fn raw_prefix_ok(b: &[u8], i: usize) -> bool {
    match i.checked_sub(1).map(|p| b[p]) {
        None => true,
        Some(b'b' | b'c') => i < 2 || !is_ident(b[i - 2]),
        Some(p) => !is_ident(p),
    }
}

/// Past the end of a raw string whose `#`s or quote start at `i`.
fn raw_string_end(b: &[u8], i: usize) -> Option<usize> {
    let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
    if b.get(i + hashes) != Some(&b'"') {
        return None;
    }
    let mut j = i + hashes + 1;
    while j < b.len() {
        if b[j] == b'"'
            && b[j + 1..]
                .iter()
                .take(hashes)
                .filter(|&&c| c == b'#')
                .count()
                == hashes
        {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(b.len())
}

/// Past the end of a character literal at `i`, or `None` for a lifetime.
fn char_literal_end(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i + 1)? {
        b'\\' => {
            let close = b.get(i + 3..)?.iter().position(|&c| c == b'\'')?;
            Some(i + 3 + close + 1)
        }
        &lead => {
            let len = match lead {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            (b.get(i + 1 + len) == Some(&b'\'')).then_some(i + 2 + len)
        }
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn ident_end(c: &[u8], i: usize) -> usize {
    i + c[i..].iter().take_while(|&&x| is_ident(x)).count()
}

fn skip_ws(c: &[u8], i: usize) -> usize {
    i + c.get(i..).map_or(0, |s| {
        s.iter().take_while(|x| x.is_ascii_whitespace()).count()
    })
}

/// Whether the `{` at `i` opens a closure body or an `async` block.
fn opens_closure(c: &[u8], i: usize) -> bool {
    let before = c[..i].trim_ascii_end();
    let last_word = |s: &[u8]| {
        let start = s.iter().rposition(|&x| !is_ident(x)).map_or(0, |p| p + 1);
        s[start..].to_vec()
    };
    match last_word(before).as_slice() {
        _ if before.ends_with(b"|") => true,
        b"async" => true,
        b"move" => last_word(before[..before.len() - 4].trim_ascii_end()) == b"async",
        _ => false,
    }
}

/// Whether the `{` at `i` opens the body of `select!` or `select_biased!`.
fn opens_select(c: &[u8], i: usize) -> bool {
    c[..i]
        .trim_ascii_end()
        .strip_suffix(b"!")
        .is_some_and(|b| matches!(last_word(b.trim_ascii_end()), b"select" | b"select_biased"))
}

/// A loop or block label `'name:` at `i`: the label, quote included, and
/// where it ends.
fn block_label<'a>(c: &[u8], src: &'a str, i: usize) -> Option<(&'a str, usize)> {
    let end = break_label(c, src, i)?.len() + i;
    let colon = skip_ws(c, end);
    (c.get(colon) == Some(&b':') && c.get(colon + 1) != Some(&b':'))
        .then_some((&src[i..end], colon + 1))
}

/// The label, quote included, that starts at `i`, as a `break` names it.
fn break_label<'a>(c: &[u8], src: &'a str, i: usize) -> Option<&'a str> {
    (c.get(i) == Some(&b'\'') && c.get(i + 1).copied().is_some_and(is_ident_start))
        .then(|| &src[i..ident_end(c, i + 1)])
}

/// Whether the identifier at `i` is the name in a `fn` item.
fn after_fn(c: &[u8], i: usize) -> bool {
    let before = c[..i].trim_ascii_end();
    before.ends_with(b"fn") && (before.len() == 2 || !is_ident(before[before.len() - 3]))
}

/// The `)` or `}` matching the `(` or `{` at `open`, or the end of the text.
fn matching(c: &[u8], open: usize) -> usize {
    let close = if c[open] == b'{' { b'}' } else { b')' };
    let mut depth = 0usize;
    for (j, &x) in c.iter().enumerate().skip(open) {
        match x {
            _ if x == c[open] => depth += 1,
            _ if x == close => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
    }
    c.len()
}

fn squeeze(c: &[u8]) -> String {
    String::from_utf8_lossy(c)
        .chars()
        .filter(|x| !x.is_whitespace())
        .collect()
}

fn is_sleep(name: &str) -> bool {
    name == "sleep" || name.starts_with("sleep_")
}

fn is_wait_name(name: &str) -> bool {
    name.split('_')
        .any(|s| matches!(s, "timeout" | "deadline" | "wait" | "until"))
}

/// Every `Duration::from_*(<number>)` in a call's arguments, outside any
/// `{ ... }` block an argument carries (an async block's own waits are found
/// on their own).
fn literal_durations(args: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            x if depth == 0
                && is_ident_start(x)
                && (i == 0 || !(is_ident(args[i - 1]) || args[i - 1] == b':')) =>
            {
                match literal_duration_at(args, i) {
                    Some((end, d)) => {
                        out.push(d);
                        i = end;
                    }
                    None => i = ident_end(args, i),
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// A `[path::]Duration::from_*(<number>)` starting at `i`: where it ends,
/// and the duration without its path.
fn literal_duration_at(c: &[u8], i: usize) -> Option<(usize, String)> {
    let mut segs = Vec::new();
    let mut j = i;
    loop {
        if !c.get(j).copied().is_some_and(is_ident_start) {
            return None;
        }
        let end = ident_end(c, j);
        segs.push(&c[j..end]);
        j = end;
        if c[j..].starts_with(b"::") {
            j += 2;
        } else {
            break;
        }
    }
    let [.., ty, ctor] = segs.as_slice() else {
        return None;
    };
    if *ty != b"Duration" || !ctor.starts_with(b"from_") {
        return None;
    }
    let open = skip_ws(c, j);
    if c.get(open) != Some(&b'(') {
        return None;
    }
    let close = matching(c, open);
    let inner = &c[open + 1..close.min(c.len())];
    if !is_number_expr(inner) {
        return None;
    }
    Some((
        close + 1,
        format!(
            "Duration::{}({})",
            String::from_utf8_lossy(ctor),
            squeeze(inner)
        ),
    ))
}

/// Numeric literals joined by arithmetic, and nothing else.
fn is_number_expr(c: &[u8]) -> bool {
    let mut saw_number = false;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            x if x.is_ascii_digit() => {
                saw_number = true;
                i += c[i..]
                    .iter()
                    .take_while(|&&y| is_ident(y) || y == b'.')
                    .count();
                continue;
            }
            b'+' | b'-' | b'*' | b'/' | b'%' | b'(' | b')' => {}
            x if x.is_ascii_whitespace() => {}
            _ => return false,
        }
        i += 1;
    }
    saw_number
}
