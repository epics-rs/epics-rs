//! `oracle-ioc --db` on a load that fails ends as `softIoc -d` does: exit 2
//! and nothing after `dbLoadRecords`' own lines. softIoc R7.0.10.1
//! (8f5015b66) `-d` on each file, stderr captured from the file's directory:
//!
//! ```text
//! nope.db  rc=2  ERROR: Can't open file 'nope.db'
//!                ERROR: Failed to load 'nope.db'
//! bad.db   rc=2  ERROR: Can't set 'B2.SCAN' to 'Bogus' using menu menuScan : Illegal choice
//!                ERROR:  at or before ')' in path "."  file "bad.db" line 2
//!                (blank, the source line, blank)
//!                ERROR: Failed to load 'bad.db'
//!                    Did you mean "I/O Intr"?
//! ```
//!
//! The suggestion is C's errlog thread, so where it lands among the
//! direct lines is timing; it is asserted present, not placed.

#![cfg(tokio_backend)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SUGGESTION: &str = "    Did you mean \"I/O Intr\"?";

/// A directory of the test's own, so `in path "."` names it.
fn scratch(case: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oracle-dload-{case}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run `oracle-ioc --db <name>` in `dir`; its exit code and stderr.
fn load(dir: &std::path::Path, name: &str) -> (Option<i32>, String) {
    let claim = epics_oracle_rs::ioc::claim_port().expect("claim a port");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oracle-ioc"))
        .arg("--db")
        .arg(name)
        .arg("--port")
        .arg(claim.port().to_string())
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oracle-ioc");
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().expect("poll oracle-ioc").is_none() {
        if Instant::now() > deadline {
            child.kill().expect("kill oracle-ioc");
            child.wait().expect("reap oracle-ioc");
            panic!("oracle-ioc --db {name} booted instead of exiting");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().expect("oracle-ioc stderr");
    let err = String::from_utf8(out.stderr)
        .expect("utf-8 stderr")
        .replace("\x1b[31;1mERROR\x1b[0m", "ERROR");
    (out.status.code(), err)
}

#[test]
fn a_missing_file_exits_2_after_dbloadrecords_lines() {
    let dir = scratch("nope");
    let (code, err) = load(&dir, "nope.db");
    std::fs::remove_dir_all(&dir).expect("remove scratch");
    assert_eq!(code, Some(2));
    assert_eq!(
        err,
        "ERROR: Can't open file 'nope.db'\nERROR: Failed to load 'nope.db'\n"
    );
}

#[test]
fn a_refused_field_exits_2_after_dbloadrecords_lines() {
    let dir = scratch("bad");
    std::fs::write(
        dir.join("bad.db"),
        "record(ai, \"B1\") { }\nrecord(ai, \"B2\") { field(SCAN, \"Bogus\") }\nrecord(ai, \"B3\") { }\n",
    )
    .expect("write bad.db");
    let (code, err) = load(&dir, "bad.db");
    std::fs::remove_dir_all(&dir).expect("remove scratch");
    assert_eq!(code, Some(2));
    let mut lines: Vec<&str> = err.lines().collect();
    let at = lines.iter().position(|l| *l == SUGGESTION);
    assert!(at.is_some(), "{err}");
    lines.remove(at.unwrap());
    assert_eq!(
        lines,
        [
            "ERROR: Can't set 'B2.SCAN' to 'Bogus' using menu menuScan : Illegal choice",
            "ERROR:  at or before ')' in path \".\"  file \"bad.db\" line 2",
            "",
            " 2 | record(ai, \"B2\") { field(SCAN, \"Bogus\") }",
            "",
            "ERROR: Failed to load 'bad.db'",
        ]
    );
}
