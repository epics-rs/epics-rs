//! **A `.db` diagnostic MUST say where in the operator's file it happened.**
//!
//! C answers "where" three times for every bad line, and this port answered it
//! none of the three. Measured on base's own `compressTest.db`
//! (`modules/database/test/std/rec/compressTest.db`), loaded by `softIoc` and
//! by `softioc-rs` from the same directory: C wrote 36 stderr lines, the port
//! wrote 9, and all 21 missing ones were position:
//!
//! 1. macLib's own notice, `macLib: macro INP is undefined (expanding string
//!    …)` — `errlogPrintf` in `macCore.c:913-917`, raised inside the expander
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
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&owned)).join();

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
/// One deviation, and it is the flattening this port does before it parses:
/// C's lexer reads a line and the parser consumes it, so C interleaves each
/// line's read-time diagnostics with that line's parse-time ones, while
/// `expand_includes` here expands the whole include tree first — so all four
/// macro notices come out before the first refusal. Every line C writes is
/// written, with the same bytes; only the grouping differs. Making them
/// interleave would mean buffering the expander's output and flushing it as
/// the install loop walks records, which is ordering by convention — the
/// install loop is under no obligation to visit lines in file order — and this
/// port would rather have the honest grouping than a fragile imitation.
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
macLib: macro ALG is undefined (expanding string   field(ALG, \"$(ALG)\")
)
WARNING: 'compressTest.db' line 8 has undefined macros
macLib: macro BALG is undefined (expanding string   field(BALG,\"$(BALG)\")
)
WARNING: 'compressTest.db' line 10 has undefined macros
macLib: macro NSAM is undefined (expanding string   field(NSAM,\"$(NSAM)\")
)
WARNING: 'compressTest.db' line 11 has undefined macros
comp.INP Has unexpanded macro
ERROR: Can't set 'comp.INP' to '$(INP,undefined) NPP'  : Bad Field value
ERROR:  at or before ')' in path \"{found_under}\"  file \"compressTest.db\" line 7

 7 |   field(INP, \"$(INP,undefined) NPP\")

comp.ALG Has unexpanded macro
ERROR: Can't set 'comp.ALG' to '$(ALG,undefined)'  : Bad Field value
    Did you mean \"Average\"?
ERROR: 
 8 |   field(ALG, \"$(ALG,undefined)\")

comp.BALG Has unexpanded macro
ERROR: Can't set 'comp.BALG' to '$(BALG,undefined)'  : Bad Field value
    Did you mean \"FIFO Buffer\"?
ERROR: 
 10 |   field(BALG,\"$(BALG,undefined)\")

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
WARNING: 'compressTest.db' line 8 has undefined macros
WARNING: 'compressTest.db' line 10 has undefined macros
WARNING: 'compressTest.db' line 11 has undefined macros
comp.INP Has unexpanded macro
ERROR: Can't set 'comp.INP' to '$(INP) NPP'  : Bad Field value
ERROR:  at or before ')' in path \"{found_under}\"  file \"compressTest.db\" line 7

 7 |   field(INP, \"$(INP) NPP\")

comp.ALG Has unexpanded macro
ERROR: Can't set 'comp.ALG' to '$(ALG)'  : Bad Field value
    Did you mean \"N to 1 Average\"?
ERROR: 
 8 |   field(ALG, \"$(ALG)\")

comp.BALG Has unexpanded macro
ERROR: Can't set 'comp.BALG' to '$(BALG)'  : Bad Field value
    Did you mean \"FIFO Buffer\"?
ERROR: 
 10 |   field(BALG,\"$(BALG)\")

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
/// through the arm that carries a message (`dbYacc.y:373-374`) — `yyparse`
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
/// Three deviations, all deliberate, none of them position:
///
/// * the message is this parser's own sentence rather than yacc's
///   `syntax error`. It names the token C leaves to the ` at or before`
///   clause, and `ERROR: <sentence>` is the shape C's own lexer diagnostics
///   already have.
/// * the clause reads ` at or before column 6` where C reads ` at or before
///   'qqq'`. C's lexer always holds a `yytext`; this recursive-descent parser
///   holds a column instead, and naming the column it has is true where
///   quoting a token it never recorded would be a guess.
/// * `WARNING: dbReadCOM: Parser stack dirty w/o error. 1` is absent. It
///   counts the half-built objects left on C's `tempList`
///   (`dbLexRoutines.c:303-305`); this parser has no such list.
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
ERROR: expected 'field', got 'qqq'
 at or before column 6 in path \"{found_under}\"  file \"syn.db\" line 3

 3 |   qqq

ERROR: Failed to load 'syn.db'
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
/// Two deviations, both from the engine and not the wording. C detects a
/// cycle once per macro TABLE entry, in the `expand()` pass it runs while the
/// table is dirty (`macCore.c:646-679`), so it names both ends and this
/// lazily-expanding one names the end it reached first. And because C's
/// pre-expansion resolves `A` before the `.db` line asks for it, C's
/// placeholder is the other member of the pair.
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

    let want = format!(
        "\
macLib: macro B is recursive (expanding macro A)
WARNING: 'rec.db' line 2 has undefined macros
C1.DESC Has unexpanded macro
ERROR: Can't set 'C1.DESC' to '$(A,recursive)'  : Bad Field value
ERROR:  at or before ')' in path \"{found_under}\"  file \"rec.db\" line 2

 2 |   field(DESC, \"$(A,recursive)\")

ERROR: Failed to load 'rec.db'
"
    );
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}
