//! Every `alias(...)` goes through C `createAlias` (`dbLexRoutines.c:1451-1483`):
//! a name already aliasing the same record is silent, one aliasing another
//! record or naming a record is refused in its own words, and an alias of
//! an alias names the real record (`dbStaticLib.c:1675-1677`). Lines and
//! values are softIoc R7.0.10.1 (8f5015b66).
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"record(ai, "A") { alias("A2") }
record(ai, "B") { alias("A2") }
record(ai, "C") { alias("A") }
alias("A", "A2")
alias("B", "B")
alias("A2", "A3")
"#;

const WANT: &str = r#"ERROR: Alias 'A2' for record 'B' already aliases 'A'
ERROR:  at or before ')' in path "{dir}"  file "aw.db" line 2

 2 | record(ai, "B") { alias("A2") }

ERROR: Alias 'A' for record 'C' is already a record name.
ERROR: 
 3 | record(ai, "C") { alias("A") }

ERROR: Alias 'B' for record 'B' is already a record name.
ERROR: 
 5 | alias("B", "B")

ERROR: Failed to load 'aw.db'
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

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn alias_refusals_are_create_alias_s() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("aw.db"), DB).expect("write the .db");
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
    let shell_db = Arc::clone(&db);
    let ran = std::thread::spawn(move || {
        IocShell::new(shell_db, bridge).execute_line("dbLoadRecords(\"aw.db\")")
    })
    .join();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    ran.expect("the shell thread").ok();
    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    let want = WANT.replace("{dir}", &dir.path().display().to_string());
    assert_eq!(err, want);

    // softIoc: `dbgf A3.NAME` is "A".
    assert_eq!(db.resolve_alias("A3").as_deref(), Some("A"));
    assert_eq!(db.resolve_alias("A2").as_deref(), Some("A"));
}
