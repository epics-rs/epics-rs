//! `yyerror`'s ` %d | %s` echo numbers the line in the file being read,
//! `pinputFileNow->line_num` (`dbYacc.y:381`), not the line of the
//! flattened include tree. Expected lines are softIoc R7.0.10.1
//! (8f5015b66) loading `mix.db`; this port echoed 2, 3 and 4 for the last
//! three.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const MIX: &str = r#"record(ai, "X1") { field(DESC, "$(U1)") }
include "inc.db"
record(calc, "X2") { field(CALC, "1+") }
# $(TAIL)
"#;

const INC: &str = r#"record(ai, "I1") { field(DESC, "$(U2)") }
record(calc, "I2") { field(CALC, "*") }
"#;

const ECHOES: [&str; 4] = [
    r#" 1 | record(ai, "X1") { field(DESC, "$(U1,undefined)") }"#,
    r#" 1 | record(ai, "I1") { field(DESC, "$(U2,undefined)") }"#,
    r#" 2 | record(calc, "I2") { field(CALC, "*") }"#,
    r#" 3 | record(calc, "X2") { field(CALC, "1+") }"#,
];

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
async fn yyerror_echo_numbers_the_file_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("mix.db"), MIX).expect("write mix.db");
    std::fs::write(dir.path().join("inc.db"), INC).expect("write inc.db");
    let script = dir.path().join("t.cmd");
    std::fs::write(&script, "dbLoadRecords(\"mix.db\")\n").expect("write the script");

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the shell writes through fd 2 for the length of one script and
    // this test is `serial`, so nothing else holds it.
    let saved = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.display().to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();
    let _ = std::io::stderr().flush();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    assert!(ran.is_ok(), "the shell panicked");

    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    let echoes: Vec<&str> = err.lines().filter(|l| l.contains(" | ")).collect();
    assert_eq!(echoes, ECHOES, "stderr:\n{err}");
}
