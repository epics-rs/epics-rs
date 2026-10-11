//! C reads, parses and installs a `.db` one record at a time, so every
//! diagnostic comes out in file order: a parse-time refusal (an unknown field)
//! after the install-time refusals of the record before it, an apply-time
//! refusal (`dbParseLink`'s report included) ahead of a menu refusal on a
//! later line, and a record-head fault on its head's line. Every byte below
//! is `softIoc` R7.0.10-146-g8f5015b66 loading this exact file with
//! `EPICS_DB_INCLUDE_PATH` at its directory, ANSI stripped.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const PO_DB: &str = r#"record(ai, "H") {
    field(INP, "{\"const\":}")
    field(SCAN, "Bogus")
}
record(ao, "B") {
    field(NOSUCH, "1")
    field(OIF, "Nope")
}
record(ao, "H") {
    field(VAL, "1")
}
"#;

const WANT: &str = r#"dbJLinkInit: parse error: unallowed token at this point in JSON text
                              {"const":}
                     (right here) ------^

ERROR: Can't set 'H.INP' to '{"const":}'  : Bad Field value
ERROR:  at or before ')' in path "{DIR}"  file "po.db" line 2

 2 |     field(INP, "{\"const\":}")

ERROR: Can't set 'H.SCAN' to 'Bogus' using menu menuScan : Illegal choice
    Did you mean "I/O Intr"?
ERROR: 
 3 |     field(SCAN, "Bogus")

ERROR: ao record 'B' doesn't have a field 'NOSUCH'
    Did you mean "DESC"?  (Descriptor)
ERROR: 
 6 |     field(NOSUCH, "1")

ERROR: Can't set 'B.OIF' to 'Nope' using menu aoOIF : Illegal choice
    Did you mean "Incremental"?
ERROR: 
 7 |     field(OIF, "Nope")

ERROR: ao record 'H' already exists, can't load ai record
ERROR: 
 9 | record(ao, "H") {

ERROR: Failed to load 'po.db'
"#;

/// Run one iocsh line with fd 2 pointed at a file: macLib's notice goes
/// through errlog and the loader's lines through stderr, and the process
/// console is the one place their order is the operator's.
fn stderr_of(line: &str) -> String {
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
    let db = Arc::new(PvDatabase::new());
    let ran = std::thread::spawn(move || {
        // A script line runs inside `iocshBody`, which may block (`iocsh.cpp:1122`).
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        IocShell::new(db, bridge).execute_line(&owned)
    })
    .join();
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the shell thread").ok();
    let raw = std::fs::read_to_string(sink.path()).expect("read the capture");
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = rest[at..].find('m').map_or("", |end| &rest[at + end + 1..]);
    }
    out.push_str(rest);
    out
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn every_diagnostic_prints_in_file_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("po.db"), PO_DB).expect("write the .db");
    let dir_text = dir.path().display().to_string();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &dir_text) };
    let got = stderr_of("dbLoadRecords(\"po.db\")");
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    let want = WANT.replace("{DIR}", &dir_text);
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}
