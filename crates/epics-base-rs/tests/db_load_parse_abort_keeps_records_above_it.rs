//! C installs each record as `yyparse` reduces it, so a syntax error ends the
//! load with every record above it in the database, the one it interrupted
//! included, and is printed after their own diagnostics
//! (`dbLexRoutines.c:301-306`). softIoc R7.0.10.1 (8f5015b66) on
//! `dbLoadRecords("f1.db")` then `dbLoadRecords("f3.db")` lists F1A, F1B,
//! F3A and F3B, reads `F3B.DESC` "b", and prints the lines pinned below.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

const F1: &str = "record(ai, \"F1A\") {}\nrecord(ai, \"F1B\") { field(DESC, \"x\") }\n";
const F3: &str = "record(ai, \"F3A\") { field(SCAN, \"Bogus\") }
record(ai, \"F3B\") {
    field(DESC, \"b\")
    field(EGU \"zz\")
    field(PREC, \"2\")
}
record(ai, \"F3C\") {}
";

/// softIoc's stderr for the load, ANSI stripped, `in path` aside.
fn want(path: &str) -> String {
    format!(
        "\
ERROR: Can't set 'F3A.SCAN' to 'Bogus' using menu menuScan : Illegal choice
    Did you mean \"I/O Intr\"?
ERROR:  at or before ')' in path \"{path}\"  file \"f3.db\" line 1

 1 | record(ai, \"F3A\") {{ field(SCAN, \"Bogus\") }}

ERROR: syntax error

 4 |     field(EGU \"zz\")

WARNING: dbReadCOM: Parser stack dirty w/o error. 1
ERROR: Failed to load 'f3.db'
"
    )
}

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

/// fd 2 redirected into a file until [`Stderr::finish`].
struct Stderr {
    sink: tempfile::NamedTempFile,
    saved: i32,
}

impl Stderr {
    fn capture() -> Self {
        let sink = tempfile::NamedTempFile::new().expect("stderr capture");
        // SAFETY: fd 2 is process-global, which is why these tests are `serial`.
        let saved = unsafe {
            let saved = libc::dup(2);
            libc::dup2(sink.as_file().as_raw_fd(), 2);
            saved
        };
        Self { sink, saved }
    }

    fn finish(self) -> String {
        // SAFETY: restoring the descriptor `capture` replaced.
        unsafe {
            libc::dup2(self.saved, 2);
            libc::close(self.saved);
        }
        strip_ansi(&std::fs::read_to_string(self.sink.path()).expect("stderr capture"))
    }
}

/// The two files, on `EPICS_DB_INCLUDE_PATH` until the guard drops.
struct Files(tempfile::TempDir);

impl Files {
    fn on_path() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f1.db"), F1).expect("write f1.db");
        std::fs::write(dir.path().join("f3.db"), F3).expect("write f3.db");
        // SAFETY: the process-global env is why these tests are `serial`.
        unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path()) };
        Self(dir)
    }

    fn path(&self) -> String {
        self.0.path().display().to_string()
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        // SAFETY: as in `on_path`.
        unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    }
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dbloadrecords_keeps_the_records_above_a_syntax_error() {
    let files = Files::on_path();
    let db = Arc::new(PvDatabase::new());
    let shell_db = Arc::clone(&db);
    let err = Stderr::capture();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    std::thread::spawn(move || {
        let shell = IocShell::new(shell_db, bridge);
        let _ = shell.execute_line("dbLoadRecords(\"f1.db\")");
        let _ = shell.execute_line("dbLoadRecords(\"f3.db\")");
    })
    .join()
    .expect("the shell thread");
    assert_eq!(err.finish(), want(&files.path()));
    for name in ["F1A", "F1B", "F3A", "F3B"] {
        assert!(db.get_record(name).is_some(), "{name} is loaded");
    }
    assert!(db.get_record("F3C").is_none(), "F3C is past the abort");
    let f3b = db.get_record("F3B").expect("F3B");
    assert_eq!(f3b.read().resolve_field("DESC").unwrap().to_string(), "b");
}

/// `softIoc -d f1.db -d f3.db` prints the same lines and exits 2; the builder
/// prints them when `build` installs f3.db, not when `db_file` reads it.
/// softIoc's `Did you mean` has no fixed place on that route: `-d` runs on
/// `_main_`, which is not OkToBlock (osdThread.c:406), so `epicsPrintf` queues
/// it for the errlog thread (errlog.c:186) and it races the parser's own
/// stderr, so it is left out.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn the_builder_reports_the_abort_when_it_installs_the_load() {
    let files = Files::on_path();
    let read = Stderr::capture();
    let builder = IocBuilder::new()
        .db_file("f1.db", &HashMap::new())
        .and_then(|b| b.db_file("f3.db", &HashMap::new()));
    assert_eq!(read.finish(), "", "db_file only reads");
    let installed = Stderr::capture();
    let built = builder.expect("both files are found").build().await;
    let err = installed.finish();
    let Err(failed) = built else {
        panic!("f3.db fails the build");
    };
    assert_eq!(failed.to_string(), "Failed to load 'f3.db'");
    let without_suggestion = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("    Did you mean"))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
    };
    assert_eq!(
        without_suggestion(&err),
        without_suggestion(&want(&files.path()))
    );
}
