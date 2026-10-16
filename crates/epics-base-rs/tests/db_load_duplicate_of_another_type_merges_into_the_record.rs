//! C `dbRecordHead` refuses a duplicate of another record type with
//! `yyerror(NULL)` but leaves `pdbentry` on the existing record
//! (`dbLexRoutines.c:1171-1177`), so the body is applied to it and each
//! field name is judged against the EXISTING type, as for `record("*",…)`
//! (`:1135-1143`). Lines and values are softIoc R7.0.10.1 (8f5015b66).
//!
//! Unix only: what is asserted is the process console.

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

/// Load `body` as `name` with `dbLoadRecords` and give back the database and
/// the load's stderr.
fn load(name: &str, body: &str) -> (Arc<PvDatabase>, String) {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), body).expect("write the .db");
    let db = Arc::new(PvDatabase::new());
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let line = format!("dbLoadRecords(\"{name}\")");
    let shell_db = Arc::clone(&db);
    let ran =
        std::thread::spawn(move || IocShell::new(shell_db, bridge).execute_line(&line)).join();
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
    )
}

fn field(db: &PvDatabase, rec: &str, field: &str) -> String {
    let rec = db.get_record(rec).expect("the record loads");
    let value = rec.read().resolve_field(field).expect("a declared field");
    value.to_string()
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn the_body_lands_on_the_existing_record_and_is_judged_by_its_type() {
    let body = r#"record(ai, "X") { field(EGU, "v") }
record(calc, "X") {
    field(DESC, "from calc block")
    field(CALC, "A+1")
    field(PREC, "3")
}
record(bo, "Y") { field(ZNAM, "off") }
record("*", "Y") {
    field(ONAM, "on")
    field(EGU, "nope")
}
"#;
    let (db, err) = load("dup.db", body);
    let lines: Vec<&str> = err.lines().collect();
    // softIoc's stderr, but for the include directory C names in line 2.
    let expected = [
        "ERROR: calc record 'X' already exists, can't load ai record",
        "",
        "",
        " 2 | record(calc, \"X\") {",
        "",
        "ERROR: ai record 'X' doesn't have a field 'CALC'",
        "    Did you mean \"DESC\"?  (Descriptor)",
        "ERROR: ",
        " 4 |     field(CALC, \"A+1\")",
        "",
        "ERROR: bo record 'Y' doesn't have a field 'EGU'",
        "    Did you mean \"DESC\"?  (Descriptor)",
        "ERROR: ",
        " 10 |     field(EGU, \"nope\")",
        "",
        "ERROR: Failed to load 'dup.db'",
    ];
    assert_eq!(lines.len(), expected.len(), "{err}");
    for (i, (got, want)) in lines.iter().zip(expected).enumerate() {
        if i == 1 {
            assert!(
                got.starts_with("ERROR:  at or before ')' in path \"")
                    && got.ends_with("\"  file \"dup.db\" line 2"),
                "{err}"
            );
        } else if i != 2 {
            assert_eq!(*got, want, "{err}");
        }
    }
    assert!(lines[2].is_empty(), "{err}");

    let x = db.get_record("X").expect("X");
    assert_eq!(x.read().record.record_type(), "ai");
    assert_eq!(field(&db, "X", "DESC"), "from calc block");
    assert_eq!(field(&db, "X", "PREC"), "3");
    assert_eq!(field(&db, "X", "EGU"), "v");
    assert_eq!(field(&db, "Y", "ONAM"), "on");
    assert_eq!(field(&db, "Y", "ZNAM"), "off");
}
