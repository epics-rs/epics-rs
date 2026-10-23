//! A scalar `DBR_STRING` write must carry a NUL within its received bytes.
//!
//! C `write_action` and `write_notify_action` return `RSRV_ERROR` when
//! `epicsStrnLen(pPayload, m_postsize) >= m_postsize` (camessage.c:761-766,
//! :1696-1701): softIoc prints "CAS: forcing disconnect" and the record keeps
//! its value. Rows measured on softIoc 8f5015b66 over this `.db`, each record
//! first set to "x", then one put of `abcdefghij`x4:
//!
//! ```text
//! 40 bytes, no NUL    WRITE / WRITE_NOTIFY  L41 L128 SO  circuit closed; VAL "x", LEN 2
//! 48 bytes, NUL at 40 WRITE                 L41 L128     39-char VAL, LEN 41
//!                     WRITE                 SO           39-char VAL
//!                     WRITE_NOTIFY          L41          ECA_NORMAL; 39-char VAL, LEN 41
//!                     WRITE_NOTIFY          SO           ECA_NORMAL; 39-char VAL
//! ```
//!
//! WRITE_NOTIFY on L128 with 48 bytes is not pinned: C's `strncpy` of 40
//! bytes leaves no NUL and the lso reads heap past the buffer. LEN 41 on
//! the accepted rows is the lso post-LEN fix, not this gate, so it is not
//! read there.

#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_READ_NOTIFY, CA_PROTO_VERSION, CA_PROTO_WRITE,
    CA_PROTO_WRITE_NOTIFY, CaHeader, ECA_NORMAL,
};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

const DB: &str = r#"
record(lso,"L41") { field(SIZV,"41") }
record(lso,"L128") { field(SIZV,"128") }
record(stringout,"SO") {}
"#;

const DBR_STRING: u16 = 0;
const DBR_LONG: u16 = 5;
const READ_TIMEOUT: Duration = Duration::from_millis(2000);
const CHARS: &[u8; 40] = b"abcdefghijabcdefghijabcdefghijabcdefghij";

struct Raw {
    sock: TcpStream,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        let mut raw = Self { sock };
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        raw.send(&h.to_bytes());
        raw
    }

    fn send(&mut self, frame: &[u8]) {
        // EPIPE after a dropped circuit is the outcome under test.
        let _ = self.sock.write_all(frame);
    }

    fn next_frame(&mut self) -> Option<Vec<u8>> {
        let mut hdr = [0u8; CaHeader::SIZE];
        self.sock.read_exact(&mut hdr).ok()?;
        let mut frame = hdr.to_vec();
        let mut body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        if body == 0xFFFF {
            let mut annex = [0u8; 8];
            self.sock.read_exact(&mut annex).ok()?;
            frame.extend_from_slice(&annex);
            body = u32::from_be_bytes([annex[0], annex[1], annex[2], annex[3]]) as usize;
        }
        let mut rest = vec![0u8; body];
        self.sock.read_exact(&mut rest).ok()?;
        frame.extend_from_slice(&rest);
        Some(frame)
    }

    fn read_until(&mut self, cmmd: u16) -> Option<Vec<u8>> {
        for _ in 0..8 {
            let frame = self.next_frame()?;
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return Some(frame);
            }
        }
        None
    }

    fn open(&mut self, cid: u32, pv: &str) -> u32 {
        let name = epics_ca_rs::protocol::pad_string(pv);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.postsize = name.len() as u16;
        h.cid = cid;
        h.available = CA_MINOR_VERSION as u32;
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&name);
        self.send(&f);
        let created = self
            .read_until(CA_PROTO_CREATE_CHAN)
            .unwrap_or_else(|| panic!("no CREATE_CHAN reply for {pv}"));
        u32::from_be_bytes([created[12], created[13], created[14], created[15]])
    }

    /// `WRITE_NOTIFY` status, or `None` when the circuit closed instead.
    fn put_notify(&mut self, sid: u32, payload: &[u8]) -> Option<u32> {
        self.send(&string_write(CA_PROTO_WRITE_NOTIFY, sid, payload));
        self.read_until(CA_PROTO_WRITE_NOTIFY)
            .map(|f| u32::from_be_bytes([f[8], f[9], f[10], f[11]]))
    }

    fn read(&mut self, sid: u32, data_type: u16) -> Option<Vec<u8>> {
        let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
        h.data_type = data_type;
        h.cid = sid;
        h.available = 0x99;
        h.set_payload_size(0, 1, CA_MINOR_VERSION)
            .expect("modern peer");
        self.send(&h.to_bytes());
        self.read_until(CA_PROTO_READ_NOTIFY)
            .map(|f| f[CaHeader::SIZE..].to_vec())
    }
}

/// A count-1 `DBR_STRING` write whose body is exactly `payload`.
fn string_write(cmmd: u16, sid: u32, payload: &[u8]) -> Vec<u8> {
    let mut h = CaHeader::new(cmmd);
    h.data_type = DBR_STRING;
    h.cid = sid;
    h.available = 0x77;
    h.set_payload_size(payload.len(), 1, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(payload);
    f
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    /// The circuit answered a read after the put.
    survived: bool,
    /// `WRITE_NOTIFY` status; `None` for `WRITE` or a closed circuit.
    status: Option<u32>,
    val: String,
    /// `LEN`, read only when the row pins one.
    len: Option<i32>,
}

fn run_case(addr: SocketAddr, record: &str, cmmd: u16, payload: &[u8], want_len: bool) -> Observed {
    let mut seed = Raw::connect(addr);
    let sid = seed.open(1, record);
    assert_eq!(seed.put_notify(sid, b"x\0\0\0\0\0\0\0"), Some(ECA_NORMAL));

    let mut c = Raw::connect(addr);
    let sid = c.open(2, record);
    let status = if cmmd == CA_PROTO_WRITE_NOTIFY {
        c.put_notify(sid, payload)
    } else {
        c.send(&string_write(cmmd, sid, payload));
        None
    };
    let survived = c.read(sid, DBR_STRING).is_some();

    let val_sid = seed.open(3, record);
    let val = seed.read(val_sid, DBR_STRING).expect("VAL read");
    let val = String::from_utf8_lossy(&val[..val.iter().position(|&b| b == 0).unwrap()]).into();
    let len = want_len.then(|| {
        let len_sid = seed.open(4, &format!("{record}.LEN"));
        let b = seed.read(len_sid, DBR_LONG).expect("LEN read");
        i32::from_be_bytes([b[0], b[1], b[2], b[3]])
    });
    Observed {
        survived,
        status,
        val,
        len,
    }
}

fn seed_db() -> Arc<PvDatabase> {
    let (db, _) = block_on_sync(
        IocBuilder::new()
            .db_string(DB, &HashMap::new())
            .expect("parse db")
            .build(),
    )
    .expect("no async runtime on this thread")
    .expect("build ioc");
    db
}

fn against_blocking(case: &dyn Fn(SocketAddr) -> Observed) -> Observed {
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            seed_db(),
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());
    let got = case(addr);
    server.shutdown();
    let _ = accept.join();
    got
}

fn against_async(case: &dyn Fn(SocketAddr) -> Observed) -> Observed {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .db_string(DB, &HashMap::new())
                .expect("parse db")
                .build()
                .await
                .expect("build CA server");
            port_tx.send(server.tcp_port()).expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });
    let port = port_rx.recv().expect("async server reports its port");
    let got = case(format!("127.0.0.1:{port}").parse().unwrap());
    let _ = stop_tx.send(());
    let _ = server_thread.join();
    got
}

fn both_loops(record: &str, cmmd: u16, payload: &[u8], expect: &Observed) {
    let case = |addr| run_case(addr, record, cmmd, payload, expect.len.is_some());
    assert_eq!(&against_blocking(&case), expect, "blocking {record} {cmmd}");
    assert_eq!(&against_async(&case), expect, "async {record} {cmmd}");
}

#[test]
fn forty_bytes_without_a_nul_drop_the_circuit() {
    for cmmd in [CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY] {
        for (record, len) in [("L41", Some(2)), ("L128", Some(2)), ("SO", None)] {
            both_loops(
                record,
                cmmd,
                CHARS,
                &Observed {
                    survived: false,
                    status: None,
                    val: "x".into(),
                    len,
                },
            );
        }
    }
}

#[test]
fn forty_eight_bytes_with_a_nul_are_accepted() {
    let mut payload = CHARS.to_vec();
    payload.extend_from_slice(&[0; 8]);
    let thirty_nine = String::from_utf8_lossy(&CHARS[..39]).into_owned();
    let rows = [
        (CA_PROTO_WRITE, "L41", None),
        (CA_PROTO_WRITE, "L128", None),
        (CA_PROTO_WRITE, "SO", None),
        (CA_PROTO_WRITE_NOTIFY, "L41", Some(ECA_NORMAL)),
        (CA_PROTO_WRITE_NOTIFY, "SO", Some(ECA_NORMAL)),
    ];
    for (cmmd, record, status) in rows {
        both_loops(
            record,
            cmmd,
            &payload,
            &Observed {
                survived: true,
                status,
                val: thirty_nine.clone(),
                len: None,
            },
        );
    }
}
