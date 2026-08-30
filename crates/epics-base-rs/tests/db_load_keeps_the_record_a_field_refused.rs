//! **A `.db` field the loader refuses costs that FIELD, never the record.**
//!
//! C reduces one `field(NAME,"value")` element at a time and every failure
//! arm of `dbRecordField` ends in `yyerror(NULL)` and `return`
//! (`dbLexRoutines.c:1405-1416` @`R7.0.10-146-g8f5015b663d764ad75df`), so the
//! record it belongs to stays in the database, the fields after it in the
//! same block still load, and only the file's status goes non-zero.
//!
//! The port handed the whole field slice to one `apply_fields` call, so the
//! FIRST refusal aborted the record and every field behind it. Measured
//! against `~/work/epics-base/bin/linux-x86_64/softIoc` on the fixture below,
//! driven with `dbLoadRecords("<file>")` then `dbl` from a startup script with
//! stdin on `/dev/null`: C's `dbl` printed `N1` and `N2`, the port's printed
//! nothing at all.
//!
//! Boundaries, one case each: the refusal is the FIRST of two fields in its
//! block (does the sibling behind it still load), the refusal is on a record
//! that is not the last in the file (does the file keep going), and the
//! refusal comes from a different arm — a JSON link naming no registered type
//! rather than a `DBF_NOACCESS` field — so the rule is the applier's, not one
//! arm's.
//!
//! Unix only: what is asserted is the process console, and reaching it means
//! pointing fds 1 and 2 somewhere else and putting them back.

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

/// Write `body` as `name` in a fresh directory, load it into a fresh
/// database, and give back that database, the load's stderr, and the
/// directory C would have echoed in its ` in path "…"` clause.
fn load(name: &str, body: &str) -> (Arc<PvDatabase>, String, String) {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), body).expect("write the .db");
    let found_under = dir.path().display().to_string();
    let db = Arc::new(PvDatabase::new());

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`;
    // nothing else holds either while they are swapped.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under);
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };

    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let line = format!("dbLoadRecords(\"{name}\")");
    let shell_db = Arc::clone(&db);
    let ran =
        std::thread::spawn(move || IocShell::new(shell_db, bridge).execute_line(&line)).join();

    // Restore BEFORE anything can panic, or the failure report has nowhere
    // to go.
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    ran.expect("the shell thread").ok();

    (
        db,
        strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture")),
        found_under,
    )
}

/// The names `dbl` would list, in load order.
async fn listed(db: &PvDatabase) -> Vec<String> {
    let mut names = db.all_record_names().await;
    names.sort();
    names
}

/// A `DBF_NOACCESS` field, C's `dbPutString` arm at `dbStaticLib.c:2646-2650`.
///
/// `RSET` is refused, `EGU` behind it in the same block is not, and `N2` after
/// it in the file is not — which is the whole of what C keeps here.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_refused_noaccess_field_loses_the_field_and_keeps_the_record() {
    let (db, err, dir) = load(
        "noacc.db",
        "record(ai, \"N1\") {\n    field(RSET, \"1\")\n    field(EGU, \"volts\")\n}\n\
         record(ai, \"N2\") {\n    field(PREC, \"2\")\n}\n",
    );

    assert_eq!(
        listed(&db).await,
        ["N1", "N2"],
        "C's dbl printed N1 then N2"
    );

    let rec = db.get_record("N1").expect("N1 survives its refused field");
    let held = rec.read();
    assert_eq!(
        held.record
            .get_field("EGU")
            .map(|v| v.to_string())
            .as_deref(),
        Some("volts"),
        "the field BEHIND the refusal still loads, as it does in C",
    );
    drop(held);

    // C's own sentence, minus the two slots this port cannot fill from a
    // single error value — `pdbentry->message` and `errSymLookup(status)`,
    // which C prints as `Can't set array field before iocInit() : Bad Field
    // value`. The frame, the position and the source echo are C's.
    assert!(
        err.starts_with("ERROR: Can't set 'N1.RSET' to '1' "),
        "C names the record, the field and the value; got:\n{err}"
    );
    assert!(
        err.contains(&format!(
            "ERROR:  at or before ')' in path \"{dir}\"  file \"noacc.db\" line 2\n\n \
             2 |     field(RSET, \"1\")\n"
        )),
        "the refusal carries C's position and source echo; got:\n{err}"
    );
    assert!(
        err.ends_with("ERROR: Failed to load 'noacc.db'\n"),
        "the file's status still goes non-zero; got:\n{err}"
    );
    assert_eq!(
        err.matches("ERROR: Can't set").count(),
        1,
        "one refusal, not one per field behind it; got:\n{err}"
    );
}

/// A different arm of the same applier: a JSON link naming no registered
/// link type, which C reports as `dbJLinkInit: Link type 'bogus' not found`
/// and then refuses through the same `dbRecordField` failure path.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_refused_json_link_loses_the_field_and_keeps_the_record() {
    let (db, err, _) = load(
        "json.db",
        "record(ai, \"J1\") {\n    field(INP, \"{\\\"bogus\\\":}\")\n    field(EGU, \"amps\")\n}\n",
    );

    assert_eq!(listed(&db).await, ["J1"], "C's dbl printed J1");
    let rec = db.get_record("J1").expect("J1 survives its refused link");
    let held = rec.read();
    assert_eq!(
        held.record
            .get_field("EGU")
            .map(|v| v.to_string())
            .as_deref(),
        Some("amps"),
        "the field BEHIND the refusal still loads, as it does in C",
    );
    drop(held);

    assert!(
        err.starts_with("ERROR: Can't set 'J1.INP' to '{\"bogus\":}' "),
        "C prints the value with its quotes stripped and escapes translated; got:\n{err}"
    );
    assert!(
        err.ends_with("ERROR: Failed to load 'json.db'\n"),
        "the file's status still goes non-zero; got:\n{err}"
    );
}
