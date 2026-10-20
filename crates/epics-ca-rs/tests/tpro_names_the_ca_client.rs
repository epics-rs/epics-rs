//! TPRO's context names the CA client a put came from: C `dbProcess` asks
//! `dbServerClient` (`dbServer.c:114-128`), whose rsrv layer answers
//! `casClientInitiatingCurrentThread` (`camsgtask.c:164-179`), and falls back
//! to the thread's name, into a 40-byte buffer (`dbAccess.c:490`,
//! `:527-534`). softIoc on the records below, `dbpf X.PROC 1` from the shell,
//! then raw CA clients named `u`@`h` putting `X.PROC` and put-callbacking
//! `D.PROC`, and two whose names run past the buffer:
//!
//! ```text
//! _main_: dbProcess of 'X'
//! _main_: dbProcess of 'Y'
//! ca:u@h: dbProcess of 'X'
//! ca:u@h: dbProcess of 'Y'
//! ca:u@h: dbProcess of 'D'
//! ca:a-long-user-name-that-runs-past-fort: dbProcess of 'X'
//! ca:a-long-user-name-that-runs-past-fort: dbProcess of 'Y'
//! ca:u@a.host.name.that.runs.past.forty.b: dbProcess of 'X'
//! ca:u@a.host.name.that.runs.past.forty.b: dbProcess of 'Y'
//! ```

#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO, CA_PROTO_HOST_NAME,
    CA_PROTO_VERSION, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY, CaHeader,
};
use epics_ca_rs::server::CaServerBuilder;

const CHILD: &str = "TPRO_CA_CLIENT_CHILD";

const DB: &str = r#"
record(calc, "X") { field(TPRO, "1") field(CALC, "VAL+1") field(FLNK, "Y") }
record(calc, "Y") { field(CALC, "VAL+1") }
record(calcout, "D") { field(TPRO, "1") field(ODLY, "0.5") }
"#;

const DBR_DOUBLE: u16 = 6;

fn hdr(cmmd: u16, size: u16, dt: u16, count: u16, p1: u32, p2: u32) -> Vec<u8> {
    [cmmd, size, dt, count]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .chain([p1, p2].iter().flat_map(|v| v.to_be_bytes()))
        .collect()
}

fn padded(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b.resize(b.len().next_multiple_of(8), 0);
    b
}

struct Client {
    sock: TcpStream,
    acc: Vec<u8>,
}

impl Client {
    fn connect(port: u16, user: &str, host: &str) -> Self {
        let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = Self {
            sock,
            acc: Vec::new(),
        };
        let (user, host) = (padded(user), padded(host));
        let mut out = hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0);
        out.extend(hdr(CA_PROTO_CLIENT_NAME, user.len() as u16, 0, 0, 0, 0));
        out.extend(user);
        out.extend(hdr(CA_PROTO_HOST_NAME, host.len() as u16, 0, 0, 0, 0));
        out.extend(host);
        c.sock.write_all(&out).unwrap();
        c
    }

    /// The first `cmd` frame from the server, those before it dropped.
    fn until(&mut self, cmd: u16) -> Vec<u8> {
        loop {
            if let Ok((h, size)) = CaHeader::from_bytes_extended(&self.acc) {
                let end = size + h.actual_postsize();
                if self.acc.len() >= end {
                    let frame: Vec<u8> = self.acc.drain(..end).collect();
                    if u16::from_be_bytes([frame[0], frame[1]]) == cmd {
                        return frame;
                    }
                    continue;
                }
            }
            let mut buf = [0u8; 4096];
            let n = self.sock.read(&mut buf).expect("a frame from the server");
            assert_ne!(n, 0, "the server closed the circuit");
            self.acc.extend_from_slice(&buf[..n]);
        }
    }

    /// Put 1.0 to `pv` on channel `cid` with `cmd` and wait for it: a
    /// WRITE_NOTIFY for its reply, a WRITE for the ECHO behind it.
    fn put(&mut self, cid: u32, cmd: u16, pv: &str) {
        let name = padded(pv);
        let mut f = hdr(CA_PROTO_CREATE_CHAN, name.len() as u16, 0, 0, cid, 13);
        f.extend(name);
        self.sock.write_all(&f).unwrap();
        let created = self.until(CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes(created[12..16].try_into().unwrap());
        let mut f = hdr(cmd, 8, DBR_DOUBLE, 1, sid, 7);
        f.extend(1.0f64.to_be_bytes());
        let reply = if cmd == CA_PROTO_WRITE {
            f.extend(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
            CA_PROTO_ECHO
        } else {
            CA_PROTO_WRITE_NOTIFY
        };
        self.sock.write_all(&f).unwrap();
        self.until(reply);
    }
}

/// This test run again in a child whose stdout is not captured, and the
/// `dbProcess` lines it printed.
fn trace_of(test: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| l.contains(": dbProcess"))
        .map(|l| {
            l.trim_start_matches(&format!("test {test} ... "))
                .to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ca_put_traces_with_the_client_s_name() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("a_ca_put_traces_with_the_client_s_name");
        assert_eq!(
            trace,
            [
                "_main_: dbProcess of 'X'",
                "_main_: dbProcess of 'Y'",
                "ca:u@h: dbProcess of 'X'",
                "ca:u@h: dbProcess of 'Y'",
                "ca:u@h: dbProcess of 'D'",
                "ca:a-long-user-name-that-runs-past-fort: dbProcess of 'X'",
                "ca:a-long-user-name-that-runs-past-fort: dbProcess of 'Y'",
                "ca:u@a.host.name.that.runs.past.forty.b: dbProcess of 'X'",
                "ca:u@a.host.name.that.runs.past.forty.b: dbProcess of 'Y'",
            ]
        );
        return;
    }
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let acf = epics_base_rs::server::access_security::new_acf_cell_watching(None, &db);
    let server = CaServerBuilder::serving(db.clone(), acf, None, None)
        .port(0)
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    // What the IOC head does for rsrv before it serves.
    epics_ca_rs::server::iocsh::register_ca_db_server();
    epics_base_rs::server::db_server::db_run_servers();
    tokio::spawn(async move { server.run().await });

    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = db.clone();
    epics_base_rs::runtime::task::run_on_main_thread(move || {
        epics_base_rs::server::iocsh::IocShell::new(shell_db, bridge).execute_line("dbpf X.PROC 1")
    })
    .unwrap()
    .await
    .unwrap()
    .unwrap();

    tokio::task::spawn_blocking(move || {
        let mut c = Client::connect(port, "u", "h");
        c.put(1, CA_PROTO_WRITE, "X.PROC");
        c.put(2, CA_PROTO_WRITE_NOTIFY, "D.PROC");
        Client::connect(port, "a-long-user-name-that-runs-past-forty-bytes", "h").put(
            1,
            CA_PROTO_WRITE,
            "X.PROC",
        );
        Client::connect(port, "u", "a.host.name.that.runs.past.forty.bytes.example").put(
            1,
            CA_PROTO_WRITE,
            "X.PROC",
        );
    })
    .await
    .unwrap();
    assert_eq!(db.get_pv("X").unwrap(), EpicsValue::Double(4.0));
}
