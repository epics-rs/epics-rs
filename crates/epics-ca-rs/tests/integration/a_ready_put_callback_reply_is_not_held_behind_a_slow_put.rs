//! A put-callback that has completed is answered at once, even while a later
//! request in the same read batch is still processing.
//!
//! rsrv does not wait for the receive loop to come back for the reply: the
//! completion queues it on `putNotifyQue` (`write_notify_done_callback`,
//! `rsrv/camessage.c:1342`) and the event task commits it and flushes the
//! client's send buffer itself (`rsrv_extra_labor`, `camessage.c:1502-1508`),
//! so a later put that blocks the receive loop in record processing (an sscan
//! PV-name special waiting for a connect) does not hold an earlier reply.
//!
//! Ports are always ephemeral (`:0`), never the real 5064.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(tokio_backend)]
use std::thread;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_VERSION, CA_PROTO_WRITE_NOTIFY, CaHeader,
};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;
use fact_budget::{FACT_BUDGET, barrier};

const READY_PV: &str = "PUTCB:READY";
const SLOW_PV: &str = "PUTCB:SLOW";
const DBR_DOUBLE: u16 = 6;
const READY_IOID: u32 = 0x0000_1111;
const SLOW_IOID: u32 = 0x0000_2222;

/// Holds the slow record's processing until the test has seen the ready reply.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }

    fn wait(&self) {
        let (open, cv) = &*self.0;
        let mut open = open.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    }
}

/// A failed assertion must not leave the server parked in the slow record.
struct OpensOnDrop(Gate);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// Processes synchronously; with a gate, processing blocks until it opens.
struct PutRecord {
    val: f64,
    gate: Option<Gate>,
}

impl Record for PutRecord {
    fn record_type(&self) -> &'static str {
        "ready_or_slow_putcb"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if let Some(gate) = &self.gate {
            gate.wait();
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Double(v)) => {
                self.val = v;
                Ok(())
            }
            ("VAL", _) => Err(CaError::TypeMismatch("VAL".into())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

fn records(gate: &Gate) -> [(&'static str, PutRecord); 2] {
    [
        (
            READY_PV,
            PutRecord {
                val: 0.0,
                gate: None,
            },
        ),
        (
            SLOW_PV,
            PutRecord {
                val: 0.0,
                gate: Some(gate.clone()),
            },
        ),
    ]
}

/// One whole frame, or `None` once `within` passes with nothing.
fn read_frame(sock: &mut TcpStream, within: Duration) -> Option<Vec<u8>> {
    sock.set_read_timeout(Some(within)).expect("read timeout");
    let mut hdr = [0u8; CaHeader::SIZE];
    match sock.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return None,
        Err(e) => panic!("the server closed the circuit: {e}"),
    }
    let body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
    let mut frame = hdr.to_vec();
    frame.resize(CaHeader::SIZE + body, 0);
    sock.set_read_timeout(Some(FACT_BUDGET))
        .expect("read timeout");
    sock.read_exact(&mut frame[CaHeader::SIZE..])
        .expect("frame body");
    Some(frame)
}

fn command(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[0], frame[1]])
}

fn word(frame: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]])
}

fn completion_of(ioid: u32) -> impl Fn(&Vec<u8>) -> bool {
    move |f| command(f) == CA_PROTO_WRITE_NOTIFY && word(f, 12) == ioid
}

fn create_chan_frame(cid: u32, pv: &str) -> Vec<u8> {
    let name = epics_ca_rs::protocol::pad_string(pv);
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.postsize = name.len() as u16;
    h.cid = cid;
    h.available = CA_MINOR_VERSION as u32;
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&name);
    f
}

fn write_notify_frame(sid: u32, ioid: u32, value: f64) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.postsize = 8;
    h.cid = sid;
    h.available = ioid;
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&value.to_be_bytes());
    f
}

/// The server's sid for each of `pvs`, in order.
fn open_channels(sock: &mut TcpStream, pvs: &[&str]) -> Vec<u32> {
    let mut version = CaHeader::new(CA_PROTO_VERSION);
    version.count = CA_MINOR_VERSION;
    let mut hello = version.to_bytes().to_vec();
    for (cid, pv) in pvs.iter().enumerate() {
        hello.extend_from_slice(&create_chan_frame(cid as u32, pv));
    }
    sock.write_all(&hello).expect("write the handshake");
    let mut sids = vec![None; pvs.len()];
    while sids.iter().any(Option::is_none) {
        let frame = read_frame(sock, FACT_BUDGET).expect("the server never answered CREATE_CHAN");
        if command(&frame) == CA_PROTO_CREATE_CHAN {
            sids[word(&frame, 8) as usize] = Some(word(&frame, 12));
        }
    }
    sids.into_iter().map(Option::unwrap).collect()
}

/// Both put-callbacks go out in one write, so the server reads them in one
/// batch; the slow one's record stays in processing until the ready one's
/// completion has reached the client.
fn assert_ready_reply_not_held(addr: SocketAddr, gate: &Gate) {
    let _release = OpensOnDrop(gate.clone());
    let mut sock = TcpStream::connect(addr).expect("connect");
    let sids = open_channels(&mut sock, &[READY_PV, SLOW_PV]);

    let mut puts = write_notify_frame(sids[0], READY_IOID, 1.0);
    puts.extend_from_slice(&write_notify_frame(sids[1], SLOW_IOID, 2.0));
    sock.write_all(&puts).expect("write both put-callbacks");

    barrier::until(
        "the ready put-callback's reply while the slow put still processes",
        completion_of(SLOW_IOID),
        completion_of(READY_IOID),
        |remaining| read_frame(&mut sock, remaining),
    );

    gate.open();
    barrier::until(
        "the slow put-callback's reply once its processing ends",
        |_| false,
        completion_of(SLOW_IOID),
        |remaining| read_frame(&mut sock, remaining),
    );
}

#[cfg(tokio_backend)]
#[test]
fn the_async_loop_answers_a_ready_put_callback_behind_a_slow_one() {
    let gate = Gate::default();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u16>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let served = gate.clone();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let mut builder = CaServer::builder().port(0).tcp_port(0);
            for (pv, record) in records(&served) {
                builder = builder.record(pv, record);
            }
            let server = builder.build().await.expect("build CA server");
            ready_tx.send(server.tcp_port()).expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let port = ready_rx.recv().expect("async server reports its port");
    assert_ready_reply_not_held(SocketAddr::from(([127, 0, 0, 1], port)), &gate);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

#[test]
fn the_blocking_loop_answers_a_ready_put_callback_behind_a_slow_one() {
    let gate = Gate::default();
    let db = Arc::new(PvDatabase::new());
    for (pv, record) in records(&gate) {
        block_on_sync(db.add_record(pv, Box::new(record)))
            .expect("no async runtime on this thread")
            .expect("add_record");
    }
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = std::thread::spawn(move || srv.serve());

    assert_ready_reply_not_held(addr, &gate);

    server.shutdown();
    let _ = accept.join();
}
