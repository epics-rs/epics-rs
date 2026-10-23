//! `IocBuilder` installs its loads as `softIoc -d` reads them: record by
//! record and field by field, each diagnostic at its own line with C's
//! position, a refused field costing only itself, and nothing read past
//! the first load that fails (`softMain.cpp:198`). Lines are softIoc
//! R7.0.10.1 (8f5015b66) on the same files.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;

const B1: &str = r#"record(ai, "R1") {
    field(NOPE, "1")
    field(DESC, "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD")
    field(PINI, "x")
    field(EGU, "v")
}
record(ai, "R2") { field(SCAN, "y") }
"#;
const B2: &str = r#"record(ai, "Q1") { field(PINI, "z") }
"#;
const TOP: &str = r#"include "inc.db"
record(ai, "TOP") { field(PINI, "x") }
"#;
const INC: &str = r#"record(ai, "INC") { field(SCAN, "y") }
"#;

/// `softIoc -d b1.db -d b2.db`.
const WANT_B: &str = r#"ERROR: ai record 'R1' doesn't have a field 'NOPE'
    Did you mean "HOPR"?  (High Operating Range)
ERROR:  at or before ')' in path "{dir}"  file "b1.db" line 2

 2 |     field(NOPE, "1")

ERROR: Can't set 'R1.DESC' to 'DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD'  : String is too long
ERROR: 
 3 |     field(DESC, "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD")

ERROR: Can't set 'R1.PINI' to 'x' using menu menuPini : Illegal choice
ERROR: 
 4 |     field(PINI, "x")

ERROR: Can't set 'R2.SCAN' to 'y' using menu menuScan : Illegal choice
ERROR: 
 7 | record(ai, "R2") { field(SCAN, "y") }

ERROR: Failed to load 'b1.db'
"#;

/// `softIoc -d top.db`.
const WANT_TOP: &str = r#"ERROR: Can't set 'INC.SCAN' to 'y' using menu menuScan : Illegal choice
ERROR:  at or before ')' in path "{dir}"  file "inc.db" line 1
 in path "{dir}"  file "top.db" line 1

 1 | record(ai, "INC") { field(SCAN, "y") }

ERROR: Can't set 'TOP.PINI' to 'x' using menu menuPini : Illegal choice
ERROR: 
 2 | record(ai, "TOP") { field(PINI, "x") }

ERROR: Failed to load 'top.db'
"#;

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

/// Build from `files` (loaded in order from a directory holding `texts`)
/// and give back the build's result and its stderr.
async fn build(texts: &[(&str, &str)], files: &[&str]) -> (CaError, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text) in texts {
        std::fs::write(dir.path().join(name), text).expect("write");
    }
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why these tests are `serial`.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let mut builder = IocBuilder::new();
    for file in files {
        builder = builder
            .db_file(file, &HashMap::new())
            .expect("the file parses");
    }
    let built = builder.build().await;
    // SAFETY: restoring what this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    let Err(err) = built else {
        panic!("the load must fail");
    };
    let stderr = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    (err, stderr, dir.path().display().to_string())
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn each_field_is_reported_at_its_line_and_the_next_load_is_not_read() {
    let (err, stderr, dir) = build(&[("b1.db", B1), ("b2.db", B2)], &["b1.db", "b2.db"]).await;
    assert_eq!(stderr, WANT_B.replace("{dir}", &dir));
    assert!(
        matches!(&err, CaError::DbLoadFailed(f) if f == "b1.db"),
        "{err:?}"
    );
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_included_line_is_reported_with_its_include_frame() {
    let (err, stderr, dir) = build(&[("top.db", TOP), ("inc.db", INC)], &["top.db"]).await;
    assert_eq!(stderr, WANT_TOP.replace("{dir}", &dir));
    assert!(
        matches!(&err, CaError::DbLoadFailed(f) if f == "top.db"),
        "{err:?}"
    );
}

/// The builder names an unknown type as `dbRecordHead` does; softIoc `-d e.db`
/// prints these lines and loads nothing after `LT`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_unknown_record_type_is_not_found() {
    let e = "record(transform, \"LT\") { }\nrecord(ai, \"E2\") { }\n";
    let (err, stderr, dir) = build(&[("e.db", e)], &["e.db"]).await;
    assert_eq!(
        stderr,
        format!(
            "\
ERROR: Record type 'transform' for record 'LT' not found
ERROR:  at or before ')' in path \"{dir}\"  file \"e.db\" line 1

 1 | record(transform, \"LT\") {{ }}

ERROR: Failed to load 'e.db'
"
        )
    );
    assert!(
        matches!(&err, CaError::DbLoadFailed(f) if f == "e.db"),
        "{err:?}"
    );
}
