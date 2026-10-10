//! softIoc links dbCa, so `INP "X CA"` resolves with nothing in the script:
//! the holder reads X over CA. softioc-rs registered no `ca` link set, so the
//! same database left H at 0 INVALID.

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A port number nothing on the box holds for UDP or for TCP. The two are
/// separate spaces, and the server needs both: a number free only for TCP can
/// already be another test's UDP socket, and softioc-rs then fails its search
/// responder bind with `EADDRINUSE` and exits (C's `tryBind` gives up on that
/// interface the same way).
fn port_free_for_udp_and_tcp() -> u16 {
    loop {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
}

/// What one softioc-rs on `db` printed, and the `dbgf H` line that showed
/// 3.5 if any did. Ready is `iocRun: All initialization complete`, which C
/// and the port both print; the port binds its CA server after it, so a bind
/// lost to another process shows up as the child exiting while H is polled.
struct Attempt {
    read: Option<String>,
    lost_bind: bool,
    heard: Vec<String>,
}

fn read_h_over_ca(db: &std::path::Path) -> Attempt {
    let port = port_free_for_udp_and_tcp().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(db)
        .env("EPICS_CA_SERVER_PORT", &port)
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");
    let mut stdin = child.stdin.take().unwrap();
    let (tx, rx) = mpsc::channel();
    for out in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);

    let mut heard = Vec::new();
    let mut read = None;
    // Both pipes close when the child exits, which ends either wait.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        ready = line.contains("iocRun: All initialization complete");
        heard.push(line);
        if ready {
            break;
        }
    }
    let mut gone = !ready;
    while !gone && read.is_none() && Instant::now() < deadline {
        if writeln!(stdin, "dbpf H.PROC 1").is_err() || writeln!(stdin, "dbgf H").is_err() {
            break;
        }
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => {
                    if line.contains("DBF_DOUBLE") && line.contains("3.5") {
                        read = Some(line.clone());
                    }
                    heard.push(line);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    gone = true;
                    break;
                }
            }
        }
    }
    let _ = writeln!(stdin, "exit");
    let _ = child.kill();
    let _ = child.wait();
    let lost_bind = heard.iter().any(|l| l.contains("Address already in use"));
    Attempt {
        read,
        lost_bind,
        heard,
    }
}

#[test]
fn a_ca_link_to_a_served_record_reads_its_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("ca.db");
    std::fs::write(
        &db,
        "record(ai, \"X\") { field(VAL, \"3.5\") }\nrecord(ai, \"H\") { field(INP, \"X CA\") }\n",
    )
    .expect("write db");

    // No port can be reserved across the spawn, so a lost bind is answered
    // with a fresh port.
    for _ in 0..5 {
        let attempt = read_h_over_ca(&db);
        if attempt.lost_bind && attempt.read.is_none() {
            continue;
        }
        let heard = attempt.heard;
        assert!(
            attempt.read.is_some(),
            "H never read X over CA; heard:\n{heard:#?}"
        );
        return;
    }
    panic!("softioc-rs lost its port bind five times");
}
