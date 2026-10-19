//! **A put-callback refused on its replay is answered ECA_PUTFAIL.**
//!
//! A put-callback that lands on a record another put-callback owns is queued
//! unwritten (`dbNotify.c:213-219`). On its restart `putCallback` fails the
//! conversion, sets `ppn->status = notifyError` and still returns
//! `didPut = 1` (`dbNotify.c:528-530`), so the record processes
//! (`:243-256`) and `write_notify_reply` answers ECA_PUTFAIL for the status
//! (`rsrv/camessage.c:1386-1403`).
//!
//! Both receive loops are driven, as each awaits the completion on its own
//! owner. Ports are always ephemeral (`:0`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_VERSION, CA_PROTO_WRITE_NOTIFY, CaHeader,
    ECA_NORMAL, ECA_PUTFAIL,
};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

const REC: &str = "QUEUED:CALCOUT";
const PV: &str = "QUEUED:CALCOUT.A";
const DBR_STRING: u16 = 0;
const DBR_DOUBLE: u16 = 6;

struct Raw(TcpStream);

impl Raw {
    fn open(addr: SocketAddr, cid: u32) -> (Self, u32) {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut c = Self(sock);
        let mut v = CaHeader::new(CA_PROTO_VERSION);
        v.count = CA_MINOR_VERSION;
        c.0.write_all(v.to_bytes().as_ref()).unwrap();
        let name = epics_ca_rs::protocol::pad_string(PV);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.postsize = name.len() as u16;
        h.cid = cid;
        h.available = CA_MINOR_VERSION as u32;
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&name);
        c.0.write_all(&f).unwrap();
        let cc = c.wait_for(CA_PROTO_CREATE_CHAN);
        (c, u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]))
    }

    fn wait_for(&mut self, cmmd: u16) -> Vec<u8> {
        loop {
            let mut hdr = [0u8; CaHeader::SIZE];
            self.0
                .read_exact(&mut hdr)
                .unwrap_or_else(|e| panic!("no 0x{cmmd:04x}: {e}"));
            let mut body = vec![0u8; u16::from_be_bytes([hdr[2], hdr[3]]) as usize];
            self.0.read_exact(&mut body).unwrap();
            if u16::from_be_bytes([hdr[0], hdr[1]]) == cmmd {
                return [hdr.as_slice(), &body].concat();
            }
        }
    }

    fn put_notify(&mut self, sid: u32, ioid: u32, data_type: u16, payload: &[u8]) {
        let mut h = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
        h.data_type = data_type;
        h.count = 1;
        h.postsize = payload.len() as u16;
        h.cid = sid;
        h.available = ioid;
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(payload);
        self.0.write_all(&f).unwrap();
    }

    /// The ECA status of the next WRITE_NOTIFY reply.
    fn reply_status(&mut self) -> u32 {
        let r = self.wait_for(CA_PROTO_WRITE_NOTIFY);
        u32::from_be_bytes([r[8], r[9], r[10], r[11]])
    }
}

fn load(db: &PvDatabase) {
    for (field, value) in [
        ("CALC", EpicsValue::String("A".into())),
        ("ODLY", EpicsValue::Double(0.5)),
    ] {
        epics_base_rs::runtime::task::block_on_sync(
            db.put_record_field_from_ca_no_notify(REC, field, value),
        )
        .expect("no async runtime on this thread")
        .expect("settable");
    }
}

fn assert_queued_refusal_fails(addr: SocketAddr, db: &PvDatabase) {
    // A owns the record for its ODLY cycle.
    let (mut a, sid_a) = Raw::open(addr, 1);
    a.put_notify(sid_a, 0x11, DBR_DOUBLE, &1.0f64.to_be_bytes());
    let rec = db.get_record(REC).unwrap();
    while !rec.read().is_processing() {
        thread::sleep(Duration::from_millis(1));
    }

    // B queues behind A; its replay fails the conversion and still processes.
    let (mut b, sid_b) = Raw::open(addr, 2);
    let mut nope = b"nope".to_vec();
    nope.resize(40, 0);
    b.put_notify(sid_b, 0x22, DBR_STRING, &nope);

    assert_eq!(a.reply_status(), ECA_NORMAL, "A's own put succeeded");
    assert_eq!(
        b.reply_status(),
        ECA_PUTFAIL,
        "B's replayed put was refused"
    );
}

#[cfg(tokio_backend)]
#[test]
fn the_async_loop_answers_a_queued_refusal_putfail() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(u16, Arc<PvDatabase>)>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .record(REC, CalcoutRecord::default())
                .build()
                .await
                .expect("build CA server");
            ready_tx
                .send((server.tcp_port(), server.database().clone()))
                .expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let (port, db) = ready_rx.recv().expect("server reports its port");
    load(&db);
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_queued_refusal_fails(addr, &db);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

#[test]
fn the_blocking_loop_answers_a_queued_refusal_putfail() {
    let db = Arc::new(PvDatabase::new());
    epics_base_rs::runtime::task::block_on_sync(
        db.add_record(REC, Box::new(CalcoutRecord::default())),
    )
    .expect("no async runtime on this thread")
    .expect("add_record");
    load(&db);
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());

    assert_queued_refusal_fails(addr, &db);

    server.shutdown();
    let _ = accept.join();
}
