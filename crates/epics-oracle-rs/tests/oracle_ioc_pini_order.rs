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
//! The whole stderr up to the port's `CA server:` line, which C's RSRV does
//! not print and `CaServer::run` writes after the ready line, is compared, so
//! a line C does not print fails the case. The TPRO line's text is not C's
//! yet (and C writes it to stdout), so it compares as the record name.
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

/// A TPRO line as its record's name; every other line as written.
fn normalized(line: &str) -> &str {
    line.strip_prefix("[TPRO] ")
        .and_then(|rest| rest.split_once(": process "))
        .map_or(line, |(name, _)| name)
}

#[test]
fn pini_passes_bracket_the_ioc_run_line_as_in_oracle_ioc() {
    let db = std::env::temp_dir().join(format!("oracle-pini-{}.db", std::process::id()));
    std::fs::write(&db, DB).expect("write db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oracle-ioc"))
        .arg("--db")
        .arg(&db)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oracle-ioc");
    // The process serves until killed, so the stream is read up to the server
    // line, which follows every PINI pass.
    let mut stderr = String::new();
    let mut seen = Vec::new();
    for line in BufReader::new(child.stderr.take().expect("piped stderr"))
        .lines()
        .map_while(Result::ok)
    {
        stderr.push_str(&line);
        stderr.push('\n');
        if line.starts_with("CA server: UDP search on port ") {
            break;
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
