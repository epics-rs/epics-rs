//! **A `dbLoadTemplate` failure MUST say which row failed, in the words C
//! uses for that row.**
//!
//! C `dbLoadTemplate` (`modules/database/src/ioc/dbtemplate/dbLoadTemplate.y`)
//! is not a loader. It opens the `.substitutions` file, parses it, and runs
//! one `dbLoadRecords` per substitution row through `msiLoadRecords` (`:49-57`),
//! so every diagnostic an operator sees comes from one of exactly three
//! places and each of them says something different:
//!
//! 1. the open — `dbLoadTemplate: error opening sub file <f>: <strerror>`
//!    (`:371-374`), before `pdbbase` is touched, returning -1;
//! 2. the grammar — `yyerror` (`:330-338`) writing the message and the
//!    lexer's position, `dbLoadTemplate` returning `yyparse`'s status;
//! 3. a ROW — the full `dbLoadRecords` diagnostic set for that row's `.db`
//!    (`dbAccess.c:808-812`), then `msiLoadRecords`' echo of the failing
//!    call and `yyerror("Error while reading included file")`, then
//!    `YYABORT`, which abandons every row after it.
//!
//! The port collapsed all three onto one sentence — `parse error: DB parse
//! error at line 0, column 0: template file not found: 'missing.db'` —
//! returned as an `Err(String)` for whichever caller happened to catch it to
//! render. That named no row, no `.substitutions` line, and never wrote
//! `Can't open file` or `Failed to load` at all; and the one place the port
//! did write `Failed to load` for a template, the after-`iocInit` refusal, it
//! named the `.substitutions` file where C names the row's `.db`.
//!
//! Every `want` below is `softIoc` R7.0.10-146 stderr, captured on the same
//! input, ANSI stripped. Deviations are named per test; the only one left is
//! wording — where C says `syntax error` this parser names the token it
//! wanted instead.
//!
//! Unix only, for the reason the sibling gate
//! (`db_load_diagnostics_say_where.rs`) is: what is captured is the process
//! console, and capturing it means pointing fd 2 elsewhere and putting it
//! back.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

/// Run one iocsh line with fd 2 pointed at a file, and give back what it
/// wrote. Same capture as `db_load_diagnostics_say_where.rs`: the two
/// families under test reach the operator by different routes and the
/// process console is the only place they are one stream again.
fn stderr_of(line: &str, db: Arc<PvDatabase>) -> String {
    ran_capturing(line, db).0
}

/// The same capture, plus the status the shell gave the line — C's
/// `dbLoadTemplate` hands back `yyparse`'s value and a recovered lexer
/// fault does not change it, so the two have to be read together.
fn ran_capturing(line: &str, db: Arc<PvDatabase>) -> (String, Result<(), String>) {
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
    let ran =
        std::thread::spawn(move || IocShell::new(db, bridge).execute_line_reported(&owned)).join();

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    let status = ran.expect("the shell thread");

    (
        strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture")),
        status,
    )
}

/// C's SGR sequences, dropped — the port's copy of what `errlog` does when
/// the console is not a terminal (`errlog.c:672-681`), so the gate holds
/// whichever way the harness's console answers.
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

/// The open, which is C's alone: `dbLoadTemplate` opens the
/// `.substitutions` file itself and reports the failure with its own
/// wording and `strerror(errno)` (`dbLoadTemplate.y:362-374`), returning -1
/// before a parser or `pdbbase` is involved.
///
/// Measured, script `dbLoadTemplate("nosuch.sub")`:
///
/// ```text
/// dbLoadTemplate: error opening sub file nosuch.sub: No such file or directory
/// ```
///
/// Byte for byte, including the name as the operator wrote it rather than
/// the path the search resolved. The port wrote `parse error: DB parse error
/// at line 0, column 0: cannot read substitutions file
/// '<abs>/nosuch.sub': No such file or directory (os error 2)`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_missing_substitutions_file_gets_cs_open_diagnostic() {
    let dir = tempfile::tempdir().expect("tempdir");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of(
        "dbLoadTemplate(\"nosuch.sub\")",
        Arc::new(PvDatabase::new()),
    );
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    assert_eq!(
        got, "dbLoadTemplate: error opening sub file nosuch.sub: No such file or directory\n",
        "\n--- got ---\n{got}"
    );
}

/// The grammar, which is C's second place: the file opened and `yyparse`
/// refused it, so `yyerror` writes the message and the lexer's position
/// (`dbLoadTemplate.y:330-338`).
///
/// Measured on this exact file:
///
/// ```text
/// Substitution file error: syntax error
/// line 2: '='
/// ```
///
/// Both lines are C's, byte for byte. The sentence is bison's one constant:
/// C sets no `parse.error verbose`, so every shape the grammar can refuse
/// reaches `yyerror` with `syntax error` and nothing else. The failing
/// lookahead supplies both the line and the `yytext`, exactly as bison's
/// does.
///
/// Also asserted: the file is distinguishable from a row failure — C returns
/// `yyparse`'s status here and nothing was loaded.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_broken_substitutions_file_gets_cs_grammar_diagnostic() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("good.db"), "record(ai, \"A1\") {}\n").expect("write the .db");
    std::fs::write(
        dir.path().join("bad.sub"),
        "file good.db {\n{ N=1 = 2 }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let got = stderr_of("dbLoadTemplate(\"bad.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
Substitution file error: syntax error
line 2: '='
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("A1").is_none(),
        "C never reaches a row when the grammar refuses the file"
    );
}

/// A ROW, which is the case the collapsed message hid entirely: the
/// `.substitutions` file is fine and one row names a `.db` that is not
/// there.
///
/// Measured, `mix.sub` = a good row then a missing one:
///
/// ```text
/// ERROR: Can't open file 'missing.db'
/// ERROR: Failed to load 'missing.db'
/// dbLoadRecords("missing.db", N=2)
/// Substitution file error: Error while reading included file
/// line 5: '}'
/// ```
///
/// Five lines from four different C functions, and the port wrote none of
/// them. The first two are `dbReadCOM` and `dbLoadRecords`
/// (`dbLexRoutines.c:284-286`, `dbAccess.c:808`) — the row IS a
/// `dbLoadRecords`, so it carries that command's whole diagnostic set. The
/// third is `msiLoadRecords` echoing the call it made
/// (`dbLoadTemplate.y:53`), which is the only line that says WHICH row. The
/// fourth is `yyerror` before `YYABORT`.
///
/// The fifth is the position `yyerror` always prints: the lexer parked on the
/// brace that closed the failing row.
///
/// The record from the row BEFORE the failure stays, which is C's `YYABORT`
/// semantics — each row is committed by its own `dbLoadRecords` before the
/// next is read — and is asserted here because it is the half a batching
/// port silently loses.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_row_whose_db_is_missing_reports_it_as_the_dbloadrecords_it_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("good.db"), "record(ai, \"A1\") {}\n").expect("write the .db");
    std::fs::write(
        dir.path().join("mix.sub"),
        "file good.db {\n{ N=1 }\n}\nfile missing.db {\n{ N=2 }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let got = stderr_of("dbLoadTemplate(\"mix.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
ERROR: Can't open file 'missing.db'
ERROR: Failed to load 'missing.db'
dbLoadRecords(\"missing.db\", N=2)
Substitution file error: Error while reading included file
line 5: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("A1").is_some(),
        "the row before the failing one is committed by its own dbLoadRecords"
    );
}

/// The after-`iocInit` refusal, which the port answered with the wrong file
/// name.
///
/// C has no load-phase gate in `dbLoadTemplate` at all: it opens and parses
/// the `.substitutions` file, and the FIRST row's `dbLoadRecords` is what
/// meets `dbReadCOM`'s `getIocState() != iocVoid` (`dbLexRoutines.c:236-238`).
/// So the name in the refusal is the row's `.db`, and the refusal is followed
/// by the same `msiLoadRecords` / `yyerror` pair every row failure gets.
/// Measured, `iocInit` then `dbLoadTemplate("ok.sub")`:
///
/// ```text
/// ERROR: Failed to load 'good.db'
///     Records cannot be loaded after iocInit!
/// dbLoadRecords("good.db", N=1)
/// Substitution file error: Error while reading included file
/// line 2: '}'
/// ```
///
/// The port wrote `ERROR: Failed to load 'ok.sub'` and the indented line, and
/// stopped — naming the file the operator did not fail to load, and leaving
/// out the row identity.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_template_row_after_iocinit_names_the_rows_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("good.db"), "record(ai, \"A1\") {}\n").expect("write the .db");
    std::fs::write(dir.path().join("ok.sub"), "file good.db {\n{ N=1 }\n}\n")
        .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    db.ioc_init().await;
    let got = stderr_of("dbLoadTemplate(\"ok.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
ERROR: Failed to load 'good.db'
    Records cannot be loaded after iocInit!
dbLoadRecords(\"good.db\", N=1)
Substitution file error: Error while reading included file
line 2: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// C's lexer does not abandon the file over a character it does not know.
/// The catch-all rule (`dbLoadTemplate_lex.l:47-56`) reports through
/// `yyerror` and returns no token, so flex resumes at the very next
/// character and `yyparse` never learns anything happened.
///
/// Measured on this exact file, `dbLoadTemplate("pct.sub")` then `dbl`:
///
/// ```text
/// Substitution file error: invalid character '%'
/// line 2: '%'
/// ```
///
/// with `A1` in `dbl` and a zero status. The port aborted the lex, so the
/// row was never loaded and the command failed: one stray byte built a
/// smaller database than C's, which is the only one of this file's
/// deviations that costs records rather than words.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_stray_character_is_reported_and_the_row_still_loads() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("rec.db"), "record(ai, \"$(N)\") {}\n").expect("write the .db");
    std::fs::write(dir.path().join("pct.sub"), "file rec.db {\n{ N=A1 } %\n}\n")
        .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (got, status) = ran_capturing("dbLoadTemplate(\"pct.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
Substitution file error: invalid character '%'
line 2: '%'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("A1").is_some(),
        "C loads the row and so must this: a recovered character costs one character"
    );
    assert!(
        status.is_ok(),
        "C returns yyparse's status, which a recovered lexer fault does not touch: {status:?}"
    );
}

/// The same recovery inside a row, where the character C drops leaves a
/// token sequence the grammar cannot take. Both reports come out, in the
/// order the lexer reached them, and the row is lost — to the syntax error,
/// not to the stray character.
///
/// Measured on this exact file:
///
/// ```text
/// Substitution file error: invalid character '%'
/// line 2: '%'
/// Substitution file error: syntax error
/// line 2: '}'
/// ```
///
/// Both messages and both position lines are C's, byte for byte.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_stray_character_inside_a_row_reports_twice_in_cs_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("rec.db"), "record(ai, \"$(N)\") {}\n").expect("write the .db");
    std::fs::write(dir.path().join("mid.sub"), "file rec.db {\n{ N=A%1 }\n}\n")
        .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (got, status) = ran_capturing("dbLoadTemplate(\"mid.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
Substitution file error: invalid character '%'
line 2: '%'
Substitution file error: syntax error
line 2: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("A").is_none() && db.get_record("A1").is_none(),
        "the grammar never reduced this row, so C loads nothing from it"
    );
    assert!(
        status.is_err(),
        "a grammar error IS yyparse's non-zero status"
    );
}

/// And the rows a stopped parse got through stay loaded. C runs each row's
/// `dbLoadRecords` from the grammar action as the row is reduced
/// (`dbLoadTemplate.y:193`), so by the time `yyparse` fails on a later line
/// the earlier rows are already in `pdbbase` — it returns non-zero over a
/// database that is not empty.
///
/// Measured on this exact file, `A1` is in `dbl` and stderr is
///
/// ```text
/// Substitution file error: syntax error
/// line 5: '='
/// ```
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_row_before_a_syntax_error_stays_loaded() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("rec.db"), "record(ai, \"$(N)\") {}\n").expect("write the .db");
    std::fs::write(
        dir.path().join("two.sub"),
        "file rec.db {\n{ N=A1 }\n}\nfile rec.db {\n{ N=B1 = 2 }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (got, status) = ran_capturing("dbLoadTemplate(\"two.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
Substitution file error: syntax error
line 5: '='
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("A1").is_some(),
        "the first row's dbLoadRecords already ran; a later syntax error cannot undo it"
    );
    assert!(
        db.get_record("B1").is_none(),
        "the failing row loads nothing"
    );
    assert!(
        status.is_err(),
        "a grammar error IS yyparse's non-zero status"
    );
}

/// The echo says which values arrived in quotes, because C's grammar has
/// one rule per token kind — `WORD EQUALS WORD` writes `name=value` and
/// `WORD EQUALS QUOTE` writes `name="value"` (`dbLoadTemplate.y:301-323`)
/// — and always writes the quotes back as `"`. Nothing about the text
/// decides it: `N="1"` and `M=2` substitute the same kind of value and
/// echo differently.
///
/// Measured on this exact file:
///
/// ```text
/// dbLoadRecords("nope.db", N="1",M=2,P="a b")
/// ```
///
/// The port's lexer had collapsed `WORD` and `QUOTE`, so the echo had to
/// guess from the text: it quoted whatever a `WORD` could not have held and
/// wrote `N=1` for C's `N="1"`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_quoted_value_echoes_with_the_quotes_c_gives_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("q.sub"),
        "file nope.db {\n{ N=\"1\", M=2, P=\"a b\" }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of("dbLoadTemplate(\"q.sub\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
ERROR: Can't open file 'nope.db'
ERROR: Failed to load 'nope.db'
dbLoadRecords(\"nope.db\", N=\"1\",M=2,P=\"a b\")
Substitution file error: Error while reading included file
line 2: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The other half of the same rule: `pattern_value` is `QUOTE` or `WORD`
/// (`dbLoadTemplate.y:220-255`), quoting the value against the name the
/// `pattern` line gave it. Measured on this exact file:
///
/// ```text
/// dbLoadRecords("nope.db", N="1",M=2)
/// ```
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_quoted_pattern_value_echoes_with_the_quotes_c_gives_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("qp.sub"),
        "file nope.db {\npattern { N, M }\n{ \"1\", 2 }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of("dbLoadTemplate(\"qp.sub\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
ERROR: Can't open file 'nope.db'
ERROR: Failed to load 'nope.db'
dbLoadRecords(\"nope.db\", N=\"1\",M=2)
Substitution file error: Error while reading included file
line 3: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// And the line in that frame is the row's CLOSING brace, not its first.
/// C's row rules all end in `C_BRACE` and bison reduces them without
/// reading a lookahead, so the lexer is still parked on the brace that
/// closed the row when `msiLoadRecords` reports — which for a row written
/// across two lines is the second of them.
///
/// Measured on this exact file:
///
/// ```text
/// dbLoadRecords("nope.db", N=P2,M=q)
/// Substitution file error: Error while reading included file
/// line 3: '}'
/// ```
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_multiline_rows_frame_names_its_closing_brace() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("multi.sub"),
        "file nope.db {\n{ N=P2,\n  M=q }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of("dbLoadTemplate(\"multi.sub\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
ERROR: Can't open file 'nope.db'
ERROR: Failed to load 'nope.db'
dbLoadRecords(\"nope.db\", N=P2,M=q)
Substitution file error: Error while reading included file
line 3: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// A value past the last `pattern` name is a stderr line in C, not a log
/// entry: `pattern_value` writes it with `fprintf(stderr, ...)` when the
/// value is reduced (`dbLoadTemplate.y:233`, `:250`), so it lands on the
/// same console as every other diagnostic here and it names the value's
/// own line — measured, a third value on line 3 under a row whose brace
/// closes on line 4 reports line 3.
///
/// Measured on this exact file:
///
/// ```text
/// dbLoadTemplate: Too many values given, line 3.
/// ERROR: Can't open file 'nope.db'
/// ERROR: Failed to load 'nope.db'
/// dbLoadRecords("nope.db", A="1",B="2")
/// Substitution file error: Error while reading included file
/// line 4: '}'
/// ```
///
/// The port had this one on `tracing::warn!`, which the operator watching
/// stderr never sees.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_surplus_pattern_value_is_reported_on_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("many.sub"),
        "file nope.db {\npattern { A, B }\n{ \"1\", \"2\", \"3\"\n}\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of("dbLoadTemplate(\"many.sub\")", Arc::new(PvDatabase::new()));
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
dbLoadTemplate: Too many values given, line 3.
ERROR: Can't open file 'nope.db'
ERROR: Failed to load 'nope.db'
dbLoadRecords(\"nope.db\", A=\"1\",B=\"2\")
Substitution file error: Error while reading included file
line 4: '}'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The deprecated `WORD { … }` row: C accepts it and says so
/// (`dbLoadTemplate.y:199-203`, `:281-285`), and the line it names is
/// `line_num` at the row's action — the closing brace, not the word's own
/// line, though the sentence is about the word.
///
/// Measured on this exact file, where `rowname` is on line 3 and the row
/// closes on line 4:
///
/// ```text
/// dbLoadTemplate: Substitution file uses deprecated syntax.
///     the string 'rowname' on line 4 that comes just before the
///     '{' character is extraneous and should be removed.
/// ```
///
/// The port took the word and said nothing, which is the one option C does
/// not offer: it accepts the file and warns.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_deprecated_row_is_accepted_and_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("good.db"), "record(ai, \"$(N)\") {}\n").expect("write the .db");
    std::fs::write(
        dir.path().join("dep.sub"),
        "file good.db {\npattern { N }\nrowname\n{ A1 }\n}\n",
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (got, status) = ran_capturing("dbLoadTemplate(\"dep.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
dbLoadTemplate: Substitution file uses deprecated syntax.
    the string 'rowname' on line 4 that comes just before the
    '{' character is extraneous and should be removed.
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("A1").is_some(),
        "C accepts the deprecated row and loads it"
    );
    assert!(status.is_ok(), "warning only: yyparse still returns 0");
}

/// C's grammar takes a `WORD` and only a `WORD` where a name goes:
/// `pattern_name: WORD` (`dbLoadTemplate.y:154`) and
/// `variable_definition: WORD EQUALS …` (`:301`, `:312`). Quoting one is a
/// syntax error there, so accepting it here would only move the failure to
/// the IOC that finally reads the file.
///
/// Measured on both files, C says:
///
/// ```text
/// Substitution file error: syntax error
/// line 2: '"N"'
/// ```
///
/// Both lines are C's; the position line keeps `yytext`'s own quotes.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_quoted_name_is_refused_the_way_c_refuses_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("qname.sub"),
        "file nope.db {\npattern { \"N\" }\n{ 1 }\n}\n",
    )
    .expect("write the pattern-name file");
    std::fs::write(
        dir.path().join("qmacro.sub"),
        "file nope.db {\n{ \"N\"=1 }\n}\n",
    )
    .expect("write the macro-name file");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (name_got, name_status) = ran_capturing("dbLoadTemplate(\"qname.sub\")", db.clone());
    let (macro_got, macro_status) = ran_capturing("dbLoadTemplate(\"qmacro.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
Substitution file error: syntax error
line 2: '\"N\"'
";
    assert_eq!(
        name_got, want,
        "\n--- got ---\n{name_got}\n--- want ---\n{want}"
    );
    assert_eq!(
        macro_got, want,
        "\n--- got ---\n{macro_got}\n--- want ---\n{want}"
    );
    assert!(
        name_status.is_err() && macro_status.is_err(),
        "yyparse fails"
    );
}

/// End of input, where C's two lines part company: the FIRST is still
/// bison's constant and the SECOND is a read of a freed lex buffer.
///
/// Measured over eleven files that end mid-grammar, three runs each, on
/// `bin/linux-x86_64/softIoc` (R7.0.10-146-g8f5015b663d764ad75df). The
/// sentence was `Substitution file error: syntax error` every time and the
/// line number never moved. The `yytext` under it was empty in six (this
/// file among them), the last matched token in one, and uninitialised heap
/// in four — where three consecutive runs of the SAME file printed three
/// different byte strings, because flex has deleted the buffer `yytext`
/// points into by the time `yyerror` runs.
///
/// So the first line is asserted as C's byte for byte, and the empty
/// `yytext` is this port's deliberate stand-in for bytes that are not
/// reproducible and must not be reproduced. For this file it is also C's
/// own answer, so the whole two-line stream matches. The line number is
/// C's: 3, because both newlines ran `line_num++`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn end_of_input_matches_cs_sentence_and_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("rec.db"), "record(ai, \"$(N)\") {}\n").expect("write the .db");
    std::fs::write(dir.path().join("eof.sub"), "file rec.db {\n{ N=A1 \n")
        .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (got, status) = ran_capturing("dbLoadTemplate(\"eof.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let mut lines = got.lines();
    assert_eq!(
        lines.next(),
        Some("Substitution file error: syntax error"),
        "the line a script greps is C's\n--- got ---\n{got}"
    );
    assert_eq!(
        lines.next(),
        Some("line 3: ''"),
        "C's line number, and the stand-in for its unreproducible yytext\n--- got ---\n{got}"
    );
    assert_eq!(lines.next(), None, "\n--- got ---\n{got}");
    assert!(status.is_err(), "yyparse fails");
}

/// C's `pattern` line fills a fixed `vars` array sized `dbTemplateMaxVars`
/// (`dbLoadTemplate.y:43`, `:376-377`), and `pattern_name` reports a name past
/// it through `yyerror(NULL)` — the message-less form, so the first line is
/// `Substitution file error.` with a period — then DROPS the name and
/// carries on (`:154-171`). Each further name reports again, and the row
/// binds only the names that fit.
///
/// Measured with 102 pattern names and 100 values, `dbLoadTemplate` then
/// `dbl`:
///
/// ```text
/// More than dbTemplateMaxVars = 100 macro variables used
/// Substitution file error.
/// line 2: 'N100'
/// More than dbTemplateMaxVars = 100 macro variables used
/// Substitution file error.
/// line 2: 'N101'
/// ```
///
/// with the record loaded and a zero status. C exports the ceiling as an
/// iocsh variable and so does this port, so this case is the default 100
/// and the next test raises it. The `sub_collect` buffer C sizes from the
/// same number is not modelled: every `strcat` into it is unguarded
/// (`:220-255`, `:301-323`), so overrunning it is an overflow with no
/// diagnostic to match, and the substitution string here is a `String`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_pattern_name_past_dbtemplatemaxvars_is_reported_and_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("n0.db"), "record(ai, \"$(N0)\") {}\n").expect("write the .db");
    let names = (0..102)
        .map(|i| format!("N{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = (0..100)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.path().join("cap.sub"),
        format!("file n0.db {{\npattern {{ {names} }}\n{{ {values} }}\n}}\n"),
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    let (got, status) = ran_capturing("dbLoadTemplate(\"cap.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
More than dbTemplateMaxVars = 100 macro variables used
Substitution file error.
line 2: 'N100'
More than dbTemplateMaxVars = 100 macro variables used
Substitution file error.
line 2: 'N101'
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    assert!(
        db.get_record("0").is_some(),
        "the names that fit still bind and the row still loads"
    );
    assert!(status.is_ok(), "yyerror(NULL) does not abort: C returns 0");
}

/// The ceiling is a knob, not a constant: `dbCore.dbd:29` declares
/// `variable(dbTemplateMaxVars,int)` over the `int` at
/// `dbLoadTemplate.y:45`, and `pattern_name` reads that global at the
/// point of use, so raising it in a startup script raises what the NEXT
/// `dbLoadTemplate` accepts.
///
/// Measured: with `var dbTemplateMaxVars 200`, the 102-name file that
/// reports twice at the default loads silently.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn var_db_template_max_vars_raises_the_ceiling() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("n0.db"), "record(ai, \"$(N0)\") {}\n").expect("write the .db");
    let names = (0..102)
        .map(|i| format!("N{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = (0..100)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.path().join("cap.sub"),
        format!("file n0.db {{\npattern {{ {names} }}\n{{ {values} }}\n}}\n"),
    )
    .expect("write the .substitutions");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let db = Arc::new(PvDatabase::new());
    ran_capturing("var dbTemplateMaxVars 200", db.clone())
        .1
        .expect("var must succeed");
    let (got, status) = ran_capturing("dbLoadTemplate(\"cap.sub\")", db.clone());
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    assert_eq!(got, "", "\n--- got ---\n{got}");
    assert!(db.get_record("0").is_some(), "every name fits now");
    assert!(status.is_ok());
}

/// And a ceiling below 1 is refused before anything is opened: C sizes
/// `vars` and `sub_collect` with `malloc` from that number, so it checks
/// first and returns -1 (`dbLoadTemplate.y:355-360`). `var` writes the
/// global unvalidated — C's `varHandler` stores through a raw pointer —
/// so this is the only place the value is judged.
///
/// Measured, `var dbTemplateMaxVars 0` then `dbLoadTemplate("cap.sub")`:
///
/// ```text
/// ERROR: dbTemplateMaxVars = 0, must be +ve
/// ```
///
/// and nothing else — no `error opening sub file`, because the file is
/// never reached.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_non_positive_db_template_max_vars_is_refused_before_the_open() {
    let db = Arc::new(PvDatabase::new());
    ran_capturing("var dbTemplateMaxVars 0", db.clone())
        .1
        .expect("var must succeed");
    let (got, status) = ran_capturing("dbLoadTemplate(\"nosuch.sub\")", db.clone());
    ran_capturing("var dbTemplateMaxVars 100", db.clone())
        .1
        .expect("var must succeed");

    assert_eq!(
        got, "ERROR: dbTemplateMaxVars = 0, must be +ve\n",
        "\n--- got ---\n{got}"
    );
    assert!(status.is_err(), "C returns -1 here");
}
