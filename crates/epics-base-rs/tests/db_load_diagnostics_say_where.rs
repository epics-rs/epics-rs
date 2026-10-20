//! **A `.db` diagnostic MUST say where in the operator's file it happened.**
//!
//! C answers "where" three times for every bad line, and this port answered it
//! none of the three. Measured on base's own `compressTest.db`
//! (`modules/database/test/std/rec/compressTest.db`), loaded by `softIoc` and
//! by `softioc-rs` from the same directory: C wrote 36 stderr lines, the port
//! wrote 9, and all 21 missing ones were position:
//!
//! 1. macLib's own notice, `macLib: macro INP is undefined (expanding string
//!    …)` — `errlogPrintf` in `macCore.c:897-900`, raised inside the expander
//!    for every unresolved reference;
//! 2. the loader's per-line warning, `WARNING: '<file>' line <N> has undefined
//!    macros` — `db_yyinput` (`dbLexRoutines.c:384-387`), raised where the line
//!    is read;
//! 3. `yyerror`'s position context (`dbYacc.y:374-381`): ` at or before
//!    '<yytext>'` plus `dbIncludePrint` for the FIRST diagnostic of a text, a
//!    bare `ERROR: ` for every one after it, and always the ` <N> | <source>`
//!    echo with a blank line under it.
//!
//! Nothing runs wrong without them — the load fails either way — so the cost
//! falls entirely on the operator holding a real `.db` and an error with no
//! line number in it.
//!
//! The assertion is the whole stderr, not a `contains`: the wording, the two
//! spaces in `ERROR:  at or before`, the bare `ERROR: ` on the second and later
//! diagnostics and the blank line under each echo are what an eye and a log
//! parser match on, and a `contains` cannot fail when one of them moves.
//!
//! ANSI is stripped before comparing. Whether the escapes are emitted is a
//! property of the console predicate, which has its own gate
//! (`errlog_escapes_come_from_the_console_predicate`); what is asserted here is
//! the text and its position.
//!
//! Unix only: what is captured is the process console, and the only way to
//! capture it is to point fd 2 somewhere else and put it back. There is no
//! fd 2 on Windows, and `libc` is a `cfg(unix)` dependency of this crate.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

/// Base's own `compressTest.db`, byte for byte. Four `$(…)` references with no
/// substitution, on lines 7, 8, 10 and 11 — the case that produced the
/// measurement above.
const COMPRESS_TEST_DB: &str = r#"record(ai, "ai") {}
record(waveform, "wf") {
  field(FTVL, "DOUBLE")
  field(NELM, "4")
}
record(compress, "comp") {
  field(INP, "$(INP) NPP")
  field(ALG, "$(ALG)")
  field(PBUF,"$(PBUF=NO)")
  field(BALG,"$(BALG)")
  field(NSAM,"$(NSAM)")
  field(N,   "$(N=1)")
}
"#;

/// Run one iocsh line with fd 2 pointed at a file, and give back what it
/// wrote.
///
/// The two families this gates reach the operator by two different routes —
/// `eprintln!` for the loader's own lines, `errlogPrintf`'s console fallback
/// for macLib's — and the only place they are one stream again is the process
/// console. So the console is what is captured: a listener would see one of
/// them and a `tracing` subscriber the other, and neither could say what order
/// the operator reads them in.
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
    let ran = std::thread::spawn(move || {
        // A script line runs inside `iocshBody`, which may block (`iocsh.cpp:1122`).
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        IocShell::new(db, bridge).execute_line(&owned)
    })
    .join();

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the shell thread").ok();

    strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"))
}

/// C's SGR sequences, dropped. `errlog` strips its own the same way when the
/// console is not a terminal (`errlog.c:672-681`); this is the test's copy so
/// the gate holds whichever way the harness's console answers.
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

/// The whole of C's answer to "where", on the file that produced the
/// measurement.
///
/// Each line's read-time notices sit between the previous line's refusals and
/// its own, where C's lexer reads it; captured from `softIoc` R7.0.10.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn every_db_diagnostic_names_its_file_and_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("compressTest.db"), COMPRESS_TEST_DB).expect("write the .db");
    let found_under = dir.path().display().to_string();

    // A BARE name resolved through the search path, which is how an `st.cmd`
    // names a file and the only way C's `inputFile.path` is ever set — the
    // ` in path "…" ` half of `dbIncludePrint` exists for exactly this shape.
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let got = stderr_of(
        "dbLoadRecords(\"compressTest.db\")",
        Arc::new(PvDatabase::new()),
    );
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = format!(
        "\
macLib: macro INP is undefined (expanding string   field(INP, \"$(INP) NPP\")
)
WARNING: 'compressTest.db' line 7 has undefined macros
comp.INP Has unexpanded macro
ERROR: Can't set 'comp.INP' to '$(INP,undefined) NPP'  : Bad Field value
ERROR:  at or before ')' in path \"{found_under}\"  file \"compressTest.db\" line 7

 7 |   field(INP, \"$(INP,undefined) NPP\")

macLib: macro ALG is undefined (expanding string   field(ALG, \"$(ALG)\")
)
WARNING: 'compressTest.db' line 8 has undefined macros
comp.ALG Has unexpanded macro
ERROR: Can't set 'comp.ALG' to '$(ALG,undefined)'  : Bad Field value
    Did you mean \"Average\"?
ERROR: 
 8 |   field(ALG, \"$(ALG,undefined)\")

macLib: macro BALG is undefined (expanding string   field(BALG,\"$(BALG)\")
)
WARNING: 'compressTest.db' line 10 has undefined macros
comp.BALG Has unexpanded macro
ERROR: Can't set 'comp.BALG' to '$(BALG,undefined)'  : Bad Field value
    Did you mean \"FIFO Buffer\"?
ERROR: 
 10 |   field(BALG,\"$(BALG,undefined)\")

macLib: macro NSAM is undefined (expanding string   field(NSAM,\"$(NSAM)\")
)
WARNING: 'compressTest.db' line 11 has undefined macros
comp.NSAM Has unexpanded macro
ERROR: Can't set 'comp.NSAM' to '$(NSAM,undefined)'  : Bad Field value
ERROR: 
 11 |   field(NSAM,\"$(NSAM,undefined)\")

ERROR: Failed to load 'compressTest.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The suppression knob's exact reach: `var dbQuietMacroWarnings 1`.
///
/// C hands it to `macSuppressWarning`, and `trans` sets `entry->error`
/// BEFORE it consults `FLAG_SUPPRESS_WARNINGS` (`macCore.c:912-928`) — so
/// the knob drops macLib's own notice and shortens the placeholder from
/// `$(NAME,undefined)` to `$(NAME)`, and the loader's per-line
/// `WARNING: … has undefined macros` still prints, because
/// `macExpandString` still returns a negative length. Measured on
/// `softIoc` with the same file: 36 lines become 28, and all four
/// warnings are among the 28.
///
/// The shorter placeholder is why this is not a logging switch: it is the
/// text the refusal quotes, and it is the text `dbPutStringSuggest`
/// scores, which is how `$(ALG)` earns `N to 1 Average` where
/// `$(ALG,undefined)` earned `Average`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn quiet_macro_warnings_drops_maclibs_notice_and_keeps_the_loaders() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("compressTest.db"), COMPRESS_TEST_DB).expect("write the .db");
    let found_under = dir.path().display().to_string();

    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    epics_base_rs::server::db_loader::set_db_quiet_macro_warnings(true);
    let got = stderr_of(
        "dbLoadRecords(\"compressTest.db\")",
        Arc::new(PvDatabase::new()),
    );
    epics_base_rs::server::db_loader::set_db_quiet_macro_warnings(false);
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = format!(
        "\
WARNING: 'compressTest.db' line 7 has undefined macros
comp.INP Has unexpanded macro
ERROR: Can't set 'comp.INP' to '$(INP) NPP'  : Bad Field value
ERROR:  at or before ')' in path \"{found_under}\"  file \"compressTest.db\" line 7

 7 |   field(INP, \"$(INP) NPP\")

WARNING: 'compressTest.db' line 8 has undefined macros
comp.ALG Has unexpanded macro
ERROR: Can't set 'comp.ALG' to '$(ALG)'  : Bad Field value
    Did you mean \"N to 1 Average\"?
ERROR: 
 8 |   field(ALG, \"$(ALG)\")

WARNING: 'compressTest.db' line 10 has undefined macros
comp.BALG Has unexpanded macro
ERROR: Can't set 'comp.BALG' to '$(BALG)'  : Bad Field value
    Did you mean \"FIFO Buffer\"?
ERROR: 
 10 |   field(BALG,\"$(BALG)\")

WARNING: 'compressTest.db' line 11 has undefined macros
comp.NSAM Has unexpanded macro
ERROR: Can't set 'comp.NSAM' to '$(NSAM)'  : Bad Field value
ERROR: 
 11 |   field(NSAM,\"$(NSAM)\")

ERROR: Failed to load 'compressTest.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The knob is reachable the only way C makes it reachable — `var` — and
/// it reaches the global the loader actually reads, not a copy the table
/// echoes.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn var_reaches_the_quiet_knob_the_loader_reads() {
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    std::thread::spawn(move || {
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        shell
            .execute_line("var dbQuietMacroWarnings 1")
            .expect("`var` knows the name");
        assert!(
            epics_base_rs::server::db_loader::db_quiet_macro_warnings(),
            "the shell's `var` has to write the loader's own global"
        );
        shell
            .execute_line("var dbQuietMacroWarnings 0")
            .expect("`var` knows the name");
        assert!(!epics_base_rs::server::db_loader::db_quiet_macro_warnings());
    })
    .join()
    .expect("the shell thread");
}

/// The abort class, which is the far more common operator experience: a
/// genuine syntax error in a real `.db`.
///
/// C reaches it through the SAME `yyerror` as every recovered fault, only
/// through the arm that carries a message (`dbYacc.y:372-373`) — `yyparse`
/// calls it with `"syntax error"`, `dbLex.l` with `"Invalid character '%c'"`
/// — so an abort names its file, its line and its source exactly as a
/// recovered fault does. Measured on this file, `softIoc` writes:
///
/// ```text
/// ERROR: syntax error
///  at or before 'qqq' in path "."  file "syn.db" line 3
///
///  3 |   qqq
///
/// WARNING: dbReadCOM: Parser stack dirty w/o error. 1
/// ERROR: Failed to load 'syn.db'
/// ```
///
/// The port wrote one line, `parse error: DB parse error at line 3, column 6:
/// expected 'field', got 'qqq'`, and it named neither the file nor the source
/// — the harm this whole gate exists for, on the input an operator hits most.
///
/// Two of the three deviations it then had are gone. The message is bison's
/// own `syntax error` — `dbYacc.y` declares no `%error-verbose`, so every
/// grammar rejection prints those three words and the token is what tells
/// them apart — and the ` at or before` clause quotes that token rather than
/// a column of this port's invention.
///
/// The abort falls inside `S1`'s body, so C's `tempList` still holds it and
/// `dbReadCOM` follows with `Parser stack dirty` (`dbLexRoutines.c:303-306`).
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_syntax_error_names_its_file_line_and_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("syn.db"),
        "record(ai, \"S1\") {\n  field(DESC, \"ok\")\n  qqq\n}\n",
    )
    .expect("write the .db");
    let found_under = dir.path().display().to_string();

    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let got = stderr_of("dbLoadRecords(\"syn.db\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = format!(
        "\
ERROR: syntax error
 at or before 'qqq' in path \"{found_under}\"  file \"syn.db\" line 3

 3 |   qqq

WARNING: dbReadCOM: Parser stack dirty w/o error. 1
ERROR: Failed to load 'syn.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// Run `dbLoadRecords("p.db")` over `parent` beside `ADBase.template` (one
/// `ai` record `CHILD`), with the directory as the include path.
fn stderr_with_adbase(parent: &str) -> (String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("ADBase.template"),
        "record(ai, \"CHILD\") {}\n",
    )
    .expect("write ADBase.template");
    std::fs::write(dir.path().join("p.db"), parent).expect("write p.db");
    let found_under = dir.path().display().to_string();

    // SAFETY: the process-global env is why the callers are `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let got = stderr_of("dbLoadRecords(\"p.db\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    (got, found_under)
}

/// C pushes the included file when the grammar reduces `include`, while the
/// rest of the line is still in the lexer's buffer: a token there is located
/// in the included file at its line 0, echoing the including line. Measured
/// on `softIoc` R7.0.10.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_word_after_an_include_is_located_in_the_included_file() {
    let (got, found_under) = stderr_with_adbase("include ADBase.template junk\n");
    let want = format!(
        "\
ERROR: syntax error
 at or before 'junk' in path \"{found_under}\"  file \"ADBase.template\" line 0
 in path \"{found_under}\"  file \"p.db\" line 1

 0 | include ADBase.template junk

ERROR: Failed to load 'p.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The included file is read before the line after the include, so a record
/// body opened on the include's line meets the included file's text first.
/// Measured on `softIoc` R7.0.10.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_included_file_is_read_before_the_next_line() {
    let (got, found_under) =
        stderr_with_adbase("include ADBase.template record(ai, \"P\") {\n field(DESC, \"x\") }\n");
    let want = format!(
        "\
ERROR: syntax error
 at or before 'record' in path \"{found_under}\"  file \"ADBase.template\" line 1
 in path \"{found_under}\"  file \"p.db\" line 1

 1 | record(ai, \"CHILD\") {{}}

WARNING: dbReadCOM: Parser stack dirty w/o error. 1
ERROR: Failed to load 'p.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The other boundary: an abort that has no line in ANY file.
///
/// The include layer raises these before a text exists to locate them in —
/// a circular `include`, a file that resolved but could not be read — and
/// says so by carrying `line: 0`. They go through the same owner, so the
/// message reaches the operator instead of being returned as a value nobody
/// prints; what they must NOT do is invent a position, and what they must
/// name is the file the operator wrote.
///
/// C has no cycle detector at all here: it recurses to `dbIncludePrint`'s
/// depth and writes the whole stack, a million lines of it, for this input.
/// Refusing the cycle is an intended deviation; the shape of the refusal is
/// what is gated.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_abort_with_no_line_prints_the_message_and_no_position() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.db"), "include \"b.db\"\n").expect("write a.db");
    std::fs::write(dir.path().join("b.db"), "include \"a.db\"\n").expect("write b.db");
    let found_under = dir.path().display().to_string();

    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let got = stderr_of("dbLoadRecords(\"a.db\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
ERROR: circular include: a.db -> b.db -> a.db
ERROR: Failed to load 'a.db'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// macLib's third error arm, `recursive`, which this port did not have.
///
/// It is reachable from a `.db` load and it is not exotic: `dbLoadTemplate`
/// hands a row's `A="$(B)", B="$(A)"` straight to `dbLoadRecords`, where
/// `macParseDefns` stores the two raw values and the first expansion walks
/// them into each other. Measured on `softIoc` with this exact pair, C writes
///
/// ```text
/// macLib: macro A is recursive (expanding macro B)
/// macLib: macro B is recursive (expanding macro A)
/// WARNING: 'rec.db' line 2 has undefined macros
/// ```
///
/// and the field refusal below quotes `$(B,recursive)`. The port wrote none
/// of the three: the recursive arm resolved the reference to the raw value
/// instead of refusing it, so the cycle broke silently, the placeholder never
/// said why, and `MacroExpansion` recorded nothing for the per-line warning
/// to fire on.
///
/// Both notices, their order and the placeholder are now the engine's own:
/// it runs C's `expand()` pass over the macro table before it looks at the
/// caller's string (`macCore.c:646-679`), so the cycle is found once per
/// table ENTRY — naming both ends, seated on the entry and not on the
/// string — and `$(A)` copies the value that pass built, which is
/// `$(B,recursive)`.
///
/// The COUNT is the assertion. C creates one `MAC_HANDLE` per `.db` and
/// expands the table on first use (`dbReadCOM`,
/// `dbLexRoutines.c:256-300`), so the pair is printed once for the whole
/// file however many lines mention the cycle; measured on `softIoc
/// R7.0.10` with these exact two files, C's whole stream is
///
/// ```text
/// macLib: macro A is recursive (expanding macro B)
/// macLib: macro B is recursive (expanding macro A)
/// WARNING: 'rec.db' line 2 has undefined macros
/// C1.DESC Has unexpanded macro
/// ERROR: Can't set 'C1.DESC' to '$(B,recursive)'  : Bad Field value
/// ```
///
/// This port's file reader used to build a `MacroTable` per LINE, so the
/// pair repeated three times for three lines; the reader now holds one
/// for the whole include tree, which is why the count here
/// is C's.
///
/// The load is driven by `dbLoadTemplate`, so the refusal is a ROW's, and C
/// closes it with `msiLoadRecords`' echo of the call it made and
/// `yyerror("Error while reading included file")` before `YYABORT`
/// (`dbLoadTemplate.y:49-57`). Measured, C's last three lines here are
///
/// ```text
/// dbLoadRecords("rec.db", A="$(B)",B="$(A)")
/// Substitution file error: Error while reading included file
/// line 2: '}'
/// ```
///
/// All three of those lines are matched, the last one being the lexer parked
/// on the brace that closed the failing row.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_recursive_macro_is_refused_and_said_out_loud() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("rec.db"),
        "record(ai, \"C1\") {\n  field(DESC, \"$(A)\")\n}\n",
    )
    .expect("write the .db");
    std::fs::write(
        dir.path().join("rec.sub"),
        "file rec.db {\n{ A=\"$(B)\", B=\"$(A)\" }\n}\n",
    )
    .expect("write the .substitutions");
    let found_under = dir.path().display().to_string();

    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let got = stderr_of("dbLoadTemplate(\"rec.sub\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    // ONE pair for the file, as C prints it — three lines of `rec.db`,
    // one table, one expansion pass.
    let want = format!(
        "\
macLib: macro A is recursive (expanding macro B)
macLib: macro B is recursive (expanding macro A)
WARNING: 'rec.db' line 2 has undefined macros
C1.DESC Has unexpanded macro
ERROR: Can't set 'C1.DESC' to '$(B,recursive)'  : Bad Field value
ERROR:  at or before ')' in path \"{found_under}\"  file \"rec.db\" line 2

 2 |   field(DESC, \"$(B,recursive)\")

ERROR: Failed to load 'rec.db'
dbLoadRecords(\"rec.db\", A=\"$(B)\",B=\"$(A)\")
Substitution file error: Error while reading included file
line 2: '}}'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// Run `dbLoadRecords("<name>")` over `db` in a directory of its own that
/// is the include path, prepared by `setup`; return what it wrote, the
/// directory, and the records it left, sorted.
async fn load_in_dir(
    name: &str,
    db: &[u8],
    setup: impl FnOnce(&std::path::Path),
) -> (String, String, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), db).expect("write the .db");
    setup(dir.path());
    let found_under = dir.path().display().to_string();
    let pvdb = Arc::new(PvDatabase::new());

    // SAFETY: the process-global env is why the callers are `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let got = stderr_of(&format!("dbLoadRecords(\"{name}\")"), pvdb.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    let mut records: Vec<String> = pvdb
        .all_record_names()
        .iter()
        .map(|n| String::from_utf8_lossy(n.as_ref()).into_owned())
        .collect();
    records.sort();
    (got, found_under, records)
}

/// A character no `INITIAL` rule of `dbLex.l` takes is a `yyerrorAbort`
/// wherever the lexer meets it, not only after a field value: the rest of
/// its line is still lexed, and records the grammar completes on it are
/// created, but the next line is EOF and the load fails. Each case is one
/// boundary of the rule; every expected text is `softIoc` R7.0.10's (`dbl`
/// for the records), with its `"."` search path for the test's directory.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_initial_state_refusal_ends_the_load_after_its_line() {
    let cases: &[(&str, &[u8], &str, &[&str])] = &[
        // Alone on a line, at file scope: EOF follows, which the grammar
        // accepts there.
        (
            "stray.db",
            b"record(ai, \"A\") {}\n@\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character '@'
 at or before '@' in path \"{dir}\"  file \"stray.db\" line 2

 2 | @

ERROR: Failed to load 'stray.db'
",
            &["A"],
        ),
        // Before a token: the record on the line is still completed.
        (
            "c3.db",
            b"record(ai, \"A\") {}\nrecord(ai, @\"X\") {}\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character '@'
 at or before '@' in path \"{dir}\"  file \"c3.db\" line 2

 2 | record(ai, @\"X\") {}

ERROR: Failed to load 'c3.db'
",
            &["A", "X"],
        ),
        // A string the newline ends: its own rule, `yytext` through the
        // newline, and the grammar then meets EOF.
        (
            "c4.db",
            b"record(ai, \"A\") {}\nrecord(ai, \"X) {}\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Newline in string, closing quote missing
 at or before '\"X) {}
' in path \"{dir}\"  file \"c4.db\" line 2

 2 | record(ai, \"X) {}

ERROR: syntax error

 2 | record(ai, \"X) {}

ERROR: Failed to load 'c4.db'
",
            &["A"],
        ),
        // A string a backslash-newline ends matches neither string rule: the
        // quote alone is refused, and the lexer goes on after it.
        (
            "c5.db",
            b"record(ai, \"A\") {}\nrecord(ai, \"X\\\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character '\"'
 at or before '\"' in path \"{dir}\"  file \"c5.db\" line 2

 2 | record(ai, \"X\\

ERROR: Invalid character '\\'

 2 | record(ai, \"X\\

ERROR: syntax error

 2 | record(ai, \"X\\

ERROR: Failed to load 'c5.db'
",
            &["A"],
        ),
        // A single quote delimits a string only in the `JSON` state.
        (
            "c6.db",
            b"record(ai, \"A\") {}\nrecord(ai, 'X') {}\n",
            "\
ERROR: Invalid character '''
 at or before ''' in path \"{dir}\"  file \"c6.db\" line 2

 2 | record(ai, 'X') {}

ERROR: Invalid character '''

 2 | record(ai, 'X') {}

ERROR: Failed to load 'c6.db'
",
            &["A", "X"],
        ),
        // Inside a record body.
        (
            "c7.db",
            b"record(ai, \"A\") { @ field(DESC, \"x\") }\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character '@'
 at or before '@' in path \"{dir}\"  file \"c7.db\" line 1

 1 | record(ai, \"A\") { @ field(DESC, \"x\") }

ERROR: Failed to load 'c7.db'
",
            &["A"],
        ),
        // `%` starts a token, which the grammar refuses at file scope.
        (
            "c8.db",
            b"record(ai, \"A\") {}\n%foo\nrecord(ai, \"B\") {}\n",
            "\
ERROR: syntax error
 at or before '%foo' in path \"{dir}\"  file \"c8.db\" line 2

 2 | %foo

ERROR: Failed to load 'c8.db'
",
            &["A"],
        ),
        // So does a quoted string.
        (
            "c14.db",
            b"record(ai, \"A\") {}\n\"X\" record(ai, \"B\") {}\n",
            "\
ERROR: syntax error
 at or before '\"X\"' in path \"{dir}\"  file \"c14.db\" line 2

 2 | \"X\" record(ai, \"B\") {}

ERROR: Failed to load 'c14.db'
",
            &["A"],
        ),
        // A character that does not print is named by its byte.
        (
            "c9.db",
            b"record(ai, \"A\") {}\n\x01 record(ai, \"C\") {}\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character 0x01
 at or before '\x01' in path \"{dir}\"  file \"c9.db\" line 2

 2 | \x01 record(ai, \"C\") {}

ERROR: Failed to load 'c9.db'
",
            &["A", "C"],
        ),
        // Every refusal on the line is reported, the position only once.
        (
            "c10.db",
            b"record(ai, \"A\") {}\n@ @ record(ai, \"C\") {} $\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character '@'
 at or before '@' in path \"{dir}\"  file \"c10.db\" line 2

 2 | @ @ record(ai, \"C\") {} $

ERROR: Invalid character '@'

 2 | @ @ record(ai, \"C\") {} $

ERROR: Invalid character '$'

 2 | @ @ record(ai, \"C\") {} $

ERROR: Failed to load 'c10.db'
",
            &["A", "C"],
        ),
        // A body left open by the EOF is a syntax error, and not a dirty
        // parser stack.
        (
            "c11.db",
            b"record(ai, \"A\") {\n  field(DESC, \"x\") @\n}\nrecord(ai, \"B\") {}\n",
            "\
ERROR: Invalid character '@'
 at or before '@' in path \"{dir}\"  file \"c11.db\" line 2

 2 |   field(DESC, \"x\") @

ERROR: syntax error

 2 |   field(DESC, \"x\") @

ERROR: Failed to load 'c11.db'
",
            &["A"],
        ),
    ];
    for (name, db, want, records) in cases {
        let (got, dir, loaded) = load_in_dir(name, db, |_| {}).await;
        let want = want.replace("{dir}", &dir);
        assert_eq!(
            got, want,
            "{name}\n--- got ---\n{got}\n--- want ---\n{want}"
        );
        assert_eq!(loaded, *records, "{name}: records left");
    }
}

/// `include sub/S.template` names `sub`: `/` is no bareword character, and
/// is refused after the include is reduced, in the file it pushed. Measured
/// on `softIoc` R7.0.10 with `sub` absent, then a directory, which `fopen`
/// opens and the first read finds empty.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_slash_after_an_include_name_is_refused() {
    let db = b"record(ai, \"A\") {}\ninclude sub/S.template\nrecord(ai, \"B\") {}\n";
    let (got, dir, loaded) = load_in_dir("n2.db", db, |_| {}).await;
    let want = format!(
        "\
ERROR: Can't open include file 'sub'
ERROR:  at or before 'sub' in path \"{dir}\"  file \"n2.db\" line 2

 2 | include sub/S.template

ERROR: Invalid character '/'

 2 | include sub/S.template

ERROR: syntax error

 2 | include sub/S.template

ERROR: Failed to load 'n2.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert_eq!(loaded, ["A"]);

    let db = b"include sub/S.template\nrecord(ai, \"B\") {}\n";
    let (got, dir, loaded) = load_in_dir("c2.db", db, |dir| {
        std::fs::create_dir(dir.join("sub")).expect("mkdir sub");
        std::fs::write(dir.join("sub/S.template"), "record(ai, \"S\") {}\n").expect("write S");
    })
    .await;
    let want = format!(
        "\
ERROR: Invalid character '/'
 at or before '/' in path \"{dir}\"  file \"sub\" line 0
 in path \"{dir}\"  file \"c2.db\" line 1

 0 | include sub/S.template

ERROR: syntax error

 0 | include sub/S.template

ERROR: Failed to load 'c2.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(loaded.is_empty(), "records left: {loaded:?}");
}

/// A keyword is the whole bareword run, as flex's longest match makes it:
/// `record.x` is refused as one token. Measured on `softIoc` R7.0.10.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_keyword_is_the_whole_bareword_run() {
    let db = b"record(ai, \"A\") {}\nrecord.x(ai, \"B\") {}\n";
    let (got, dir, loaded) = load_in_dir("kw.db", db, |_| {}).await;
    let want = format!(
        "\
ERROR: syntax error
 at or before 'record.x' in path \"{dir}\"  file \"kw.db\" line 2

 2 | record.x(ai, \"B\") {{}}

ERROR: Failed to load 'kw.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert_eq!(loaded, ["A"]);
}
