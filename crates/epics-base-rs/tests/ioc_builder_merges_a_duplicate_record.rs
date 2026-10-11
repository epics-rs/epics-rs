//! A second block for a record already loaded with the same type is applied
//! to that record (`dbRecordHead`, `dbLexRoutines.c:1169-1188`: only a
//! mismatched type or `dbRecordsOnceOnly` is refused). softIoc R7.0.10.1
//! (8f5015b66) on `dup.db` prints nothing and, after `iocInit`, reads
//! `D1.DESC` "first", `D1.EGU` "cm" and `D1.PREC` 3.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DUP_DB: &str = "record(ai, \"D1\") {
    field(DESC, \"first\")
    field(EGU, \"mm\")
}
record(ai, \"D1\") {
    field(EGU, \"cm\")
    field(PREC, \"3\")
}
";

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_same_type_duplicate_adds_and_overrides_fields() {
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global, which is why this test is `serial`.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = IocBuilder::new()
        .db_string(DUP_DB, &HashMap::new())
        .expect("dup.db parses")
        .build()
        .await;
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    let err = std::fs::read_to_string(err_sink.path()).expect("stderr capture");
    assert_eq!(err, "", "softIoc prints nothing for a same-type duplicate");
    let (db, _) = built.expect("the duplicate merges");
    let rec = db.get_record("D1").expect("D1 loads");
    let field = |name: &str| rec.read().resolve_field(name).unwrap().to_string();
    assert_eq!(field("DESC"), "first");
    assert_eq!(field("EGU"), "cm");
    assert_eq!(field("PREC"), "3");
}

/// A duplicate of ANOTHER type takes the same shared install on the builder
/// route as on the shell's. oracleStdIoc (8f5015b66) running `st-std.cmd` with
/// this file prints these lines, then reads `DUP.EGU` "mm", `DUP.DESC`
/// "first", `DUP.SCAN` "Passive" and lists AFTER.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn another_types_duplicate_merges_into_the_existing_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.db");
    std::fs::write(
        &path,
        "record(ao, \"DUP\") { field(DESC, \"first\") }\n\
         record(ai, \"DUP\") { field(EGU, \"mm\") field(SCAN, \"Bogus\") field(INP, \"x\") }\n\
         record(ai, \"AFTER\") { }\n",
    )
    .expect("write a.db");
    let path = path.display().to_string();
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global, which is why this test is `serial`.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = IocBuilder::new()
        .boot_past_failed_loads()
        .db_file(&path, &HashMap::new())
        .expect("a.db is found")
        .build()
        .await;
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    let err = std::fs::read_to_string(err_sink.path())
        .expect("stderr capture")
        .replace("\x1b[31;1mERROR\x1b[0m", "ERROR");
    let line2 =
        " 2 | record(ai, \"DUP\") { field(EGU, \"mm\") field(SCAN, \"Bogus\") field(INP, \"x\") }";
    assert_eq!(
        err,
        format!(
            "\
ERROR: ai record 'DUP' already exists, can't load ao record
ERROR:  at or before ')' in file \"{path}\" line 2

{line2}

ERROR: Can't set 'DUP.SCAN' to 'Bogus' using menu menuScan : Illegal choice
    Did you mean \"I/O Intr\"?
ERROR: 
{line2}

ERROR: ao record 'DUP' doesn't have a field 'INP'
    Did you mean \"OUT\"?  (Output Specification)
ERROR: 
{line2}

ERROR: Failed to load '{path}'
"
        )
    );
    let (db, _) = built.expect("a st.cmd goes on");
    let rec = db.get_record("DUP").expect("DUP loads");
    let field = |name: &str| rec.read().resolve_field(name).unwrap().to_string();
    assert_eq!(field("EGU"), "mm");
    assert_eq!(field("DESC"), "first");
    // menuScan choice 0, "Passive".
    assert_eq!(field("SCAN"), "0");
    assert!(db.get_record("AFTER").is_some(), "AFTER loads");
}
