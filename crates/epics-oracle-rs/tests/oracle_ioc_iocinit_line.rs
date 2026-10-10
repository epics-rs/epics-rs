//! `oracle-ioc` says `Starting iocInit` where `oracleIoc` does: on stderr,
//! before the database's `init_record` reports, with the `coreRelease`
//! banner on stdout (C `iocBuild_1`, `iocInit.c:129` and `:147`).
//! `oracleIoc st.cmd` with `DB=badsub.db`, the two streams captured apart:
//!
//! ```text
//! err: Starting iocInit
//!      BADSUB.SNAM ERROR function 'noSuchSub' not found
//!      iocRun: All initialization complete
//! out: (the st.cmd echo, then)
//!      ############################################################################
//!      ## EPICS R7.0.10.1-DEV
//!      ...
//! ```

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

const DB: &str = "record(sub, \"BADSUB\") { field(SNAM, \"noSuchSub\") }\n";

#[test]
fn starting_ioc_init_precedes_the_init_record_report_as_in_oracle_ioc() {
    let db = std::env::temp_dir().join(format!("oracle-badsub-{}.db", std::process::id()));
    std::fs::write(&db, DB).expect("write db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oracle-ioc"))
        .arg("--db")
        .arg(&db)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oracle-ioc");
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let stdout = BufReader::new(child.stdout.take().unwrap());
    // The process serves until killed; each stream is read up to the line
    // that ends what the case is about.
    let mut err = Vec::new();
    for line in stderr.lines().map_while(Result::ok) {
        let ready = line.starts_with("iocRun:");
        err.push(line);
        if ready {
            break;
        }
    }
    let mut out = Vec::new();
    for line in stdout.lines().map_while(Result::ok) {
        let ready = line == "ORACLE_IOC_READY";
        out.push(line);
        if ready {
            break;
        }
    }
    child.kill().expect("kill oracle-ioc");
    child.wait().expect("reap oracle-ioc");
    std::fs::remove_file(&db).expect("remove db");

    assert_eq!(
        err,
        [
            "Starting iocInit",
            "BADSUB.SNAM \x1b[31;1mERROR\x1b[0m function 'noSuchSub' not found",
            "iocRun: All initialization complete",
        ]
    );
    assert_eq!(
        out.first().map(String::as_str),
        Some("#".repeat(76).as_str()),
        "{out:?}"
    );
}
