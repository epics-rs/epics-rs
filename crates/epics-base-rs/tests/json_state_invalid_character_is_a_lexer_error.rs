//! **A character the JSON start condition cannot lex is reported by the lexer,
//! one line each, before the grammar's `syntax error`.**
//!
//! Inside `field(X, <value>)` C switches `dbLex.l` into its `JSON` start
//! condition, whose rules know only punctuation, strings, numbers and barewords
//! (`dbLex.l:99-125`). Any other character falls to the catch-all
//! (`dbLex.l:143-155`): `yyerrorAbort("Invalid character '%c'")`, which sets
//! `yyAbort` and goes on lexing the REST of the buffered line, one message per
//! character. The next line read returns EOF (`dbLexRoutines.c:374`), and the
//! grammar's `syntax error` is what the truncated record then draws. No yajl is
//! involved: the text never reaches `dbJLinkParse`.
//!
//! Expected bytes are `softIoc -d` stderr for these exact files (R7.0.10), ANSI
//! stripped, with C's cwd `in path "."` replaced by the fixture directory.

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

/// (file, the value on line 2, the diagnostics in order). Only the first
/// diagnostic quotes its token: the second and later get the echo alone.
type Case = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const SYNTAX: &str = "syntax error";

const CASES: &[Case] = &[
    // `)` after `:` — the only JSON-state character here, so one message.
    (
        "colon_paren.db",
        r#"{"const":)"#,
        &[("Invalid character ')'", ")"), (SYNTAX, "")],
    ),
    // `/` and `*` are both refused; the `}` after them still closes the object.
    (
        "slash_star.db",
        r#"{"const":1 /*})"#,
        &[
            ("Invalid character '/'", "/"),
            ("Invalid character '*'", ""),
            (SYNTAX, ""),
        ],
    ),
    (
        "open_slash_star.db",
        "{/*})",
        &[
            ("Invalid character '/'", "/"),
            ("Invalid character '*'", ""),
            (SYNTAX, ""),
        ],
    ),
    // A quote with no partner on the line is not a string: the catch-all
    // refuses it and the rest of the line is lexed as barewords.
    (
        "unterminated_dq.db",
        r#"{"const":"abc})"#,
        &[("Invalid character '\"'", "\""), (SYNTAX, "")],
    ),
    (
        "unterminated_dq_sq.db",
        r#"{"const":"abc'})"#,
        &[
            ("Invalid character '\"'", "\""),
            ("Invalid character '''", ""),
            (SYNTAX, ""),
        ],
    ),
    // A closed string with a character `jsonstr` excludes: the error rule.
    (
        "bad_escape.db",
        r#"{"const":"a\1b"})"#,
        &[("Bad character in JSON string", r#""a\1b""#), (SYNTAX, "")],
    ),
    (
        "bad_tab.db",
        "{\"const\":\"a\tb\"})",
        &[("Bad character in JSON string", "\"a\tb\""), (SYNTAX, "")],
    ),
    // The grammar rejects first, so the characters after the offending
    // token are never lexed and never reported.
    ("grammar_first.db", r#"{"a" "b" /*})"#, &[(SYNTAX, "\"b\"")]),
    (
        "grammar_first_at.db",
        r#"{"a":1 "b" @})"#,
        &[(SYNTAX, "\"b\"")],
    ),
    (
        "grammar_double_comma.db",
        r#"{"a":1,, @})"#,
        &[(SYNTAX, ",")],
    ),
    // A value that is not a brace or bracket is the same `json_value`: a quote
    // with no partner is refused, and the barechar run after it is the value.
    (
        "top_dq_eol.db",
        r#""abc"#,
        &[("Invalid character '\"'", "\""), (SYNTAX, "")],
    ),
    (
        "top_sq_eol.db",
        "'abc",
        &[("Invalid character '''", "'"), (SYNTAX, "")],
    ),
    (
        "top_dq_close.db",
        r#""abc)"#,
        &[("Invalid character '\"'", "\""), (SYNTAX, "")],
    ),
    // The value is complete, so the token after it is lexed by the `INITIAL`
    // rules: `\` is refused, and the quote that follows has a newline, not a
    // partner, which is the `INITIAL` error rule.
    (
        "top_dq_esc_q.db",
        r#""ab\"c)"#,
        &[
            ("Invalid character '\"'", "\""),
            ("Invalid character '\\'", ""),
            ("Newline in string, closing quote missing", ""),
            (SYNTAX, ""),
        ],
    ),
    // A closed string `jsonstr` refuses is the error rule, and the `)` after
    // it is still JSON state: the dropped token left no value.
    (
        "top_bad_esc.db",
        r#""a\1b")"#,
        &[
            ("Bad character in JSON string", r#""a\1b""#),
            ("Invalid character ')'", ""),
            (SYNTAX, ""),
        ],
    ),
    (
        "top_bad_tab.db",
        "\"a\tb\")",
        &[
            ("Bad character in JSON string", "\"a\tb\""),
            ("Invalid character ')'", ""),
            (SYNTAX, ""),
        ],
    ),
    (
        "top_bad_esc_junk.db",
        r#""a\1b") @ "q"#,
        &[
            ("Bad character in JSON string", r#""a\1b""#),
            ("Invalid character ')'", ""),
            ("Invalid character '@'", ""),
            ("Invalid character '\"'", ""),
            (SYNTAX, ""),
        ],
    ),
    // After a complete bareword, brace or bracket value.
    (
        "bare_then_at.db",
        "abc@def)",
        &[("Invalid character '@'", "@"), (SYNTAX, "")],
    ),
    (
        "bare_junk_quote.db",
        r#"abc @ "q)"#,
        &[
            ("Invalid character '@'", "@"),
            ("Newline in string, closing quote missing", ""),
            (SYNTAX, ""),
        ],
    ),
    (
        "obj_then_quote.db",
        r#"{"a":1} "q)"#,
        &[
            ("Newline in string, closing quote missing", "\"q)\n"),
            (SYNTAX, ""),
        ],
    ),
    (
        "obj_then_junk_quote.db",
        r#"{"a":1} @ "q)"#,
        &[
            ("Invalid character '@'", "@"),
            ("Newline in string, closing quote missing", ""),
            (SYNTAX, ""),
        ],
    ),
    (
        "arr_fault_then_quote.db",
        r#"[1,@] "q)"#,
        &[
            ("Invalid character '@'", "@"),
            ("Newline in string, closing quote missing", ""),
            (SYNTAX, ""),
        ],
    ),
];

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn json_state_refusals_print_what_the_lexer_and_grammar_print() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = |value: &str| format!("record(ai, \"A4\") {{\n    field(INP, {value}\n}}\n");
    for (name, value, _) in CASES {
        std::fs::write(dir.path().join(name), file(value)).expect("write the .db");
    }
    let found_under = dir.path().display().to_string();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };

    for (name, value, diagnostics) in CASES {
        let got = stderr_of(
            &format!("dbLoadRecords(\"{name}\")"),
            Arc::new(PvDatabase::new()),
        );
        let source = format!("    field(INP, {value}");
        let mut want = String::new();
        for (i, (message, token)) in diagnostics.iter().enumerate() {
            want += &format!("ERROR: {message}\n");
            if i == 0 {
                want += &format!(
                    " at or before '{token}' in path \"{found_under}\"  file \"{name}\" line 2\n"
                );
            }
            want += &format!("\n 2 | {source}\n\n");
        }
        // No lexer refusal means no `yyAbort`, so `dbReadCOM` finds the
        // record's temporary still pushed (`dbLexRoutines.c:303-306`).
        if diagnostics.iter().all(|(message, _)| *message == SYNTAX) {
            want += "WARNING: dbReadCOM: Parser stack dirty w/o error. 1\n";
        }
        want += &format!("ERROR: Failed to load '{name}'\n");
        assert_eq!(
            got, want,
            "{name}\n--- got ---\n{got}\n--- want ---\n{want}"
        );
    }

    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
}
