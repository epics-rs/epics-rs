//! The PINI passes run where softIoc runs them: YES inside `iocBuild`
//! (`initialProcess`, `iocInit.c:194-195`), and `piniProcessHook`'s
//! (`:630-651`) RUN and RUNNING either side of every `iocRun` line and
//! PAUSE and PAUSED either side of every `iocPause` line; the CP holder DST
//! processes at `dbCaRun` (`:260`), before the first `iocRun` line.
//! softIoc R7.0.10 with stdout unbuffered and merged with stderr:
//!
//! ```text
//! $ printf 'iocPause\niocRun\n' | stdbuf -o0 softIoc -d pini.db 2>&1
//! Starting iocInit
//! ############################################################################
//! ...
//! _main_: dbProcess of 'PY'
//! _main_: dbProcess of 'PR'
//! dbCaLink: dbProcess of 'DST'
//! iocRun: All initialization complete
//! _main_: dbProcess of 'PG'
//! epics> _main_: dbProcess of 'PP'
//! iocPause: IOC suspended
//! _main_: dbProcess of 'PD'
//! epics> _main_: dbProcess of 'PR'
//! iocRun: IOC restarted
//! _main_: dbProcess of 'PG'
//! ```
//!
//! The whole merged output past the banner is compared, so a line C does not
//! print fails the case. One difference is named rather than tolerated: the
//! port's `CA server:` line, which C's RSRV does not print, is required
//! exactly once wherever the server's start lands it.

// `softioc-rs` serves through the async CA front-end and refuses at startup
// on `exec_backend`.
#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};

const DB: &str = "\
record(calc, \"PY\") { field(PINI, \"YES\") field(TPRO, \"1\") field(CALC, \"1\") }
record(calc, \"PR\") { field(PINI, \"RUN\") field(TPRO, \"1\") field(CALC, \"2\") }
record(calc, \"PG\") { field(PINI, \"RUNNING\") field(TPRO, \"1\") field(CALC, \"3\") }
record(calc, \"PP\") { field(PINI, \"PAUSE\") field(TPRO, \"1\") field(CALC, \"4\") }
record(calc, \"PD\") { field(PINI, \"PAUSED\") field(TPRO, \"1\") field(CALC, \"5\") }
record(ao, \"SRC\") {}
record(calc, \"DST\") { field(INPA, \"SRC CP\") field(TPRO, \"1\") field(CALC, \"A\") }
";

/// A `dbProcess` line as its record's name and any other line as written,
/// past the shell's prompt; the banner, elided above, is dropped.
fn normalized(line: &str) -> Option<&str> {
    if line.starts_with('#') {
        return None;
    }
    let line = line.rsplit_once("epics> ").map_or(line, |(_, rest)| rest);
    let line = line.trim_start_matches("\u{1b}[0m");
    let line = line
        .split_once(": dbProcess of '")
        .and_then(|(_, rest)| rest.strip_suffix('\''))
        .unwrap_or(line);
    (!line.is_empty()).then_some(line)
}

#[test]
fn pini_passes_bracket_every_run_and_pause_line_as_in_soft_ioc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("pini.db");
    std::fs::write(&db, DB).expect("write db");
    // The TPRO line is on stdout and the rest on stderr; one pipe takes both
    // in the order they are written, as `2>&1` does.
    let (mut output, writer) = std::io::pipe().expect("pipe");
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "-d"])
        .arg(&db)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(writer.try_clone().expect("clone the pipe"))
        .stderr(writer)
        .spawn()
        .expect("spawn softioc-rs");
    // Written whole and closed: the shell runs both lines, then exits on EOF.
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"iocPause\niocRun\n")
        .expect("write the shell lines");
    let mut stderr = String::new();
    output.read_to_string(&mut stderr).expect("read the output");
    child.wait().expect("reap softioc-rs");

    let (server, seen): (Vec<&str>, Vec<&str>) = stderr
        .lines()
        .filter_map(normalized)
        .partition(|line| line.starts_with("CA server: UDP search on port "));
    assert_eq!(server.len(), 1, "{stderr}");
    assert_eq!(
        seen,
        [
            "Starting iocInit",
            "PY",
            "PR",
            "DST",
            "iocRun: All initialization complete",
            "PG",
            "PP",
            "iocPause: IOC suspended",
            "PD",
            "PR",
            "iocRun: IOC restarted",
            "PG",
        ],
        "{stderr}"
    );
}
