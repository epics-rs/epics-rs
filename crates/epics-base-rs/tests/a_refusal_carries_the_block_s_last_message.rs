//! C allocates one `DBENTRY` per `record()` block (`dbLexRoutines.c:1131`)
//! and never clears its `message`, so a refusal prints whatever text an
//! earlier refusal of the same block left behind (`dbLexRoutines.c:1410-1413`):
//! a link or numeric refusal after a menu or `DBF_NOACCESS` one carries the
//! older text, and the next block starts empty. Every byte below is `softIoc`
//! R7.0.10-146-g8f5015b66 loading this exact file with `EPICS_DB_INCLUDE_PATH`
//! at its directory, ANSI stripped.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const SLOT_DB: &str = r#"record(ai, "M") {
    field(INP, "{\"const\":}")
    field(SCAN, "Bogus")
    field(INP, "{\"const\":}")
    field(PREC, "x")
}
record(ai, "M") {
    field(PREC, "y")
    field(PBRK, "z") field(INP, "{\"const\":}")
    field(LINR, "nope") field(PBRK, "w")
}
record(ao, "N") {
    field(OIF, "x")
}
record(ao, "N") {
    field(DRVH, "q")
}
"#;

const WANT: &str = r#"dbJLinkInit: parse error: unallowed token at this point in JSON text
                              {"const":}
                     (right here) ------^

ERROR: Can't set 'M.INP' to '{"const":}'  : Bad Field value
ERROR:  at or before ')' in path "{DIR}"  file "slot.db" line 2

 2 |     field(INP, "{\"const\":}")

ERROR: Can't set 'M.SCAN' to 'Bogus' using menu menuScan : Illegal choice
    Did you mean "I/O Intr"?
ERROR: 
 3 |     field(SCAN, "Bogus")

dbJLinkInit: parse error: unallowed token at this point in JSON text
                              {"const":}
                     (right here) ------^

ERROR: Can't set 'M.INP' to '{"const":}' using menu menuScan : Bad Field value
ERROR: 
 4 |     field(INP, "{\"const\":}")

ERROR: Can't set 'M.PREC' to 'x' using menu menuScan : No digits to convert
ERROR: 
 5 |     field(PREC, "x")

ERROR: Can't set 'M.PREC' to 'y'  : No digits to convert
ERROR: 
 8 |     field(PREC, "y")

ERROR: Can't set 'M.PBRK' to 'z' Can't set array field before iocInit() : Bad Field value
ERROR: 
 9 |     field(PBRK, "z") field(INP, "{\"const\":}")

dbJLinkInit: parse error: unallowed token at this point in JSON text
                              {"const":}
                     (right here) ------^

ERROR: Can't set 'M.INP' to '{"const":}' Can't set array field before iocInit() : Bad Field value
ERROR: 
 9 |     field(PBRK, "z") field(INP, "{\"const\":}")

ERROR: Can't set 'M.LINR' to 'nope' using menu menuConvert : Illegal choice
    Did you mean "SLOPE"?
ERROR: 
 10 |     field(LINR, "nope") field(PBRK, "w")

ERROR: Can't set 'M.PBRK' to 'w' Can't set array field before iocInit() : Bad Field value
ERROR: 
 10 |     field(LINR, "nope") field(PBRK, "w")

ERROR: Can't set 'N.OIF' to 'x' using menu aoOIF : Illegal choice
ERROR: 
 13 |     field(OIF, "x")

ERROR: Can't set 'N.DRVH' to 'q'  : No digits to convert
ERROR: 
 16 |     field(DRVH, "q")

ERROR: Failed to load 'slot.db'
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
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&owned)).join();
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
async fn a_refusal_prints_the_message_an_earlier_one_left() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("slot.db"), SLOT_DB).expect("write the .db");
    let dir_text = dir.path().display().to_string();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &dir_text) };
    let got = stderr_of("dbLoadRecords(\"slot.db\")");
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    let want = WANT.replace("{DIR}", &dir_text);
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}
