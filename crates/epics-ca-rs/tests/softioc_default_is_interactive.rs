//! C softIoc starts a shell unless told not to, and that decides how the
//! process ENDS.
//!
//! `softMain.cpp:243-271`: `interactive` is true from `:137` and only `-S`
//! clears it (`:202-203`). An interactive softIoc runs `iocsh(NULL)`, which
//! reads stdin and returns at EOF, so `softIoc st.cmd < /dev/null` exits 0
//! the moment the script is done. A non-interactive one with nothing loaded
//! and no script never gets that far: it prints its usage and `Nothing to
//! do!` and exits 1.
//!
//! This port defaulted to the NON-interactive arm, so the same command
//! served forever and every harness that ran it hit its own timeout. Both
//! arms are measured against `softIoc` R7.0.10 here.

// Every case here drives the `softioc-rs` binary as a subprocess, and that
// binary serves through the async CA front-end, so on `exec_backend` it
// refuses at startup instead of running. `realtime-ca-ioc` is the entry
// point that brings a CA IOC up on that backend, through the blocking
// thread-per-client driver; it is a different binary with a different
// command line, so these cases follow `softioc-rs` rather than move.
#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Wait for `child` to exit, killing it if it outlives the budget.
///
/// Returns `None` on the kill, which is the assertion that matters: an IOC
/// that is still serving has NOT taken C's interactive exit.
fn wait_for_exit(mut child: std::process::Child, budget: Duration) -> Option<i32> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait().expect("poll the IOC") {
            Some(status) => return status.code(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// C `softMain.cpp:247-252`: the script has run, the shell is the last
/// thing left, and stdin is already at EOF — so `iocsh(NULL)` returns 0 and
/// the process is done. Measured: `softIoc st.cmd < /dev/null` exits 0.
#[test]
fn a_script_and_a_closed_stdin_end_the_process_as_c_does() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("eof.db");
    std::fs::write(&db, "record(ai, \"EOF:PV\") { field(VAL, \"1\") }\n").expect("write db");
    let script = dir.path().join("st.cmd");
    let mut line = String::from("dbLoadRecords(\"");
    line.push_str(db.to_str().expect("utf-8 path"));
    line.push_str("\")\n");
    std::fs::write(&script, &line).expect("write script");

    let child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0"])
        .arg(&script)
        // Off the network: this test is about the exit, not the server.
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn softioc-rs");

    assert_eq!(
        wait_for_exit(child, Duration::from_secs(30)),
        Some(0),
        "an interactive softIoc at stdin EOF exits 0; still running means -S is still the default"
    );
}

/// The other side of the same default: `-S` is what makes the IOC serve
/// past EOF (C `:258-261` spins forever once anything was loaded), so the
/// harnesses that want a server must ask for it.
///
/// The proof is the "CA server:" line, not a stopwatch: reaching it means
/// the process passed the point where the interactive arm would already
/// have read EOF and returned, and it is still running there. A bare sleep
/// would pass on a loaded host for the wrong reason — an IOC too slow to
/// boot is also an IOC that has not exited.
#[test]
fn dash_s_keeps_serving_after_stdin_is_closed() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["-S", "--port", "0", "--pv", "EOF:SERVED:double:1.0"])
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");

    let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr"));
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut serving = false;
    while Instant::now() < deadline {
        let mut line = String::new();
        if stderr.read_line(&mut line).expect("read the IOC's stderr") == 0 {
            break;
        }
        if line.starts_with("CA server:") {
            serving = true;
            break;
        }
    }
    let alive = child.try_wait().expect("poll the IOC").is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(serving, "the -S IOC never announced its CA server");
    assert!(alive, "-S must serve past EOF, not exit with the shell");
}

/// C `softMain.cpp:262-271`: a NON-interactive softIoc that loaded nothing
/// and ran no script prints its usage on stdout, `Nothing to do!` on
/// stderr, and exits 1. Byte-compared against `softIoc -S` at R7.0.10 for
/// the stderr line and the status; the usage block above it is clap's,
/// which lists this binary's own long options.
#[test]
fn nothing_to_do_is_c_s_refusal_and_c_s_status() {
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-S")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs -S");

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "Nothing to do!\n");
    assert!(
        !out.stdout.is_empty(),
        "C prints the usage block on stdout before refusing"
    );
}

/// An interactive IOC reads its shell from stdin, so the same binary that
/// exits at EOF must still RUN what stdin holds. Measured against
/// `softIoc -d good.db` fed `dbl\nexit\n`, which lists the record and
/// exits 0.
#[test]
fn stdin_is_the_shell_when_no_dash_s_is_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("typed.db");
    std::fs::write(&db, "record(ai, \"EOF:TYPED\") { field(VAL, \"1\") }\n").expect("write db");

    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "-d"])
        .arg(&db)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn softioc-rs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"dbl\nexit\n")
        .expect("type into the shell");

    let out = child.wait_with_output().expect("collect the shell output");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("EOF:TYPED"),
        "the shell must have run `dbl`, got {stdout:?}"
    );
}
