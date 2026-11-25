//! A `.db` value that does not fit its `DBF_STRING` field is refused by
//! `dbPutString` with `S_dbLib_strLen` before anything is copied
//! (`dbStaticLib.c:2589`), so the field keeps what it held and the load
//! fails. The limit is the declared `size` less the NUL: 40 for dbCommon
//! `DESC`, 119 for transform's `CLCx`. Lines and values are softIoc
//! R7.0.10.1 (8f5015b66) for `DESC` and oracleIoc (calc transform) for
//! `CLCA`.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::transform::TransformRecord;

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
    value.text_g()
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_desc_past_40_characters_is_refused_and_the_field_kept() {
    let d41 = "D".repeat(41);
    let e40 = "E".repeat(40);
    let body = format!(
        "record(ai, \"A1\") {{ field(DESC, \"{d41}\") field(EGU, \"x\") }}\n\
         record(ai, \"A2\") {{ field(DESC, \"{e40}\") }}\n\
         record(ai, \"A3\") {{ field(DESC, \"keep\") }}\n\
         record(ai, \"A3\") {{ field(DESC, \"{d41}\") }}\n"
    );
    let (db, err) = load("desc.db", &body);
    let refusal =
        |rec: &str| format!("ERROR: Can't set '{rec}.DESC' to '{d41}'  : String is too long\n");
    assert!(err.starts_with(&refusal("A1")), "{err}");
    assert!(err.contains(&refusal("A3")), "{err}");
    assert!(!err.contains("A2.DESC"), "{err}");
    assert!(err.contains("ERROR: Failed to load 'desc.db'"), "{err}");
    assert_eq!(field(&db, "A1", "DESC"), "");
    assert_eq!(field(&db, "A1", "EGU"), "x");
    assert_eq!(field(&db, "A2", "DESC"), e40);
    assert_eq!(field(&db, "A3", "DESC"), "keep");
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_transform_clca_past_119_characters_is_refused() {
    epics_base_rs::server::db_loader::register_record_type(
        "transform",
        Box::new(|| Box::new(TransformRecord::default())),
    );
    let clca129 = format!("{}1", "1+".repeat(64));
    let clca119 = format!("{}1", "1+".repeat(59));
    let body = format!(
        "record(transform, \"T1\") {{ field(CLCA, \"{clca129}\") field(CLCB, \"2\") }}\n\
         record(transform, \"T2\") {{ field(CLCA, \"{clca119}\") }}\n"
    );
    let (db, err) = load("clc.db", &body);
    assert!(
        err.starts_with(&format!(
            "ERROR: Can't set 'T1.CLCA' to '{clca129}'  : String is too long\n"
        )),
        "{err}"
    );
    assert!(!err.contains("T2.CLCA"), "{err}");
    assert_eq!(field(&db, "T1", "CLCA"), "");
    assert_eq!(field(&db, "T1", "CLCB"), "2");
    assert_eq!(field(&db, "T2", "CLCA"), clca119);
}
