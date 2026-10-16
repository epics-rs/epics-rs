//! `Starting iocInit` comes before the database's `init_record` reports, on
//! stderr, and the `coreRelease` banner goes to stdout: C `iocBuild_1` says
//! the line (`iocInit.c:129`) and prints the banner (`:147`) before
//! `initDatabase` runs a record. softIoc R7.0.10 on this database, the two
//! streams captured apart:
//!
//! ```text
//! $ softIoc -d badsub.db </dev/null 2>err >out
//! err: Starting iocInit
//!      BADSUB.SNAM ERROR function 'noSuchSub' not found
//!      iocRun: All initialization complete
//! out: ############################################################################
//!      ## EPICS R7.0.10.1-DEV
//!      ...
//! ```

// `softioc-rs` serves through the async CA front-end and refuses at startup
// on `exec_backend`.
#![cfg(tokio_backend)]

use std::process::{Command, Stdio};

const DB: &str = "record(sub, \"BADSUB\") { field(SNAM, \"noSuchSub\") }\n";

#[test]
fn starting_ioc_init_precedes_the_init_record_report_as_in_soft_ioc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("badsub.db");
    std::fs::write(&db, DB).expect("write db");
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "-d"])
        .arg(&db)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    // `--port 0`'s port report has no C counterpart; it is written at the
    // bind, which `iocInit` now does at `dbInitServers()`.
    let first: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.starts_with("CA server: UDP search on port"))
        .take(3)
        .collect();
    assert_eq!(
        first,
        [
            "Starting iocInit",
            "BADSUB.SNAM \x1b[31;1mERROR\x1b[0m function 'noSuchSub' not found",
            "iocRun: All initialization complete",
        ],
        "{stderr}"
    );
    assert_eq!(
        stdout.lines().next(),
        Some("#".repeat(76).as_str()),
        "{stdout}"
    );
}
