//! A file that is there but will not open is not found, as C's `fopen`
//! decides in `dbOpenFile` (`dbLexRoutines.c:166-193`): an `include` walks
//! on to the next path entry, or else is `Can't open include file` and the
//! load goes on; a top-level file is `Can't open file`. A socket opens for
//! nobody, root included. softIoc R7.0.10.1 on these files prints the lines
//! pinned below and lists TOP1, L2, AFTER1, TOP2 and AFTER2.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
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
async fn a_db_file_that_will_not_open_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (d1, d2) = (dir.path().join("d1"), dir.path().join("d2"));
    std::fs::create_dir(&d1).expect("d1");
    std::fs::create_dir(&d2).expect("d2");
    let _sockets =
        [d1.join("locked.db"), d1.join("sock.db")].map(|p| UnixListener::bind(p).expect("socket"));
    for n in [1, 2] {
        std::fs::write(
            d1.join(format!("top{n}.db")),
            format!("record(ai, \"TOP{n}\") {{}}\ninclude \"locked.db\"\nrecord(ai, \"AFTER{n}\") {{}}\n"),
        )
        .expect("write top.db");
    }
    std::fs::write(d2.join("locked.db"), "record(ai, \"L2\") {}\n").expect("write locked.db");
    let both = std::env::join_paths([&d1, &d2]).expect("path list");
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let db = Arc::new(PvDatabase::new());
    let shell_db = Arc::clone(&db);
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let d1_shell = d1.clone();
    std::thread::spawn(move || {
        let shell = IocShell::new(shell_db, bridge);
        // SAFETY: as above.
        unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &both) };
        let _ = shell.execute_line("dbLoadRecords(\"top1.db\")");
        // SAFETY: as above.
        unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &d1_shell) };
        let _ = shell.execute_line("dbLoadRecords(\"top2.db\")");
        let _ = shell.execute_line("dbLoadRecords(\"sock.db\")");
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
    let d1 = d1.display();
    let want = format!(
        "\
ERROR: Can't open include file 'locked.db'
ERROR:  at or before '\"locked.db\"' in path \"{d1}\"  file \"top2.db\" line 2

 2 | include \"locked.db\"

ERROR: Failed to load 'top2.db'
ERROR: Can't open file 'sock.db'
ERROR: Failed to load 'sock.db'
"
    );
    assert_eq!(err, want);
    for name in ["TOP1", "L2", "AFTER1", "TOP2", "AFTER2"] {
        assert!(db.get_record(name).is_some(), "{name} is loaded");
    }
}
