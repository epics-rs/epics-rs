//! **A client that dies with its put-callback still busy hands the record to
//! whoever is queued behind it.**
//!
//! C does this from the teardown itself: `rsrvFreePutNotify` sees
//! `pNotify->busy` and calls `dbNotifyCancel` (`rsrv/camessage.c:1630-1638`),
//! whose `notifyProcessInProgress` arm runs `restartCheck` on every record the
//! notify owned (`dbNotify.c:428-434`) — and `restartCheck` pops the record's
//! restart list and gives the record to its head (`:156-168`).
//!
//! The queued client is what makes the teardown call load-bearing rather than
//! merely tidy. Releasing the slot lazily, at the next put that tests
//! ownership, cannot reach this case: the second client is ALREADY queued when
//! the first dies, so nothing further arrives and nothing tests anything. It
//! waits forever.
//!
//! `busy` is the record type that reaches the case without any device
//! simulation. It withholds `recGblFwdLink` — and so the put-callback — for as
//! long as VAL is non-zero (`busyRecord.c:271`), which is the contract, not a
//! stall: the client is expected to wait for the operation the record
//! represents. A client that gives up instead is exactly the teardown here.
//!
//! Both receive loops are driven, because each hands the completion to a
//! different owner — the async loop to the circuit's write-notify queue, the
//! blocking driver to its event thread — and the release rides on that owner
//! dropping the pending. One loop passing says nothing about the other.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

// RTEMS-EXEC-MODEL-ALLOW(1): measured, not argued — the 1 ungated case here
// (the blocking driver's) runs and passes under
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO, CA_PROTO_VERSION, CA_PROTO_WRITE_NOTIFY,
    CaHeader,
};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

const PV: &str = "CANCEL:BUSY";
const DBR_DOUBLE: u16 = 6;
const IOID_A: u32 = 0x0000_1111;
const IOID_B: u32 = 0x0000_2222;
/// Long enough that a completion which is coming at all has landed; the
/// failing shape never completes, so this is the whole cost of a red run.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

struct Raw {
    sock: TcpStream,
    pending: VecDeque<Vec<u8>>,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        Self {
            sock,
            pending: VecDeque::new(),
        }
    }

    fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("write frame");
    }

    fn next_frame(&mut self) -> Option<Vec<u8>> {
        if let Some(f) = self.pending.pop_front() {
            return Some(f);
        }
        let mut hdr = [0u8; CaHeader::SIZE];
        self.sock.read_exact(&mut hdr).ok()?;
        let mut frame = hdr.to_vec();
        let mut body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        if body == 0xFFFF {
            let mut annex = [0u8; 8];
            self.sock.read_exact(&mut annex).expect("extended annex");
            frame.extend_from_slice(&annex);
            body = u32::from_be_bytes([annex[0], annex[1], annex[2], annex[3]]) as usize;
        }
        if body > 0 {
            let mut rest = vec![0u8; body];
            self.sock.read_exact(&mut rest).expect("frame body");
            frame.extend_from_slice(&rest);
        }
        Some(frame)
    }

    fn wait_for(&mut self, cmmd: u16) -> Vec<u8> {
        let mut held = VecDeque::new();
        let found = loop {
            match self.next_frame() {
                Some(f) if u16::from_be_bytes([f[0], f[1]]) == cmmd => break f,
                Some(f) => held.push_back(f),
                None => panic!("circuit fell silent before 0x{cmmd:04x}"),
            }
        };
        held.append(&mut self.pending);
        self.pending = held;
        found
    }

    /// Open a channel on [`PV`] and return its sid.
    fn open(addr: SocketAddr, cid: u32) -> (Self, u32) {
        let mut c = Self::connect(addr);
        c.send(&version_frame());
        c.send(&create_chan_frame(cid, PV));
        let cc = c.wait_for(CA_PROTO_CREATE_CHAN);
        let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);
        (c, sid)
    }

    /// Put with callback, then round-trip an echo so the reply proves the
    /// receive loop has already run the write head for it.
    fn put_notify(&mut self, sid: u32, ioid: u32, value: f64) {
        self.send(&write_notify_frame(sid, ioid, value));
        self.send(CaHeader::new(CA_PROTO_ECHO).to_bytes().as_ref());
        self.wait_for(CA_PROTO_ECHO);
    }
}

fn version_frame() -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = CA_MINOR_VERSION;
    h.available = 0;
    h.to_bytes().to_vec()
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

fn val(db: &PvDatabase) -> Option<EpicsValue> {
    db.get_record(PV)?.read().record.get_field("VAL")
}

/// The async server's completions live in the circuit's write-notify queue
/// task, so the release rides on that task's `Vec` dropping when the circuit
/// ends.
#[cfg(tokio_backend)]
#[test]
fn the_async_loop_hands_the_record_to_the_queued_put_callback() {
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
                .record(PV, BusyRecord::default())
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
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_queued_client_is_answered(addr, &db);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

/// The blocking driver hands the completion to its event thread instead, so
/// the release rides on that thread's queue dropping. This is also the
/// embedded path, and the only one an exec-backend run can see.
#[test]
fn the_blocking_loop_hands_the_record_to_the_queued_put_callback() {
    let db = Arc::new(PvDatabase::new());
    epics_base_rs::runtime::task::block_on_sync(db.add_record(PV, Box::new(BusyRecord::default())))
        .expect("no async runtime on this thread")
        .expect("add_record");
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

    assert_queued_client_is_answered(addr, &db);

    server.shutdown();
    let _ = accept.join();
}

/// Two clients, one record: the second is queued when the first dies, and C
/// hands it the record from the teardown itself.
fn assert_queued_client_is_answered(addr: SocketAddr, db: &PvDatabase) {
    // Client A takes the record to BUSY. Its callback is withheld by contract,
    // so A owns the record's notify slot from here on.
    let (mut a, sid_a) = Raw::open(addr, 0x1111);
    a.put_notify(sid_a, IOID_A, 1.0);
    assert_eq!(val(db), Some(EpicsValue::Enum(1)), "A's write landed");

    // Client B arrives while A still owns it. C `processNotifyCommon` tests
    // ownership above `putCallback` (`dbNotify.c:213-219`), so B writes nothing
    // and parks on the record's restart list.
    let (mut b, sid_b) = Raw::open(addr, 0x2222);
    b.put_notify(sid_b, IOID_B, 0.0);
    assert_eq!(
        val(db),
        Some(EpicsValue::Enum(1)),
        "a queued put-callback must write nothing until it is replayed"
    );

    // A gives up and its circuit goes away with the put-callback still busy.
    drop(a);

    let reply = b.wait_for(CA_PROTO_WRITE_NOTIFY);
    assert_eq!(
        u32::from_be_bytes([reply[12], reply[13], reply[14], reply[15]]),
        IOID_B,
        "the completion must be B's"
    );
    assert_eq!(
        val(db),
        Some(EpicsValue::Enum(0)),
        "restartCheck hands the record to the queued client, which then writes"
    );
}
