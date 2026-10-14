//! `oracle-ioc` runs the three PINI passes where `oracleIoc` does: YES inside
//! `iocBuild` (`initialProcess`, `iocInit.c:194-195`), RUN at
//! `initHookAtIocRun` and RUNNING at `initHookAfterIocRunning`, either side
//! of the `iocRun` line; the CP holder DST processes at `dbCaRun`
//! (`iocInit.c:260`), before the line. `oracleIoc st.cmd` with
//! `DB=pini.db`, stdout unbuffered (`stdbuf -o0`) and merged with stderr:
//!
//! ```text
//! Starting iocInit
//! ############################################################################
//! ...
//! _main_: dbProcess of 'PY'
//! _main_: dbProcess of 'PR'
//! dbCaLink: dbProcess of 'DST'
//! iocRun: All initialization complete
//! _main_: dbProcess of 'PG'
//! ```
//!
//! The whole merged output past the banner, up to the port's `CA server:`
//! line, which C's RSRV does not print and `CaServer::run` writes after the
//! ready line, is compared, so a line C does not print fails the case.
//!
//! `oracle-ioc` has no shell, so its `iocPause`/`iocRun` cycle is pinned on
//! the same `build_ioc` path in-process, by `epics-base-rs`'s
//! `build_ioc_pini_hook`.

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

const DB: &str = "\
record(calc, \"PY\") { field(PINI, \"YES\") field(TPRO, \"1\") field(CALC, \"1\") }
record(calc, \"PR\") { field(PINI, \"RUN\") field(TPRO, \"1\") field(CALC, \"2\") }
record(calc, \"PG\") { field(PINI, \"RUNNING\") field(TPRO, \"1\") field(CALC, \"3\") }
record(ao, \"SRC\") {}
record(calc, \"DST\") { field(INPA, \"SRC CP\") field(TPRO, \"1\") field(CALC, \"A\") }
";

/// A `dbProcess` line as its record's name; every other line as written.
fn normalized(line: &str) -> &str {
    line.split_once(": dbProcess of '")
        .and_then(|(_, rest)| rest.strip_suffix('\''))
        .unwrap_or(line)
}

#[test]
fn pini_passes_bracket_the_ioc_run_line_as_in_oracle_ioc() {
    let db = std::env::temp_dir().join(format!("oracle-pini-{}.db", std::process::id()));
    std::fs::write(&db, DB).expect("write db");
    // The TPRO line is on stdout and the rest on stderr; one pipe takes both
    // in the order they are written, as `2>&1` does.
    let claim = epics_oracle_rs::ioc::claim_port().expect("claim a port");
    let (output, writer) = std::io::pipe().expect("pipe");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oracle-ioc"))
        .arg("--db")
        .arg(&db)
        .arg("--port")
        .arg(claim.port().to_string())
        .stdin(Stdio::null())
        .stdout(writer.try_clone().expect("clone the pipe"))
        .stderr(writer)
        .spawn()
        .expect("spawn oracle-ioc");
    // The process serves until killed, so the stream is read up to the ready
    // marker, which follows every PINI pass and the socket binds.
    let mut stderr = String::new();
    let mut seen = Vec::new();
    for line in BufReader::new(output).lines().map_while(Result::ok) {
        stderr.push_str(&line);
        stderr.push('\n');
        if line == "ORACLE_IOC_READY" {
            break;
        }
        // The banner, elided above, and the harness's port line, which
        // oracleIoc does not print.
        if line.starts_with('#') || line.starts_with("ORACLE_IOC_") {
            continue;
        }
        seen.push(normalized(&line).to_string());
    }
    child.kill().expect("kill oracle-ioc");
    child.wait().expect("reap oracle-ioc");
    std::fs::remove_file(&db).expect("remove db");

    assert_eq!(
        seen,
        [
            "Starting iocInit",
            "PY",
            "PR",
            "DST",
            "iocRun: All initialization complete",
            "PG",
        ],
        "{stderr}"
    );
}
