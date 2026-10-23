//! A file-scope `alias("record","new")` is C `dbAlias` run where it stands
//! (`dbLexRoutines.c:1501-1520`): its target is looked up in the database
//! as loaded up to that line, so a record declared further down is
//! unknown, and its refusal prints between the lines around it. Lines and
//! values are softIoc R7.0.10.1 (8f5015b66).
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"record(ai, "A") { alias("A2") }
record(ai, "B") { alias("A2") }
record(ai, "C") { alias("A") }
alias("A", "A2")
alias("NOPE", "N2")
alias("B", "B")
alias("B", "C")
record(ai, "D") { field(DESC, "d") }
alias("D", "A")
"#;

const WANT: &str = r#"ERROR: Alias 'A2' for record 'B' already aliases 'A'
ERROR:  at or before ')' in path "{dir}"  file "al.db" line 2

 2 | record(ai, "B") { alias("A2") }

ERROR: Alias 'A' for record 'C' is already a record name.
ERROR: 
 3 | record(ai, "C") { alias("A") }

ERROR: Alias 'N2' names an unknown record 'NOPE'
ERROR: 
 5 | alias("NOPE", "N2")

ERROR: Alias 'B' for record 'B' is already a record name.
ERROR: 
 6 | alias("B", "B")

ERROR: Alias 'C' for record 'B' is already a record name.
ERROR: 
 7 | alias("B", "C")

ERROR: Alias 'A' for record 'D' is already a record name.
ERROR: 
 9 | alias("D", "A")

ERROR: Failed to load 'al.db'
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

/// Run `f` with fd 2 captured and give back what it wrote there.
fn capture_stderr<R>(f: impl FnOnce() -> R) -> (R, String) {
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global, which is why these tests are `serial`.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let out = f();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    (out, err)
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn db_load_records_runs_each_file_alias_at_its_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("al.db"), DB).expect("write the .db");
    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = Arc::clone(&db);
    let path = dir.path().to_path_buf();
    let (ran, err) = capture_stderr(move || {
        // SAFETY: the process-global env is why this test is `serial`.
        unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &path) };
        let ran = std::thread::spawn(move || {
            IocShell::new(shell_db, bridge).execute_line("dbLoadRecords(\"al.db\")")
        })
        .join();
        // SAFETY: as above.
        unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
        ran
    });
    ran.expect("the shell thread").ok();
    let want = WANT.replace("{dir}", &dir.path().display().to_string());
    assert_eq!(err, want);

    // softIoc `dbl`: A A2 B C D, and A2 stays A's.
    assert_eq!(db.resolve_alias("A2").as_deref(), Some(&b"A"[..]));
    assert!(db.resolve_alias("N2").is_none());
}

/// The builder's loads are C's successive `dbLoadRecords`: an alias names a
/// record an earlier file loaded, but not one its own file declares below
/// it. softIoc on the same two files prints only the `U2` refusal and
/// `Failed to load 'x2.db'`, and `dbl` lists T T2 U.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn ioc_builder_runs_each_file_alias_at_its_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("x1.db"), "record(ai, \"T\")\n").expect("write");
    std::fs::write(
        dir.path().join("x2.db"),
        "alias(\"T\", \"T2\")\nalias(\"U\", \"U2\")\nrecord(ai, \"U\")\n",
    )
    .expect("write");
    let none = HashMap::new();
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = match IocBuilder::new()
        .db_file("x1.db", &none)
        .and_then(|b| b.db_file("x2.db", &none))
    {
        Ok(builder) => builder.build().await.map(|_| ()),
        Err(e) => Err(e),
    };
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"));
    assert!(built.is_err(), "the second load fails as x2.db does");
    assert_eq!(
        err,
        format!(
            "ERROR: Alias 'U2' names an unknown record 'U'\n\
             ERROR:  at or before ')' in path \"{}\"  file \"x2.db\" line 2\n\
             \n 2 | alias(\"U\", \"U2\")\n\n\
             ERROR: Failed to load 'x2.db'\n",
            dir.path().display()
        )
    );
}
