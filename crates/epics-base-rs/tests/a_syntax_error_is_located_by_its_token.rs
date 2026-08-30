//! **A `.db` parse abort is located by C's `yytext`, and worded as bison words
//! it.**
//!
//! `dbYacc.y` declares no `%error-verbose` and no per-production message, so
//! every grammar rejection in C prints the same three words —
//! `yyerror("syntax error")` — and the ` at or before '%s'` clause that follows
//! quotes `yytext`, the token the lexer had matched (`dbYacc.y:370-383`). The
//! token is the whole of what tells one rejection from another.
//!
//! This port wrote its own sentence and its own locator: `ERROR: expected ')',
//! got '}'` over ` at or before column 1`. Both halves diverged, and the second
//! is the one that costs an operator something — a column counts characters in
//! a line the echo below already prints, where C names the text.
//!
//! Every expected byte below was captured from `softIoc` built from
//! `~/work/epics-base` (`R7.0.10`) loading these exact files, with two known
//! subtractions noted per case: C's `WARNING: dbReadCOM: Parser stack dirty
//! w/o error. 1` counts half-built objects on a `tempList` this parser does not
//! have, and C's `in path "."` names its own cwd where this gate names the
//! directory it wrote the fixture into.
//!
//! ANSI is stripped before comparing; whether the escapes are emitted has its
//! own gate. Unix only: what is captured is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

/// Run one iocsh line with fd 2 pointed at a file, and give back what it wrote.
fn stderr_of(line: &str, db: Arc<PvDatabase>) -> String {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let owned = line.to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&owned)).join();

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the shell thread").ok();

    strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"))
}

/// C's SGR sequences, dropped — `errlog` strips its own the same way when the
/// console is not a terminal (`errlog.c:672-681`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// One case per token SHAPE the locator has to be able to name, not one per
/// malformed-file story: a punctuation character, a quoted string WITH its
/// quotes, a bareword, and the two arity boundaries of one argument list.
///
///   * `punct` — `record(ai, "A1" {`: the offending token is `{`;
///   * `quoted` — `field(DESC "x")`: C quotes `"x"`, the whole `tokenSTRING`
///     the lexer matched, where naming the character would say `"`;
///   * `bareword_kw` — `recrd(...)`: the unknown keyword itself;
///   * `bareword_val` — `field(DESC, x y)`: the second bareword, mid-value;
///   * `next_line` — a field left unclosed, so the token that rejects is a `}`
///     one LINE below and the position must follow it there;
///   * `too_many` — `record(ai, "", extra)`: C's grammar has one production per
///     arity, so the rejection is at the COMMA that would open the third
///     argument and the empty name is never reached;
///   * `too_few` — `alias("C8")` at file scope: the `)` that ended a
///     two-argument list after one.
const CASES: &[(&str, &str, &str, u32)] = &[
    ("punct.db", "record(ai, \"A1\" {\n}\n", "{", 1),
    (
        "quoted.db",
        "record(ai, \"A2\") {\n    field(DESC \"x\")\n}\n",
        "\"x\"",
        2,
    ),
    ("bareword_kw.db", "recrd(ai, \"A5\") {\n}\n", "recrd", 1),
    (
        "bareword_val.db",
        "record(ai, \"A6\") {\n    field(DESC, x y)\n}\n",
        "y",
        2,
    ),
    (
        "next_line.db",
        "record(ai, \"A4\") {\n    field(DESC, \"x\"\n}\n",
        "}",
        3,
    ),
    ("too_many.db", "record(ai, \"\", extra) {\n}\n", ",", 1),
    ("too_few.db", "alias(\"C8\")\n", ")", 1),
];

/// The whole of what C writes for each shape, byte for byte.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn every_rejection_says_syntax_error_and_names_its_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body, _, _) in CASES {
        std::fs::write(dir.path().join(name), body).expect("write the .db");
    }
    let found_under = dir.path().display().to_string();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };

    for (name, body, token, line) in CASES {
        let got = stderr_of(
            &format!("dbLoadRecords(\"{name}\")"),
            Arc::new(PvDatabase::new()),
        );
        let source = body
            .lines()
            .nth(*line as usize - 1)
            .expect("the echoed line");
        let want = format!(
            "\
ERROR: syntax error
 at or before '{token}' in path \"{found_under}\"  file \"{name}\" line {line}

 {line} | {source}

ERROR: Failed to load '{name}'
"
        );
        assert_eq!(
            got, want,
            "{name}\n--- got ---\n{got}\n--- want ---\n{want}"
        );
    }

    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
}

/// The boundary the arity move must NOT cross: a list with exactly `max`
/// arguments still parses, and one with exactly `min` still parses.
///
/// The rejection moved from "after the `)`, counting what was collected" to
/// "at the comma that opens one argument too many", which is a much earlier
/// gate; these are the two counts either side of it.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn the_arity_boundaries_themselves_still_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("ok.db"),
        "record(ai, \"OK1\") {\n    alias(\"OK1:A\")\n}\nalias(\"OK1\", \"OK1:B\")\n",
    )
    .expect("write the .db");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of("dbLoadRecords(\"ok.db\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    assert_eq!(got, "", "a well-formed file must say nothing: {got}");
}
