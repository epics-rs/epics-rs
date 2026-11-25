//! `lnkCalc_string` takes a string only for `units`, `time` and the three
//! expressions; anywhere else, an `args` element among them, it prints
//! `Unexpected string` and stops (`lnkCalc.c:188-191`), so the field is
//! refused, the load goes on and fails at its end, and the record keeps an
//! empty link. Lines and values are softIoc R7.0.10.1 (8f5015b66).
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"record(ai, "A1") { field(INP, {calc:{expr:"A",args:["X"]}}) }
record(ai, "A2") { field(VAL, "3") }
record(ao, "A3") { field(OUT, {calc:{expr:"A",out:"PV"}}) }
record(ai, "A4") { field(INP, {calc:{expr:"A",prec:"x"}}) }
record(ai, "A5") { field(INP, {calc:"A"}) }
"#;

const WANT: &str = r#"lnkCalc: Unexpected string "X"
ERROR: Can't set 'A1.INP' to '{calc:{expr:"A",args:["X"]}}'  : Bad Field value
ERROR:  at or before ')' in path "{dir}"  file "ls.db" line 1

 1 | record(ai, "A1") { field(INP, {calc:{expr:"A",args:["X"]}}) }

lnkCalc: Unexpected string "PV"
ERROR: Can't set 'A3.OUT' to '{calc:{expr:"A",out:"PV"}}'  : Bad Field value
ERROR: 
 3 | record(ao, "A3") { field(OUT, {calc:{expr:"A",out:"PV"}}) }

lnkCalc: Unexpected string "x"
ERROR: Can't set 'A4.INP' to '{calc:{expr:"A",prec:"x"}}'  : Bad Field value
ERROR: 
 4 | record(ai, "A4") { field(INP, {calc:{expr:"A",prec:"x"}}) }

lnkCalc: Unexpected string "A"
ERROR: Can't set 'A5.INP' to '{calc:"A"}'  : Bad Field value
ERROR: 
 5 | record(ai, "A5") { field(INP, {calc:"A"}) }

ERROR: Failed to load 'ls.db'
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
async fn a_string_outside_its_states_refuses_the_field() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("ls.db"), DB).expect("write the .db");
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
        // A script line runs inside `iocshBody`, which may block (`iocsh.cpp:1122`).
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        IocShell::new(shell_db, bridge).execute_line("dbLoadRecords(\"ls.db\")")
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

    let field = |rec: &str, field: &str| {
        let rec = db.get_record(rec).expect("the record loads");
        let value = rec.read().resolve_field(field).expect("a declared field");
        value.text_g()
    };
    assert_eq!(field("A1", "INP"), "");
    assert_eq!(field("A2", "VAL"), "3");
}
