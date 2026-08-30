//! **A `.db` rejection reaches the operator through ONE owner, on stderr,
//! with C's sentence.**
//!
//! Measured against `~/work/epics-base/bin/linux-x86_64/softIoc`
//! (R7.0.10-146-g8f5015b663d764ad75df) driven with
//! `dbLoadRecords("<file>")` from a startup script, stdin on `/dev/null`,
//! ANSI stripped, `softMain`'s trailing `epics> ` prompt dropped. The
//! search path is handed to both shells through `EPICS_DB_INCLUDE_PATH`,
//! which is the only thing C's ` in path "…"` clause echoes — C printed
//! `"."` when the file sat in its cwd and the directory's own name when
//! it did not.
//!
//! Two of these three already matched C byte for byte before this file
//! existed, `Did you mean` included: the diagnostic machinery — the
//! similarity scorer, the confusion map, the ` at or before` locator,
//! `yyFailed`'s once-only rule and the ` N | <source>` echo — is ported.
//! They are here because that is worth a regression gate, and because
//! the third one is what a missing case actually looks like.
//!
//! There is NO caret. `rg` over `modules/database/src/ioc/dbStatic/`
//! finds no `^` written to any stream; what C prints is a gcc-style
//! gutter, ` %d | %s` (`dbYacc.y:381`), and nothing under it.
//!
//! Unix only: what is asserted is the process console, and reaching it
//! means pointing fds 1 and 2 somewhere else and putting them back.

#![cfg(unix)]

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

/// Both streams of one `dbLoadRecords` line.
///
/// BOTH, not just stderr: the defect this file gates is a diagnostic
/// that went to stdout, and a test that reads only stderr cannot see
/// the difference between "printed on the wrong stream" and "printed".
fn streams_of(line: &str, db: Arc<PvDatabase>) -> (String, String) {
    use std::os::fd::AsRawFd;
    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the shell writes through fds 1 and 2 for the length of one
    // line and this test is `serial`, so nothing else holds either while
    // they are swapped.
    let (saved_out, saved_err) = unsafe {
        let saved = (libc::dup(1), libc::dup(2));
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };

    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let owned = line.to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&owned)).join();

    // Restore BEFORE anything can panic, or the failure report has
    // nowhere to go.
    // SAFETY: restoring the two descriptors this call replaced.
    unsafe {
        libc::dup2(saved_out, 1);
        libc::dup2(saved_err, 2);
        libc::close(saved_out);
        libc::close(saved_err);
    }
    ran.expect("the shell thread").ok();

    (
        strip_ansi(&std::fs::read_to_string(out_sink.path()).expect("stdout capture")),
        strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture")),
    )
}

/// Write `body` as `name` in a fresh directory, load it, and give back
/// `(stdout, stderr)` with the directory's own name folded away.
fn load(name: &str, body: &str) -> (String, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), body).expect("write the .db");
    let found_under = dir.path().display().to_string();

    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    let (out, err) = streams_of(
        &format!("dbLoadRecords(\"{name}\")"),
        Arc::new(PvDatabase::new()),
    );
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    (out, err, found_under)
}

/// An unknown FIELD, with C's suggestion under it: the whole shape,
/// byte for byte.
///
/// `DESC` is what C's weighted `epicsStrSimilarity`
/// (`dbLexRoutines.c:1242-1385`) proposes for `NOSUCHFLD`, prompt string
/// and all, and the two spaces before `at or before` are `yyerror(NULL)`
/// writing a bare `ERROR: ` that the position's own clause continues.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_unknown_field_carries_cs_suggestion_and_position() {
    let (out, err, dir) = load(
        "bad2.db",
        "record(ai, \"A2\") {\n    field(NOSUCHFLD, \"3\")\n}\n",
    );
    assert_eq!(out, "", "C writes no diagnostic to stdout");
    assert_eq!(
        err,
        format!(
            "\
ERROR: ai record 'A2' doesn't have a field 'NOSUCHFLD'
    Did you mean \"DESC\"?  (Descriptor)
ERROR:  at or before ')' in path \"{dir}\"  file \"bad2.db\" line 2

 2 |     field(NOSUCHFLD, \"3\")

ERROR: Failed to load 'bad2.db'
"
        )
    );
}

/// A menu choice one letter off, through `dbStaticLib.c:2670-2704`'s
/// unweighted scorer this time. Same shape, different suggester — which
/// is why both are gated and not just one.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_misspelt_menu_choice_carries_cs_suggestion_and_position() {
    let (out, err, dir) = load(
        "bad5.db",
        "record(ai, \"A5\") {\n    field(SCAN, \"1 secnd\")\n}\n",
    );
    assert_eq!(out, "", "C writes no diagnostic to stdout");
    assert_eq!(
        err,
        format!(
            "\
ERROR: Can't set 'A5.SCAN' to '1 secnd' using menu menuScan : Illegal choice
    Did you mean \"1 second\"?
ERROR:  at or before ')' in path \"{dir}\"  file \"bad5.db\" line 2

 2 |     field(SCAN, \"1 secnd\")

ERROR: Failed to load 'bad5.db'
"
        )
    );
}

/// An unknown RECORD TYPE — C's `yyerrorAbort` class, and the one that
/// was leaving by a different door.
///
/// C `dbRecordHead` (`dbLexRoutines.c:1162-1167`) builds the sentence out
/// of the type and the record NAME, neither of which the port's record
/// factory sees, so this port forwarded the factory's error instead and
/// an operator read `DB parse error at line 0, column 0: unknown record
/// type: 'nosuchtype'` — on STDOUT, where a `2>` startup log never saw
/// it, in this port's own words, naming a position it did not have.
///
/// Two things C prints here that this assertion does not, both deliberate:
///
/// 1. the position (` at or before ')' … line 1` and the source echo).
///    C's lexer is still parked on the record head; this port checks the
///    type in the install loop, after the text is gone, and
///    `DbRecordDef` carries no line the way `DbFieldDef` does. Naming a
///    line it does not hold would be a guess, so it names none —
///    `db_loader`'s struct is where that line would have to start.
/// 2. a SECOND `ERROR: syntax error` with the same source line echoed
///    under it again. That is bison unwinding its stack after `yyAbort`,
///    not a second thing wrong with the file, and this port has no bison
///    stack to unwind. Same for C's `WARNING: dbReadCOM: Parser stack
///    dirty w/o error. 1` (`dbLexRoutines.c:303-306`), which reports
///    `ellCount(&tempList)` — a C parser-internal list with no
///    counterpart here.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_unknown_record_type_gets_cs_sentence_on_cs_stream() {
    let (out, err, _dir) = load(
        "bad4.db",
        "record(nosuchtype, \"A4\") {\n    field(VAL, \"1\")\n}\n",
    );
    assert_eq!(
        out, "",
        "the rejection belongs on stderr; C never writes one to stdout"
    );
    assert_eq!(
        err,
        "\
ERROR: Record type 'nosuchtype' for record 'A4' not found
ERROR: Failed to load 'bad4.db'
"
    );
}

/// A record type the port DECLARES but cannot construct — C's rec_size == 0,
/// not C's "not found".
///
/// Measured on `softIoc` with a `.dbd` that declares `recordtype(zzz)` and no
/// `registerRecordDeviceDriver` to fill in its size:
///
/// ```text
/// stdout: \t*** Did you run x_RegisterRecordDeviceDriver(pdbbase) yet? ***
/// stderr: dbAllocRecord(Z1) with zzz rec_size = 0
///         ERROR: Can't create zzz record 'Z1'
/// ```
///
/// `swait` is this port's version of that state: `stdRecords.dbd` does not
/// carry it, the crate vendors its `.dbd` anyway, and an application opts
/// into the factory with `register_record_type`. Until it does, the type has
/// a shape and no support — which is what C's `rec_size == 0` means, and why
/// this is not the `Record type … not found` line the case below gets.
///
/// The hint goes to STDOUT and the size line to stderr because that is where
/// C puts them: `printf` and `epicsPrintf` respectively
/// (`dbStaticRun.c:92-94`). An operator redirecting only one of the two
/// streams sees half of C's answer, and would have seen none of ours.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_declared_but_unregistered_record_type_is_cs_rec_size_zero() {
    let (out, err, _dir) = load(
        "unreg.db",
        "record(swait, \"Z1\") {\n    field(SCAN, \"Passive\")\n}\n",
    );
    assert_eq!(
        out, "\t*** Did you run x_RegisterRecordDeviceDriver(pdbbase) yet? ***\n",
        "C's printf half, on stdout"
    );
    assert_eq!(
        err,
        "\
dbAllocRecord(Z1) with swait rec_size = 0
ERROR: Can't create swait record 'Z1'
ERROR: Failed to load 'unreg.db'
"
    );
}
