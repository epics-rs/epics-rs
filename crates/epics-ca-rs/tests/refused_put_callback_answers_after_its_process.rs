//! A put callback whose write is refused but whose record still processes
//! (`db_put_process` returns 1 on a failed `dbChannelPut`, `db_access.c:
//! 1037-1043`) is answered from `dbNotifyCompletion`, once that process ends:
//! the `ECA_PUTFAIL` reply comes after the chain, as a successful put's
//! `ECA_NORMAL` would. A refusal that processes nothing (`DISP`, `:1031-1033`)
//! reaches `callDone` inline and is answered at once.

#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_VERSION, CA_PROTO_WRITE_NOTIFY, CaHeader,
    ECA_PUTFAIL,
};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

/// `A` is pp for calcout; `ODLY` holds the process for half a second.
const DB: &str = r#"
record(calcout, "CO") { field(CALC, "1") field(ODLY, "0.5") }
record(calcout, "COD") { field(CALC, "1") field(ODLY, "0.5") field(DISP, "1") }
"#;

const DBR_STRING: u16 = 0;
const DELAY: Duration = Duration::from_millis(500);

struct Raw {
    sock: TcpStream,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut raw = Self { sock };
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        raw.send(&h.to_bytes());
        raw
    }

    fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("send");
    }

    fn next_frame(&mut self) -> Vec<u8> {
        let mut hdr = [0u8; CaHeader::SIZE];
        self.sock.read_exact(&mut hdr).expect("a reply");
        let mut frame = hdr.to_vec();
        let body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        let mut rest = vec![0u8; body];
        self.sock.read_exact(&mut rest).expect("a reply body");
        frame.extend_from_slice(&rest);
        frame
    }

    fn read_until(&mut self, cmmd: u16) -> Vec<u8> {
        loop {
            let frame = self.next_frame();
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return frame;
            }
        }
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
        let created = self.read_until(CA_PROTO_CREATE_CHAN);
        u32::from_be_bytes([created[12], created[13], created[14], created[15]])
    }

    /// A `DBR_STRING` put callback of `text`: its status and how long the
    /// reply took.
    fn put_notify(&mut self, sid: u32, text: &str) -> (u32, Duration) {
        let mut payload = text.as_bytes().to_vec();
        payload.resize(8, 0);
        let mut h = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
        h.data_type = DBR_STRING;
        h.cid = sid;
        h.available = 0x77;
        h.set_payload_size(payload.len(), 1, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&payload);
        let sent = Instant::now();
        self.send(&f);
        let reply = self.read_until(CA_PROTO_WRITE_NOTIFY);
        (
            u32::from_be_bytes([reply[8], reply[9], reply[10], reply[11]]),
            sent.elapsed(),
        )
    }
}

fn put(addr: SocketAddr, pv: &str) -> (u32, Duration) {
    let mut c = Raw::connect(addr);
    let sid = c.open(1, pv);
    c.put_notify(sid, "nope")
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

fn against_blocking<T>(case: impl Fn(SocketAddr) -> T) -> T {
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

fn against_async<T>(case: impl Fn(SocketAddr) -> T) -> T {
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

/// Boundary — the refused put processes the record: PUTFAIL after the delay.
#[test]
fn a_refused_put_that_processes_is_answered_after_the_process() {
    for (front, (status, took)) in [
        ("blocking", against_blocking(|a| put(a, "CO.A"))),
        ("async", against_async(|a| put(a, "CO.A"))),
    ] {
        assert_eq!(status, ECA_PUTFAIL, "{front}");
        assert!(
            took >= DELAY,
            "{front}: answered {took:?} into a {DELAY:?} process"
        );
    }
}

/// Boundary — `DISP` refuses the put and nothing processes: PUTFAIL at once.
#[test]
fn a_refused_put_that_does_not_process_is_answered_at_once() {
    for (front, (status, took)) in [
        ("blocking", against_blocking(|a| put(a, "COD.A"))),
        ("async", against_async(|a| put(a, "COD.A"))),
    ] {
        assert_eq!(status, ECA_PUTFAIL, "{front}");
        assert!(
            took < DELAY,
            "{front}: answered {took:?}, waiting out a process"
        );
    }
}

/// Boundary — the refused write is to `PROC`, which processes on any put
/// (`dbNotify.c:243-246`): PUTFAIL after the delay.
#[test]
fn a_refused_proc_put_is_answered_after_the_process() {
    for (front, (status, took)) in [
        ("blocking", against_blocking(|a| put(a, "CO.PROC"))),
        ("async", against_async(|a| put(a, "CO.PROC"))),
    ] {
        assert_eq!(status, ECA_PUTFAIL, "{front}");
        assert!(
            took >= DELAY,
            "{front}: answered {took:?} into a {DELAY:?} process"
        );
    }
}
