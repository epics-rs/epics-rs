//! **An errlog message MUST NOT carry an ANSI escape the console predicate did
//! not choose.**
//!
//! C strips the escapes at print time when its console is not a terminal —
//! `if(!ttyConsole) errlogStripANSI(base+1u)` (`errlog.c:789-793`), with
//! `pvt.ttyConsole = isATTY(pvt.console)` at `:555` and `isATTY` also demanding
//! a non-empty `$TERM` (`:218-237`). This port has no such strip on its console
//! path on purpose: `runtime::log::write_console` writes the caller's bytes
//! verbatim, exactly as `fprintf(console, "%s", base+1u)` does (`errlog.c:795`),
//! and the *call site* decides whether the word is painted by asking
//! `runtime::log::erl_warning()` or `runtime::log::errlog_console_paints()`.
//!
//! Owner of the decision: those two predicates, and nothing else. A raw
//! `\x1b[…` typed into an errlog format string, or a bare `ERL_ERROR` /
//! `ERL_WARNING` / `ANSI_ESC_*` handed to one, bypasses them — and then a
//! redirected stderr, an `iocLogServer` capture or a file gets escape bytes a C
//! IOC would have stripped. Two sites did exactly that before this guard
//! existed (`ioc_app.rs`'s `iocRun`/`iocPause` refusals, which spelled a plain
//! `WARNING` and so never painted at all).
//!
//! What this sees: escapes written *at* the errlog call. What it cannot see: a
//! value built elsewhere and passed by name — `scan.rs`'s over-run `warning` is
//! the one such site today, and it is built through `erl_warning()`.

use std::path::{Path, PathBuf};

/// The errlog entry points whose argument is the console's bytes.
const ENTRY_POINTS: [&str; 4] = [
    "errlog_printf(",
    "errlog_sev_printf(",
    "errlog_message(",
    "errlog_printf_no_console(",
];

/// Ways an escape can be spelled into a call argument, source-text form.
///
/// The last entry is a literal ESC byte, which is legal in a Rust string and
/// invisible in review — the reason to name it here rather than trust the
/// backslash spellings.
const BANNED: [&str; 7] = [
    "\\x1b",
    "\\u{1b}",
    "\\033",
    "\\e[",
    "ANSI_ESC_",
    "ERL_ERROR",
    "ERL_WARNING",
];

/// The predicate that makes an escape legitimate when it appears in the same
/// call — `erl_warning()` needs no exemption because it names no escape.
const PREDICATE: &str = "errlog_console_paints";

/// Floors that turn a scan of nothing into a failure rather than a pass: a
/// moved directory or a renamed entry point would otherwise report green
/// forever. Both are well under today's counts.
const MIN_FILES: usize = 500;
const MIN_SITES: usize = 30;

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Production source only: everything from a module-level `#[cfg(test)]` on is
/// test code, and `log.rs`'s own strip tests feed `errlog_printf` escapes on
/// purpose. The convention in this workspace is one such block, last in the
/// file; a file that puts production code after one is under-scanned, which the
/// site floor above is there to catch if it ever becomes the norm.
fn production_half(text: &str) -> &str {
    match text.find("\n#[cfg(test)]") {
        Some(at) => &text[..at],
        None => text,
    }
}

/// The text between an entry point's parentheses, by brace counting, so a
/// nested `format!(…)` comes with it.
fn call_argument(text: &str, open: usize) -> &str {
    let mut depth = 1usize;
    let bytes = text.as_bytes();
    let mut i = open;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    &text[open..i.saturating_sub(1).max(open)]
}

#[test]
fn every_errlog_escape_comes_from_the_console_predicate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/epics-libcom-rs")
        .to_path_buf();

    let mut files = Vec::new();
    for group in ["crates", "examples"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for entry in entries.flatten() {
            rust_files(&entry.path().join("src"), &mut files);
        }
    }
    files.sort();
    assert!(
        files.len() >= MIN_FILES,
        "the guard found only {} source files under {}; it is not scanning the \
         workspace",
        files.len(),
        root.display()
    );

    let mut sites = 0usize;
    let mut violations = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read workspace source");
        let text = production_half(&text);
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string();
        for entry in ENTRY_POINTS {
            let mut from = 0usize;
            while let Some(at) = text[from..].find(entry) {
                let open = from + at + entry.len();
                from = open;
                // `pub fn errlog_printf(` and friends: the definitions, not calls.
                if text[..from - entry.len()].trim_end().ends_with("fn") {
                    continue;
                }
                sites += 1;
                let arg = call_argument(text, open);
                if arg.contains(PREDICATE) {
                    continue;
                }
                let line = text[..open].matches('\n').count() + 1;
                for banned in BANNED {
                    if arg.contains(banned) || arg.contains('\u{1b}') {
                        violations.push(format!(
                            "{rel}:{line} passes `{banned}` to `{entry}…)` without asking \
                             `{PREDICATE}`"
                        ));
                        break;
                    }
                }
            }
        }
    }

    assert!(
        sites >= MIN_SITES,
        "the guard found only {sites} errlog call sites; the entry-point names \
         have moved and it is checking nothing"
    );
    assert!(
        violations.is_empty(),
        "an errlog message must not carry an ANSI escape the console predicate \
         did not choose — C strips them when its console is not a terminal \
         (`errlog.c:789-793`) and this port's console writes verbatim, so the \
         call site must ask `runtime::log::erl_warning()` or \
         `runtime::log::{PREDICATE}()`.\n\n{} site(s) breach it:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

/// The guard's own boundaries: each way of breaching the rule fires, and each
/// way of satisfying it does not. The fixtures are assembled with `concat!` so
/// this file cannot match itself if the live scan is ever widened to `tests/`.
#[test]
fn the_guard_fires_on_a_breach_and_passes_the_predicate() {
    let esc = concat!("\\x", "1b");
    let call = |arg: &str| {
        let src = format!("    errlog_printf(&format!({arg}));\n");
        let open = src.find("errlog_printf(").unwrap() + "errlog_printf(".len();
        let arg = call_argument(&src, open).to_string();
        let banned = BANNED
            .iter()
            .any(|b| arg.contains(b) || arg.contains('\u{1b}'));
        banned && !arg.contains(PREDICATE)
    };

    assert!(
        call(&format!("\"{esc}[35;1mWARNING{esc}[0m\"")),
        "raw escape"
    );
    assert!(
        call("\"{ERL_ERROR} iocBuild: asInit Failed.\\n\""),
        "constant"
    );
    assert!(
        !call("\"iocRun: {} IOC not paused\\n\", erl_warning()"),
        "predicate"
    );
    assert!(!call("\"Starting iocInit\\n\""), "plain text");
    assert!(
        !call("\"{a}x{b}\", if errlog_console_paints() { ERL_ERROR } else { \"ERROR\" }"),
        "an inline predicate makes the constant legitimate"
    );

    // The `#[cfg(test)]` cut, which is what keeps `log.rs`'s strip fixtures out.
    let cut = concat!("\n#[cfg", "(test)]\nmod tests { errlog_printf(\"x\"); }");
    assert_eq!(production_half(&format!("fn a() {{}}{cut}")), "fn a() {}");
}
