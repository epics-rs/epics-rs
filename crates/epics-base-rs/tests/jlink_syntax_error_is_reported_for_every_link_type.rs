//! C `dbJLinkParse` prints yajl's rendered error for any link text yajl
//! rejects, whatever the link type (`dbJLink.c:426-433`), unless the first
//! key already named a type `dbjl_map_key` could not find. Lines are
//! softIoc R7.0.10.1 (8f5015b66); `S3` loads.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"record(ai, "S1") { field(INP, "{\"calc\":}") field(DESC, "s1") }
record(ai, "S2") { field(INP, "{,\"calc\":1}") }
record(ai, "S3") { field(INP, "{calc:{expr:'A+1', args:[1,2],},}") }
record(ai, "S4") { field(INP, "{\"calc\":{\"expr\":\"A\" \"x\"}}") }
record(ai, "S5") { field(INP, "{\"calc\":{1:2}}") }
record(ai, "S6") { field(INP, "{\"calc\":{\"expr\":\"1\"} /* x }") }
record(ai, "S7") { field(INP, "{\"nope\":}") }
"#;

const WANT: &str = r#"dbJLinkInit: parse error: unallowed token at this point in JSON text
                               {"calc":}
                     (right here) ------^

ERROR: Can't set 'S1.INP' to '{"calc":}'  : Bad Field value
ERROR:  at or before ')' in path "{dir}"  file "js.db" line 1

 1 | record(ai, "S1") { field(INP, "{\"calc\":}") field(DESC, "s1") }

dbJLinkInit: lexical error: invalid char in json text.
                                      {,"calc":1}
                     (right here) ------^

ERROR: Can't set 'S2.INP' to '{,"calc":1}'  : Bad Field value
ERROR: 
 2 | record(ai, "S2") { field(INP, "{,\"calc\":1}") }

dbJLinkInit: parse error: after key and value, inside map, I expect ',' or '}'
                  {"calc":{"expr":"A" "x"}}
                     (right here) ------^

ERROR: Can't set 'S4.INP' to '{"calc":{"expr":"A" "x"}}'  : Bad Field value
ERROR: 
 4 | record(ai, "S4") { field(INP, "{\"calc\":{\"expr\":\"A\" \"x\"}}") }

dbJLinkInit: lexical error: invalid char in json text.
                              {"calc":{1:2}}
                     (right here) ------^

ERROR: Can't set 'S5.INP' to '{"calc":{1:2}}'  : Bad Field value
ERROR: 
 5 | record(ai, "S5") { field(INP, "{\"calc\":{1:2}}") }

dbJLinkInit: parse error: premature EOF
                                       {"calc":{"expr":"1"} /* x }
                     (right here) ------^

ERROR: Can't set 'S6.INP' to '{"calc":{"expr":"1"} /* x }'  : Bad Field value
ERROR: 
 6 | record(ai, "S6") { field(INP, "{\"calc\":{\"expr\":\"1\"} /* x }") }

dbJLinkInit: Link type 'nope' not found
ERROR: Can't set 'S7.INP' to '{"nope":}'  : Bad Field value
ERROR: 
 7 | record(ai, "S7") { field(INP, "{\"nope\":}") }

ERROR: Failed to load 'js.db'
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
async fn a_link_yajl_rejects_prints_the_yajl_error() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("js.db"), DB).expect("write the .db");
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
        IocShell::new(shell_db, bridge).execute_line("dbLoadRecords(\"js.db\")")
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

    let s1 = db.get_record("S1").expect("S1 loads");
    assert_eq!(s1.read().resolve_field("DESC").unwrap().text_g(), "s1");
    let s3 = db.get_record("S3").expect("S3 loads");
    assert_eq!(
        s3.read().resolve_field("INP").unwrap().text_g(),
        "{calc:{expr:'A+1', args:[1,2],},}"
    );
}
