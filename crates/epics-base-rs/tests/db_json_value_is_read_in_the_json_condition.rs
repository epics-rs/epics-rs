//! A field value is a `json_value` read in the `.db` lexer's `JSON` start
//! condition (`dbYacc.y:256-362`, `dbLex.l:105-153`): its grammar errors are
//! syntax errors at the token, and its lexer errors are `yyerrorAbort`s,
//! after which the lexer reads to the end of that line and then sees end of
//! file (`dbLexRoutines.c:374`). Every stderr below is softIoc R7.0.10.1
//! (8f5015b66) loading the same file, and the records are its `dbl`.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

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

/// Load `body` as `name` and give back the database and the load's stderr.
fn load(name: &str, body: &str) -> (Arc<PvDatabase>, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), body).expect("write the .db");
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why these tests are `serial`.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let db = Arc::new(PvDatabase::new());
    let shell_db = Arc::clone(&db);
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let line = format!("dbLoadRecords(\"{name}\")");
    let ran = std::thread::spawn(move || {
        // A script line runs inside `iocshBody`, which may block (`iocsh.cpp:1122`).
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        IocShell::new(shell_db, bridge).execute_line(&line)
    })
    .join();
    // SAFETY: restoring what this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    ran.expect("the shell thread").ok();
    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    (db, err, dir.path().display().to_string())
}

fn check(name: &str, body: &str, want: &str, have: &[&str], lack: &[&str]) {
    let (db, err, path) = load(name, body);
    assert_eq!(err, want.replace("{path}", &path));
    for rec in have {
        assert!(db.get_record(rec).is_some(), "{rec} is loaded");
    }
    for rec in lack {
        assert!(
            db.get_record(rec).is_none(),
            "{rec} is past the end of the text"
        );
    }
}

fn field(db: &PvDatabase, rec: &str, field: &str) -> String {
    let rec = db.get_record(rec).expect("the record loads");
    let value = rec.read().resolve_field(field).expect("a declared field");
    value.text_g()
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_invalid_character_aborts_and_the_line_ends_in_a_syntax_error() {
    check(
        "fb.db",
        r#"record(ai, "JC") { field(DESC, "c") }
record(ai, "JD") { field(INP, {calc:{expr:"A"}) field(DESC, "d") }
record(ai, "JE") {}
"#,
        r#"ERROR: Invalid character ')'
 at or before ')' in path "{path}"  file "fb.db" line 2

 2 | record(ai, "JD") { field(INP, {calc:{expr:"A"}) field(DESC, "d") }

ERROR: syntax error

 2 | record(ai, "JD") { field(INP, {calc:{expr:"A"}) field(DESC, "d") }

ERROR: Failed to load 'fb.db'
"#,
        &["JC", "JD"],
        &["JE"],
    );
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_json_grammar_error_is_a_syntax_error_at_its_token() {
    check(
        "f1.db",
        r#"record(ai, "M1") {}
record(ai, "M2") { field(INP, {a b}) }
record(ai, "M3") {}
"#,
        r#"ERROR: syntax error
 at or before 'b' in path "{path}"  file "f1.db" line 2

 2 | record(ai, "M2") { field(INP, {a b}) }

WARNING: dbReadCOM: Parser stack dirty w/o error. 1
ERROR: Failed to load 'f1.db'
"#,
        &["M1", "M2"],
        &["M3"],
    );
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_abort_reads_on_to_the_end_of_its_line() {
    check(
        "f3.db",
        r#"record(ai, "M1") { field(INP, {a:1)}) }
record(ai, "M3") {}
"#,
        r#"ERROR: Invalid character ')'
 at or before ')' in path "{path}"  file "f3.db" line 1

 1 | record(ai, "M1") { field(INP, {a:1)}) }

dbJLinkInit: Link type 'a' not found
ERROR: Can't set 'M1.INP' to '{a:1}'  : Bad Field value
ERROR: 
 1 | record(ai, "M1") { field(INP, {a:1)}) }

ERROR: Failed to load 'f3.db'
"#,
        &["M1"],
        &["M3"],
    );
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_bad_json_string_aborts() {
    check(
        "f4.db",
        r#"record(ai, "M1") { field(INP, {a:"\1"}) }
"#,
        r#"ERROR: Bad character in JSON string
 at or before '"\1"' in path "{path}"  file "f4.db" line 1

 1 | record(ai, "M1") { field(INP, {a:"\1"}) }

ERROR: syntax error

 1 | record(ai, "M1") { field(INP, {a:"\1"}) }

ERROR: Failed to load 'f4.db'
"#,
        &["M1"],
        &[],
    );
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_comment_inside_a_json_value_is_an_invalid_character() {
    check(
        "ls.db",
        r#"record(lsi,"LS:PLAIN") { field(INP,{const:"hi there"}) }
record(lsi,"LS:BLOCK") { field(INP,{const:/*which*/"hi there"}) }
record(lsi,"LS:KEYCMT") { field(INP,{/*t*/const:"hi there"}) }
"#,
        r#"ERROR: Invalid character '/'
 at or before '/' in path "{path}"  file "ls.db" line 2

 2 | record(lsi,"LS:BLOCK") { field(INP,{const:/*which*/"hi there"}) }

ERROR: Invalid character '*'

 2 | record(lsi,"LS:BLOCK") { field(INP,{const:/*which*/"hi there"}) }

ERROR: Invalid character '*'

 2 | record(lsi,"LS:BLOCK") { field(INP,{const:/*which*/"hi there"}) }

ERROR: Invalid character '/'

 2 | record(lsi,"LS:BLOCK") { field(INP,{const:/*which*/"hi there"}) }

ERROR: syntax error

 2 | record(lsi,"LS:BLOCK") { field(INP,{const:/*which*/"hi there"}) }

ERROR: Failed to load 'ls.db'
"#,
        &["LS:PLAIN", "LS:BLOCK"],
        &["LS:KEYCMT"],
    );
}

/// The grammar's actions rebuild the value with no whitespace, a bare key
/// holding `+`, `-` or `.` quoted, a bare value quoted, an element's
/// trailing comma kept and a member's dropped; softIoc's `dbgf` reads these.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_json_value_is_stored_as_the_grammar_rebuilds_it() {
    let body = "record(ai, \"M1\") { field(DESC, {a : 1 ,})\n}\n\
                record(ai, \"M2\") { field(DESC, [1, 2,]) field(EGU, {\"a.b\":c}) }\n\
                record(ai, \"M3\") { field(DESC, {d-e:1.5e3, f:nullx, g:[true,null]}) }\n";
    let (db, err, _) = load("canon.db", body);
    assert_eq!(err, "");
    assert_eq!(field(&db, "M1", "DESC"), "{a:1}");
    assert_eq!(field(&db, "M2", "DESC"), "[1,2,]");
    assert_eq!(field(&db, "M2", "EGU"), r#"{"a.b":"c"}"#);
    assert_eq!(
        field(&db, "M3", "DESC"),
        r#"{"d-e":1.5e3,f:"nullx",g:[true,null]}"#
    );
}

/// A bare value is the `barechar` run (`dbLex.l:24`); what follows it is
/// read by the grammar again.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_bare_value_ends_at_the_last_barechar() {
    check(
        "f8.db",
        r#"record(ai, "M1") { field(INP, X:Y) }
"#,
        r#"ERROR: syntax error
 at or before ':Y' in path "{path}"  file "f8.db" line 1

 1 | record(ai, "M1") { field(INP, X:Y) }

WARNING: dbReadCOM: Parser stack dirty w/o error. 1
ERROR: Failed to load 'f8.db'
"#,
        &["M1"],
        &[],
    );
}

/// `)` is not JSON punctuation, so an empty value is an invalid character.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_empty_value_is_an_invalid_character() {
    check(
        "g2.db",
        r#"record(ai, "N1") { field(DESC, ) }
"#,
        r#"ERROR: Invalid character ')'
 at or before ')' in path "{path}"  file "g2.db" line 1

 1 | record(ai, "N1") { field(DESC, ) }

ERROR: syntax error

 1 | record(ai, "N1") { field(DESC, ) }

ERROR: Failed to load 'g2.db'
"#,
        &["N1"],
        &[],
    );
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn bare_numbers_and_keywords_are_stored_as_written() {
    let body = "record(ai, \"N1\") { field(DESC, abc) field(PREC, 3) field(EGU, null) }\n\
                record(ai, \"N2\") { field(DESC, a-b.c+d) field(EGU, 01) }\n";
    let (db, err, _) = load("bare.db", body);
    assert_eq!(err, "");
    assert_eq!(field(&db, "N1", "DESC"), "abc");
    assert_eq!(field(&db, "N1", "PREC"), "3");
    assert_eq!(field(&db, "N1", "EGU"), "null");
    assert_eq!(field(&db, "N2", "DESC"), "a-b.c+d");
    assert_eq!(field(&db, "N2", "EGU"), "01");
}
