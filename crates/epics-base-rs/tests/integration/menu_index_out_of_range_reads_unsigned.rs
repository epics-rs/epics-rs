//! A menu field holding an index past its choices reads back as the index's
//! `epicsEnum16` bits, `%u`: `cvt_menu_st` for `dbgf` and a `DBR_STRING`
//! link read (`dbFastLinkConv.c:1590-1597`), `dbGetStringNum` for `dbpr`
//! (`dbStaticLib.c:2140-2142`). `SIML "-1"` loads SIMM at `DBF_USHORT`, so it
//! holds 65535; softIoc R7.0.10.1 (8f5015b66) prints the lines below.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"
record(ai, "A") { field(SIML, "-1") }
record(stringin, "S") { field(INP, "A.SIMM") }
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

/// What the shell printed on stdout running `lines` after the load.
fn console(lines: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("t.db"), DB).expect("write the .db");
    let script = dir.path().join("t.cmd");
    std::fs::write(
        &script,
        format!("dbLoadRecords(\"t.db\")\niocInit\n{lines}"),
    )
    .expect("write the script");

    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    // SAFETY: the shell writes through fd 1 for the length of one script and
    // this test is `serial`, so nothing else holds it.
    let saved = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(1);
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        saved
    };
    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.display().to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();
    let _ = std::io::stdout().flush();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    assert!(ran.is_ok(), "the shell panicked");
    strip_ansi(&std::fs::read_to_string(out_sink.path()).expect("stdout capture"))
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn menu_index_out_of_range_reads_unsigned() {
    let out = console("dbgf A.SIMM\ndbpf S.PROC 1\ndbgf S.VAL\ndbpr A 1\n");
    let want_dbgf = "dbgf A.SIMM\nDBF_STRING:         \"65535\"   \n";
    let want_link = "dbgf S.VAL\nDBF_STRING:         \"65535\"   \n";
    assert!(out.contains(want_dbgf), "dbgf A.SIMM:\n{out}");
    assert!(out.contains(want_link), "link read:\n{out}");
    assert!(out.contains("SIMM: 65535         "), "dbpr A 1:\n{out}");
}
