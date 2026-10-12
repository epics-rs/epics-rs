//! A scan thread is not ok to block, so when the errlog console stalls its
//! messages are dropped and counted rather than waited on (`msgbufAlloc`,
//! `errlog.c:132-142`), and the worker reports the count once it gets going
//! again (`errlog.c:826-827`).
//!
//! The database writes 1000 `@errlog` stdio lines in one scan pass. The
//! console is a one-page pipe that is not read while the pass runs. Measured
//! on softIoc R7.0.10-146-g8f5015b66, 2560/512, over five runs:
//!
//! ```text
//! printed=355 lost=645    printed=434 lost=566    printed=353 lost=647
//! printed=380 lost=620    printed=456 lost=544
//! errlog: lost 645 messages
//! ```
//!
//! The split moves with the worker's timing, in C as here. Two things do not
//! move: every line is either printed or counted, and something is lost.
#![cfg(all(tokio_backend, target_os = "linux"))]

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BURST: usize = 1000;

/// `ONCE` runs first in the 1 second list and turns the stdio records off
/// from the second pass on, so exactly one pass writes.
fn once_db() -> String {
    let mut db = String::from(
        "record(calc, \"ONCE\") { field(SCAN, \"1 second\") field(PHAS, \"-1\") \
         field(CALC, \"A:=A+1;A>1\") }\n",
    );
    for i in 0..BURST {
        db.push_str(&format!(
            "record(stringout, \"E{i:04}\") {{ field(DTYP, \"stdio\") field(OUT, \"@errlog\") \
             field(VAL, \"burst line {i:04}\") field(SCAN, \"1 second\") \
             field(SDIS, \"ONCE\") field(DISV, \"1\") }}\n"
        ));
    }
    db
}

use named_port::verdict as softioc_verdict;

/// `(burst lines printed, sum of every lost count, lost-line count)`.
fn tally(text: &str) -> (usize, usize, usize) {
    let printed = text.matches("burst line ").count();
    let mut lost = 0;
    let mut lines = 0;
    for line in text.lines() {
        if let Some(n) = line
            .strip_prefix("errlog: lost ")
            .and_then(|rest| rest.strip_suffix(" messages"))
        {
            lost += n.parse::<usize>().expect("C's %zu");
            lines += 1;
        }
    }
    (printed, lost, lines)
}

/// softioc-rs on `path` and `port` with a one-page console nobody reads while
/// the pass runs: everything it printed, or `None` when it reports `port`
/// taken.
fn console_of_a_stalled_pass(path: &std::path::Path, port: u16) -> Option<String> {
    let (mut out, writer) = std::io::pipe().expect("pipe");
    // One page, so the worker blocks on the console long before the pass ends.
    // SAFETY: F_SETPIPE_SZ on a pipe this test owns.
    assert!(unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) } >= 4096);
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(path)
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(writer.try_clone().expect("dup pipe"))
        .stderr(writer)
        .spawn()
        .expect("spawn softioc-rs");

    let mut seen = Vec::new();
    let mut chunk = [0u8; 65536];
    while !String::from_utf8_lossy(&seen).contains("iocRun") {
        let n = out.read(&mut chunk).expect("read console");
        assert!(n > 0, "softioc-rs exited before iocRun");
        seen.extend_from_slice(&chunk[..n]);
    }
    // The pass runs on the first tick; nothing reads the console meanwhile.
    std::thread::sleep(Duration::from_secs(3));

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        loop {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let (printed, lost, _) = tally(&String::from_utf8_lossy(&seen));
        if printed + lost >= BURST {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(bytes) => seen.extend_from_slice(&bytes),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // The console closed: the IOC is gone.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let text = String::from_utf8_lossy(&seen).into_owned();
    (!softioc_verdict::rs_ioc_port_was_taken(&text, port)).then_some(text)
}

#[test]
fn a_stalled_console_drops_and_counts_what_a_scan_thread_posts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("once.db");
    std::fs::write(&path, once_db()).expect("write db");
    let text = named_port::on_a_named_port(|port| console_of_a_stalled_pass(&path, port));

    let (printed, lost, lost_lines) = tally(&text);
    assert_eq!(
        printed + lost,
        BURST,
        "every line printed or counted:\n{text}"
    );
    assert!(
        lost > 0 && lost_lines > 0,
        "a scan thread waited for the stalled console instead of dropping: \
         printed={printed} lost={lost}"
    );
}
