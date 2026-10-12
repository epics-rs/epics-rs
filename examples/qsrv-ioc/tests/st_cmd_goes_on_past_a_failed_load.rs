//! A C st.cmd goes on past a failed `dbLoadRecords`: the next file loads,
//! the failed file's other records stay, and `iocInit` runs. Only `softIoc -d`
//! stops there (`softMain.cpp:198`). softIoc R7.0.10.1 (8f5015b66) on this
//! st.cmd prints the lines below and `dbl` lists B1, B2, B3 and G1.

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const WANT: &str = r#"ERROR: Can't set 'B2.SCAN' to 'Bogus' using menu menuScan : Illegal choice
    Did you mean "I/O Intr"?
ERROR:  at or before ')' in path "."  file "b1.db" line 2

 2 | record(ai, "B2") { field(SCAN, "Bogus") }

ERROR: Failed to load 'b1.db'
Starting iocInit
"#;

#[test]
fn a_failed_load_leaves_the_next_file_and_its_own_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
    write(
        "b1.db",
        "record(ai, \"B1\") { }\nrecord(ai, \"B2\") { field(SCAN, \"Bogus\") }\nrecord(ai, \"B3\") { }\n",
    );
    write("b2.db", "record(ai, \"G1\") { }\n");
    write(
        "st.cmd",
        "dbLoadRecords(\"b1.db\", \"\")\ndbLoadRecords(\"b2.db\", \"\")\niocInit\n",
    );
    let port = |what: &str| {
        let probe = TcpListener::bind("127.0.0.1:0").expect(what);
        probe.local_addr().expect("bound").port().to_string()
    };
    let mut child = Reaped(
        Command::new(env!("CARGO_BIN_EXE_qsrv-ioc"))
            .arg("st.cmd")
            .current_dir(dir.path())
            .env("EPICS_CAS_SERVER_PORT", port("a CA port"))
            .env("EPICS_PVAS_SERVER_PORT", port("a PVA port"))
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn qsrv-ioc"),
    );
    let mut reader = BufReader::new(child.0.stderr.take().expect("piped stderr"));
    let (mut head, mut pvs) = (String::new(), Vec::new());
    let mut in_list = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read qsrv-ioc stderr");
        assert!(n > 0, "qsrv-ioc exited; stderr so far:\n{head}");
        let plain = strip_ansi(&line);
        if plain.starts_with("qsrv-ioc: dual-protocol IOC") {
            in_list = true;
        } else if !in_list {
            // C prints this too; whether it beats the banner is a race.
            if !plain.starts_with("iocRun: ") {
                head.push_str(&plain);
            }
        } else if plain.starts_with("    ") {
            pvs.push(plain.trim().to_string());
        } else if plain.starts_with("  PVs") {
        } else if !pvs.is_empty() || plain.trim().is_empty() {
            break;
        }
        if in_list && pvs.len() >= 4 {
            break;
        }
    }
    assert_eq!(head, WANT);
    for rec in ["B1", "B2", "B3", "G1"] {
        assert!(pvs.iter().any(|p| p == rec), "{rec} in {pvs:?}");
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}
