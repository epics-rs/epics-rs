//! A link type's `alloc_jlink` sees the link field's type and may refuse
//! it: lnkConst anything but `DBF_INLINK`, lnkCalc and lnkState
//! `DBF_FWDLINK` (`lnkConst.c:50-53`, `lnkCalc.c:81-84`, `lnkState.c:54-57`).
//! `dbjl_map_key` then prints its own line and stops at the key, before yajl
//! reaches a syntax error later in the text (`dbJLink.c:278-283`). Lines and
//! values are softIoc R7.0.10.1 (8f5015b66).
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"record(ai, "F1") { field(FLNK, "{\"calc\":{\"expr\":\"1\"}}") field(DESC, "f1") }
record(ai, "F2") { field(FLNK, "{\"state\":\"s\"}") }
record(ao, "F3") { field(OUT, "{\"const\":1}") }
record(ai, "F4") { field(FLNK, "{\"const\":}") }
record(ao, "F5") { field(OUT, "{\"state\":\"s\"}") }
record(ai, "F6") { field(INP, "{\"const\":1}") }
"#;

const WANT: &str = r#"lnkCalc: No support for forward links
dbJLinkInit: Link type 'calc' allocation failed. 
ERROR: Can't set 'F1.FLNK' to '{"calc":{"expr":"1"}}'  : Bad Field value
ERROR:  at or before ')' in path "{dir}"  file "jf.db" line 1

 1 | record(ai, "F1") { field(FLNK, "{\"calc\":{\"expr\":\"1\"}}") field(DESC, "f1") }

lnkState: DBF_FWDLINK not supported
dbJLinkInit: Link type 'state' allocation failed. 
ERROR: Can't set 'F2.FLNK' to '{"state":"s"}'  : Bad Field value
ERROR: 
 2 | record(ai, "F2") { field(FLNK, "{\"state\":\"s\"}") }

lnkConst: Only works with input links
dbJLinkInit: Link type 'const' allocation failed. 
ERROR: Can't set 'F3.OUT' to '{"const":1}'  : Bad Field value
ERROR: 
 3 | record(ao, "F3") { field(OUT, "{\"const\":1}") }

lnkConst: Only works with input links
dbJLinkInit: Link type 'const' allocation failed. 
ERROR: Can't set 'F4.FLNK' to '{"const":}'  : Bad Field value
ERROR: 
 4 | record(ai, "F4") { field(FLNK, "{\"const\":}") }

ERROR: Failed to load 'jf.db'
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
async fn a_link_type_refusing_the_field_type_prints_both_lines() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("jf.db"), DB).expect("write the .db");
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
        IocShell::new(shell_db, bridge).execute_line("dbLoadRecords(\"jf.db\")")
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
        value.to_string()
    };
    assert_eq!(field("F1", "DESC"), "f1");
    assert_eq!(field("F1", "FLNK"), "");
    assert_eq!(field("F3", "OUT"), "");
    assert_eq!(field("F5", "OUT"), r#"{"state":"s"}"#);
    assert_eq!(field("F6", "INP"), r#"{"const":1}"#);
}
