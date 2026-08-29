//! **Two put-callbacks that report together reply in the order they were
//! requested — on either receive loop.**
//!
//! C settles every one of a client's put-callbacks from one place. The
//! database's completion callback appends to the per-client `putNotifyQue`
//! (`write_notify_done_callback`, `rsrv/camessage.c:1342`) and the single
//! `write_notify_reply` drain pops it and commits the frame
//! (`camessage.c:1376-1422`), so the wire order is the queue order. Two
//! put-callbacks on ONE channel are ordered before they ever reach that queue:
//! `write_notify_action` waits on `client->blockSem` while the channel's
//! previous one is busy (`camessage.c:1666-1705`), so the second is not even
//! submitted until the first has been answered. Two on one *record* are
//! ordered by the database instead — `processNotifyCommon` parks the loser on
//! `precord->ppnr->restartList` (`dbNotify.c:217`) and `restartCheck` pops it
//! with `ellFirst` (`dbNotify.c:156-164`).
//!
//! The async server used to spawn one task per put-callback, so both
//! completions became runnable microseconds apart and then raced for the
//! outbox. Nothing decided that race, but a runtime with spare workers wins it
//! the right way almost every time — one circuit's pair reversed 0 times in
//! 800 on an idle 96-CPU box — which is why the defect surfaced as one
//! full-workspace nextest run in two rather than as a reproducible failure.
//! So the starvation is built into the test rather than left to the machine:
//! the server runtime gets two worker threads and eight circuits drive it at
//! once. Against the one-task-per-put-callback shape that is 10 failures in 10
//! runs; against the queue, 0 in 10.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

// RTEMS-EXEC-MODEL-ALLOW(1): measured, not argued — all 1 case(s) here run and
// pass under `EPICS_RS_BUILD_EXEC_BACKEND=thread`. The file-level gate removed
// with this marker asserted a reactor panic the exec backend does not produce,
// and while it stood the exec-backend suite could not see this file at all.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, RecordProcessResult};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO, CA_PROTO_VERSION, CA_PROTO_WRITE_NOTIFY,
    CaHeader,
};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

/// One record per client thread: the threads must contend for the server's
/// runtime, not for one record's restart list, or the database serialises the
/// completions before the emission order is ever in question.
const PVS: [&str; 8] = [
    "PUTCB:ORDER0",
    "PUTCB:ORDER1",
    "PUTCB:ORDER2",
    "PUTCB:ORDER3",
    "PUTCB:ORDER4",
    "PUTCB:ORDER5",
    "PUTCB:ORDER6",
    "PUTCB:ORDER7",
];
const DBR_DOUBLE: u16 = 6;
/// ioids of the two put-callbacks, distinct so a reply names which one it is.
const FIRST: u32 = 0x0000_1111;
const SECOND: u32 = 0x0000_2222;
/// Trials per client thread. One trial is a coin toss on a scheduler heavily
/// biased towards the right answer, so the property only shows up over a run
/// of them, and only while the runtime has other completions to interleave —
/// hence one thread per entry in `PVS`, all firing into the same
/// two-worker server.
const TRIALS: usize = 100;
/// How long a read waits before the circuit is declared silent. Loopback
/// replies land in microseconds; this only has to outrun scheduling noise.
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// The record: every other process goes async, so ONE server runs every trial
// ---------------------------------------------------------------------------

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// C holds PACT across a device round-trip; this holds it until
/// `complete_async_record`, which is the same window with a deterministic end.
/// It re-arms on the following process — the one the restarted second
/// put-callback runs — so a trial leaves the record exactly as it found it and
/// the next trial forks async again.
struct AlternatingRecord {
    val: f64,
    next_is_async: bool,
}

impl Record for AlternatingRecord {
    fn record_type(&self) -> &'static str {
        "alternating_putcb"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let go_async = self.next_is_async;
        self.next_is_async = !self.next_is_async;
        if go_async {
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: Vec::new(),
                device_did_compute: false,
                post_write_fields: Vec::new(),
            })
        } else {
            Ok(ProcessOutcome::complete())
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    /// VAL is a `pp(TRUE)` field: a put to it processes the Passive record.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

fn new_record() -> AlternatingRecord {
    AlternatingRecord {
        val: 0.0,
        next_is_async: true,
    }
}

// ---------------------------------------------------------------------------
// A raw client that reads whole frames and never discards one
// ---------------------------------------------------------------------------

struct Raw {
    sock: TcpStream,
    pending: VecDeque<Vec<u8>>,
}

enum Read1 {
    Frame(Vec<u8>),
    Silent,
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

    fn next_frame(&mut self) -> Read1 {
        if let Some(f) = self.pending.pop_front() {
            return Read1::Frame(f);
        }
        let mut hdr = [0u8; CaHeader::SIZE];
        if self.sock.read_exact(&mut hdr).is_err() {
            return Read1::Silent;
        }
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
        Read1::Frame(frame)
    }

    /// Read until `cmmd` shows up, keeping every other frame for later: waiting
    /// for the echo must not swallow a completion reply that raced it.
    fn wait_for(&mut self, cmmd: u16) -> Vec<u8> {
        let mut held = VecDeque::new();
        let found = loop {
            match self.next_frame() {
                Read1::Frame(f) if u16::from_be_bytes([f[0], f[1]]) == cmmd => break f,
                Read1::Frame(f) => held.push_back(f),
                Read1::Silent => panic!("circuit fell silent before 0x{cmmd:04x}"),
            }
        };
        // Restore wire order: what was parked came before anything still unread.
        held.append(&mut self.pending);
        self.pending = held;
        found
    }

    /// The ioids of the next `want` WRITE_NOTIFY completions, in wire order.
    fn completion_ioids(&mut self, want: usize) -> Vec<u32> {
        let mut out = Vec::new();
        while out.len() < want {
            match self.next_frame() {
                Read1::Silent => break,
                Read1::Frame(f) => {
                    if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_WRITE_NOTIFY {
                        out.push(u32::from_be_bytes([f[12], f[13], f[14], f[15]]));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Frame builders
// ---------------------------------------------------------------------------

fn version_frame() -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = CA_MINOR_VERSION;
    h.available = 0;
    h.to_bytes().to_vec()
}

fn echo_frame() -> Vec<u8> {
    CaHeader::new(CA_PROTO_ECHO).to_bytes().to_vec()
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

// ---------------------------------------------------------------------------
// One trial
// ---------------------------------------------------------------------------

/// Open a channel, fire two WRITE_NOTIFYs back to back while the record is
/// still mid-device-round-trip, end the round trip, and report the completion
/// ioids in wire order.
///
/// The `CA_PROTO_ECHO` between the puts and the completion is the
/// synchronisation point: its reply proves the receive loop has already run
/// the write head for BOTH put-callbacks, so the second one really did arrive
/// while the first was in flight. Ending the round trip then fires the first
/// put-callback's completion and, through the database's restart list, the
/// second's — back to back, from one thread, which is the case the order is
/// about.
fn one_trial(addr: SocketAddr, db: &PvDatabase, pv: &str) -> Vec<u32> {
    let mut c = Raw::connect(addr);
    c.send(&version_frame());
    c.send(&create_chan_frame(0x1234, pv));
    let cc = c.wait_for(CA_PROTO_CREATE_CHAN);
    let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

    c.send(&write_notify_frame(sid, FIRST, 7.0));
    c.send(&write_notify_frame(sid, SECOND, 9.0));
    c.send(&echo_frame());
    c.wait_for(CA_PROTO_ECHO);

    let _ = block_on_sync(db.complete_async_record(pv)).expect("blockable");
    c.completion_ioids(2)
}

/// Every [`PVS`] entry drives [`TRIALS`] trials on its own circuit, at the same
/// time, so the server always has other put-callbacks settling while any one
/// circuit's pair reports.
fn assert_request_order(loop_name: &str, addr: SocketAddr, db: &Arc<PvDatabase>) {
    let mut clients = Vec::new();
    for pv in PVS {
        let db = db.clone();
        let loop_name = loop_name.to_string();
        clients.push(thread::spawn(move || {
            for trial in 0..TRIALS {
                assert_eq!(
                    one_trial(addr, &db, pv),
                    vec![FIRST, SECOND],
                    "{loop_name} answered two concurrent put-callbacks on {pv} \
                     out of request order on trial {trial}"
                );
            }
        }));
    }
    let mut failed = Vec::new();
    for c in clients {
        if let Err(panic) = c.join() {
            failed.push(panic);
        }
    }
    if let Some(panic) = failed.pop() {
        std::panic::resume_unwind(panic);
    }
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

#[cfg(tokio_backend)]
/// The defect: on the async loop each completion had its own task, so two that
/// reported together reached the outbox in scheduler order.
#[test]
fn the_async_loop_replies_in_request_order() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(u16, Arc<PvDatabase>)>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let mut builder = CaServer::builder().port(0).tcp_port(0);
            for pv in PVS {
                builder = builder.record(pv, new_record());
            }
            let server = builder.build().await.expect("build CA server");
            ready_tx
                .send((server.tcp_port(), server.database().clone()))
                .expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let (port, db) = ready_rx.recv().expect("async server reports its port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_request_order("the async loop", addr, &db);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

/// The same property on the blocking driver, whose event thread has always
/// been the single owner of these replies. It is a pin: it held before the
/// async loop got one, and a one-loop test here is what let the two disagree.
#[test]
fn the_blocking_loop_replies_in_request_order() {
    let db = Arc::new(PvDatabase::new());
    for pv in PVS {
        block_on_sync(db.add_record(pv, Box::new(new_record())))
            .expect("no async runtime on this thread")
            .expect("add_record");
    }
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

    assert_request_order("the blocking loop", addr, &db);

    server.shutdown();
    let _ = accept.join();
}
