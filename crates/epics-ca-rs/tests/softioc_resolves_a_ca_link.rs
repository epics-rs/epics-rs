//! softIoc links dbCa, so `INP "X CA"` resolves with nothing in the script:
//! the holder reads X over CA. softioc-rs registered no `ca` link set, so the
//! same database left H at 0 INVALID.

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn a_ca_link_to_a_served_record_reads_its_value() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("ca.db");
    std::fs::write(
        &db,
        "record(ai, \"X\") { field(VAL, \"3.5\") }\nrecord(ai, \"H\") { field(INP, \"X CA\") }\n",
    )
    .expect("write db");

    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(&db)
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

    let mut heard = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let read = loop {
        writeln!(stdin, "dbpf H.PROC 1").unwrap();
        writeln!(stdin, "dbgf H").unwrap();
        let mut got = None;
        while let Ok(line) = rx.recv_timeout(Duration::from_millis(500)) {
            if line.contains("DBF_DOUBLE") && line.contains("3.5") {
                got = Some(line.clone());
            }
            heard.push(line);
        }
        if got.is_some() || Instant::now() >= deadline {
            break got;
        }
    };
    let _ = writeln!(stdin, "exit");
    let _ = child.kill();
    let _ = child.wait();

    assert!(read.is_some(), "H never read X over CA; heard:\n{heard:#?}");
}
