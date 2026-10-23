//! A missing include is `yyerror(NULL)` (`dbIncludeNew`,
//! `dbLexRoutines.c:443-457`): the refusal carries the position of the
//! `include` line and its include stack, and the records around it still
//! load. softIoc R7.0.10.1 (8f5015b66) on `dbLoadRecords("top.db")` and
//! `dbLoadRecords("outer.db")` prints the lines pinned below and lists I1, I2
//! and M1.
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

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_missing_include_names_its_line_and_stack() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("top.db"),
        "record(ai, \"I1\") {}\ninclude \"missing.db\"\nrecord(ai, \"I2\") {}\n",
    )
    .expect("write top.db");
    std::fs::write(
        dir.path().join("outer.db"),
        "record(ai, \"M1\") {}\ninclude \"top.db\"\n",
    )
    .expect("write outer.db");
    let path = dir.path().display().to_string();
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let db = Arc::new(PvDatabase::new());
    let shell_db = Arc::clone(&db);
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    std::thread::spawn(move || {
        let shell = IocShell::new(shell_db, bridge);
        let _ = shell.execute_line("dbLoadRecords(\"top.db\")");
        let _ = shell.execute_line("dbLoadRecords(\"outer.db\")");
    })
    .join()
    .expect("the shell thread");
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    let want = format!(
        "\
ERROR: Can't open include file 'missing.db'
ERROR:  at or before '\"missing.db\"' in path \"{path}\"  file \"top.db\" line 2

 2 | include \"missing.db\"

ERROR: Failed to load 'top.db'
ERROR: Can't open include file 'missing.db'
ERROR:  at or before '\"missing.db\"' in path \"{path}\"  file \"top.db\" line 2
 in path \"{path}\"  file \"outer.db\" line 2

 2 | include \"missing.db\"

ERROR: Failed to load 'outer.db'
"
    );
    assert_eq!(err, want);
    for name in ["I1", "I2", "M1"] {
        assert!(db.get_record(name).is_some(), "{name} is loaded");
    }
}
