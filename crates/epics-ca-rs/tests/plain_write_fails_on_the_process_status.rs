//! rsrv answers a plain `CA_PROTO_WRITE` whose processing returned non-zero
//! with `ECA_PUTFAIL`, and a `WRITE_NOTIFY` to the same record normally.
//!
//! `write_action` sends `ECA_PUTFAIL` when `dbChannel_put` is negative
//! (`camessage.c:812`), which it is for every non-zero `dbPutField` status
//! (`db_access.c:980`); `write_notify_action` goes through `dbNotify`, which
//! never reads the process status. Measured on softIocPVX with
//! `record(ao,"F:ao"){field(OUT,"NOPV:X CA")}`: `caput F:ao.PROC 1` raises
//! "Channel write request failed", `caput -c` succeeds; a longout on the same
//! dead link succeeds both ways, its soft device dropping `dbPutLink`'s status.

#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO, CA_PROTO_ERROR, CA_PROTO_HOST_NAME,
    CA_PROTO_VERSION, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY, CaHeader, ECA_NORMAL, ECA_PUTFAIL,
};
use epics_ca_rs::server::CaServerBuilder;

/// Every remote channel down: `dbCa`'s -1 on a put (`dbCa.c:558-560`).
struct Down;

#[async_trait::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: &[u8]) -> bool {
        false
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("channel disconnected".into())
    }
}

const DB: &str = r#"
record(ao, "F:ao") { field(OUT, "NOPV:X CA") }
record(longout, "S:lo") { field(OUT, "NOPV:X CA") }
"#;

const DBR_DOUBLE: u16 = 6;

async fn serve() -> u16 {
    let db = Arc::new(PvDatabase::new());
    db.register_link_set("ca", Arc::new(Down)).await;
    let (db, _) = IocBuilder::new()
        .with_database(db)
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let acf = epics_base_rs::server::access_security::new_acf_cell_watching(None, &db);
    let server = CaServerBuilder::serving(db, acf, None, None)
        .port(0)
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    tokio::spawn(async move { server.run().await });
    port
}

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
    fn connect(port: u16) -> Self {
        let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c = Self {
            sock,
            acc: Vec::new(),
        };
        let mut out = hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0);
        out.extend(hdr(CA_PROTO_CLIENT_NAME, 8, 0, 0, 0, 0));
        out.extend(padded("u"));
        out.extend(hdr(CA_PROTO_HOST_NAME, 8, 0, 0, 0, 0));
        out.extend(padded("h"));
        c.sock.write_all(&out).unwrap();
        c
    }

    fn next(&mut self) -> Vec<u8> {
        loop {
            if let Ok((h, size)) = CaHeader::from_bytes_extended(&self.acc) {
                let end = size + h.actual_postsize();
                if self.acc.len() >= end {
                    return self.acc.drain(..end).collect();
                }
            }
            let mut buf = [0u8; 4096];
            let n = self.sock.read(&mut buf).expect("a frame from the server");
            assert_ne!(n, 0, "the server closed the circuit");
            self.acc.extend_from_slice(&buf[..n]);
        }
    }

    fn create(&mut self, cid: u32, pv: &str) -> u32 {
        let name = padded(pv);
        let mut f = hdr(CA_PROTO_CREATE_CHAN, name.len() as u16, 0, 0, cid, 13);
        f.extend(name);
        self.sock.write_all(&f).unwrap();
        loop {
            let frame = self.next();
            if u16::from_be_bytes([frame[0], frame[1]]) == CA_PROTO_CREATE_CHAN {
                return u32::from_be_bytes(frame[12..16].try_into().unwrap());
            }
        }
    }

    /// Put 1.0 with `cmd`, then ECHO, and return the status of every ERROR
    /// and WRITE_NOTIFY reply that came before the ECHO's.
    fn put(&mut self, cmd: u16, sid: u32) -> Vec<(u16, u32)> {
        let mut f = hdr(cmd, 8, DBR_DOUBLE, 1, sid, 7);
        f.extend(1.0f64.to_be_bytes());
        f.extend(hdr(CA_PROTO_ECHO, 0, 0, 0, 0, 0));
        self.sock.write_all(&f).unwrap();
        let mut got = Vec::new();
        loop {
            let frame = self.next();
            let status = u32::from_be_bytes(frame[8..12].try_into().unwrap());
            match u16::from_be_bytes([frame[0], frame[1]]) {
                CA_PROTO_ECHO => return got,
                c @ (CA_PROTO_ERROR | CA_PROTO_WRITE_NOTIFY) => {
                    let status = if c == CA_PROTO_ERROR {
                        u32::from_be_bytes(frame[12..16].try_into().unwrap())
                    } else {
                        status
                    };
                    got.push((c, status));
                }
                _ => {}
            }
        }
    }
}

async fn puts(record: &'static str) -> (Vec<(u16, u32)>, Vec<(u16, u32)>) {
    let port = serve().await;
    tokio::task::spawn_blocking(move || {
        let mut c = Client::connect(port);
        let sid = c.create(1, &format!("{record}.PROC"));
        (
            c.put(CA_PROTO_WRITE, sid),
            c.put(CA_PROTO_WRITE_NOTIFY, sid),
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_process_fails_the_plain_write_only() {
    let (plain, notify) = puts("F:ao").await;
    assert_eq!(plain, [(CA_PROTO_ERROR, ECA_PUTFAIL)]);
    assert_eq!(notify, [(CA_PROTO_WRITE_NOTIFY, ECA_NORMAL)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_that_returns_zero_fails_neither() {
    let (plain, notify) = puts("S:lo").await;
    assert_eq!(plain, []);
    assert_eq!(notify, [(CA_PROTO_WRITE_NOTIFY, ECA_NORMAL)]);
}
