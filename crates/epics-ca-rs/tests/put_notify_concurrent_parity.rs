//! **A second `CA_PROTO_WRITE_NOTIFY` never costs the first one its reply —
//! on either receive loop.**
//!
//! C `write_notify_action` (`rsrv/camessage.c:1704-1750`) serialises
//! concurrent put-callbacks on one channel by *waiting*: the arriving request
//! is never refused, and the predecessor is cancelled and answered
//! ECA_PUTCBINPROG only when `epicsEventWaitWithTimeout(client->blockSem,
//! 60.0)` runs out — `camessage.c:1745`, the sole site that status has in all
//! of rsrv, and it frames the reply from `&pPutNotify->msg`, the *saved*
//! request.
//!
//! The port used to answer the predecessor with ECA_PUTCBINPROG on every
//! arrival on the async loop, while the blocking loop, which registered
//! nothing, let the arriving request take the status from the database
//! instead. Both halves are now decided in `serve_write_head`, which both
//! loops run, so this file drives ONE script against both servers and
//! compares. A test that covered only the async loop is exactly what let the
//! disagreement survive a review round, so a one-loop test here is a bug in
//! the test.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

// Host/tokio-only, for the reason `receive_gate_parity.rs` gives: the async
// server's listener stack needs a tokio reactor the `rtems-exec-model`
// background executor does not start, and what this file adds is the
// comparison between the two loops, which is only meaningful where both run.
#![cfg(not(feature = "rtems-exec-model"))]

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
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

const PV: &str = "PUTCB:PARITY";
const DBR_DOUBLE: u16 = 6;
/// ioids of the two put-callbacks, distinct so a reply names which one it is.
const FIRST: u32 = 0x0000_1111;
const SECOND: u32 = 0x0000_2222;
/// How long a read waits before the circuit is declared silent. Loopback
/// replies land in microseconds; this only has to outrun scheduling noise.
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// The record: its first process goes async, so a WRITE_NOTIFY to it forks to
// `WriteHeadOutcome::AsyncPending` and stays in flight until the test says so
// ---------------------------------------------------------------------------

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// C holds PACT across a device round-trip; this holds it until
/// `complete_async_record`, which is the same window with a deterministic
/// end. Mirrors `AsyncOnceRecord` in
/// `epics-base-rs/tests/put_notify_defers_on_pact.rs`.
struct AsyncOnceRecord {
    val: f64,
    pending: bool,
}

impl Record for AsyncOnceRecord {
    fn record_type(&self) -> &'static str {
        "asynconce_putcb"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.pending {
            self.pending = false;
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: Vec::new(),
                device_did_compute: false,
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

fn new_record() -> AsyncOnceRecord {
    AsyncOnceRecord {
        val: 0.0,
        pending: true,
    }
}

fn seed_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    block_on_sync(db.add_record(PV, Box::new(new_record())))
        .expect("no async runtime on this thread")
        .expect("add_record");
    db
}

// ---------------------------------------------------------------------------
// What a loop did with the script
// ---------------------------------------------------------------------------

/// One WRITE_NOTIFY completion reply: which request it answers and with what.
#[derive(Debug, PartialEq, Eq)]
struct Completion {
    ioid: u32,
    status: u32,
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
    Closed,
    Quiet,
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
        match self.sock.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Read1::Closed,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Read1::Quiet;
            }
            Err(_) => return Read1::Closed,
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

    /// Read until `cmmd` shows up, keeping every other frame for later — the
    /// point of the `pending` queue: waiting for the echo must not swallow a
    /// completion reply that raced it.
    fn wait_for(&mut self, cmmd: u16) -> Vec<u8> {
        // Frames that are not the one being waited for go to `held`, not
        // straight back into `pending`: putting them back in the queue this
        // very loop reads from would hand the same frame round for ever.
        let mut held = VecDeque::new();
        let found = loop {
            match self.next_frame() {
                Read1::Frame(f) if u16::from_be_bytes([f[0], f[1]]) == cmmd => break f,
                Read1::Frame(f) => held.push_back(f),
                other => {
                    let what = match other {
                        Read1::Closed => "circuit closed",
                        _ => "circuit fell silent",
                    };
                    panic!("{what} before the expected 0x{cmmd:04x} frame");
                }
            }
        };
        // Restore wire order: what was parked came before anything still
        // unread.
        held.append(&mut self.pending);
        self.pending = held;
        found
    }

    /// Every WRITE_NOTIFY completion the circuit has produced so far, in wire
    /// order, reading until the server falls silent.
    fn drain_completions(&mut self, into: &mut Vec<Completion>) {
        loop {
            match self.next_frame() {
                Read1::Quiet | Read1::Closed => return,
                Read1::Frame(f) => {
                    if u16::from_be_bytes([f[0], f[1]]) == CA_PROTO_WRITE_NOTIFY {
                        into.push(Completion {
                            status: u32::from_be_bytes([f[8], f[9], f[10], f[11]]),
                            ioid: u32::from_be_bytes([f[12], f[13], f[14], f[15]]),
                        });
                    }
                }
            }
            assert!(into.len() <= 8, "runaway completion stream: {into:?}");
        }
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
// The script, run identically against each loop
// ---------------------------------------------------------------------------

/// Handshake, open the channel, fire two WRITE_NOTIFYs back to back while the
/// record is still mid-device-round-trip, then let the record finish and
/// report every completion the circuit produced.
///
/// The `CA_PROTO_ECHO` between the puts and the completion is the
/// synchronisation point: its reply proves the dispatch loop has already run
/// the write head for BOTH put-callbacks, so the second one really did arrive
/// while the first was in flight.
fn drive(addr: SocketAddr, db: &PvDatabase) -> Vec<Completion> {
    let mut c = Raw::connect(addr);
    c.send(&version_frame());
    c.send(&create_chan_frame(0x1234, PV));
    let cc = c.wait_for(CA_PROTO_CREATE_CHAN);
    let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);

    c.send(&write_notify_frame(sid, FIRST, 7.0));
    c.send(&write_notify_frame(sid, SECOND, 9.0));
    c.send(&echo_frame());
    c.wait_for(CA_PROTO_ECHO);

    let mut completions = Vec::new();
    // The device round-trip ends (C `dbNotifyCompletion`). A second pass is
    // offered for whatever the first completion restarted; the record is
    // synchronous by then, so a refusal here just means there was nothing
    // left to finish.
    for _ in 0..2 {
        let _ = block_on_sync(db.complete_async_record(PV)).expect("blockable");
        c.drain_completions(&mut completions);
    }
    completions
}

/// Run the script against the blocking driver.
fn against_blocking() -> Vec<Completion> {
    let db = seed_db();
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

    let out = drive(addr, &db);

    server.shutdown();
    let _ = accept.join();
    out
}

/// Run the script against the async driver.
///
/// The server owns a tokio runtime on its own thread and hands its database
/// back so the test can end the device round-trip; the client stays a plain
/// blocking socket, so both loops face byte-for-byte the same peer.
fn against_async() -> Vec<Completion> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(u16, Arc<PvDatabase>)>();
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
                .record(PV, new_record())
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

    let (port, db) = ready_rx.recv().expect("async server reports its port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let out = drive(addr, &db);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
    out
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// The status C reserves for a put-callback it gave up waiting for.
const ECA_PUTCBINPROG: u32 = epics_ca_rs::protocol::ECA_PUTCBINPROG;

fn first_completion(completions: &[Completion]) -> &Completion {
    completions
        .iter()
        .find(|c| c.ioid == FIRST)
        .unwrap_or_else(|| panic!("no completion for the first put-callback: {completions:?}"))
}

/// The defect, on the async loop: the arriving WRITE_NOTIFY used to cancel
/// the in-flight one and answer it ECA_PUTCBINPROG immediately. C only does
/// that after the 60 s `blockSem` wait (`camessage.c:1745`), which no test
/// here comes near, so the first put must keep its own reply.
#[test]
fn the_async_loop_leaves_the_first_put_callback_its_own_reply() {
    let got = against_async();
    assert_ne!(
        first_completion(&got).status,
        ECA_PUTCBINPROG,
        "async loop cancelled a put-callback that was nowhere near C's blockSem \
         deadline: {got:?}"
    );
}

/// The same boundary on the blocking loop, which registered no in-flight
/// put-callback at all and so could not have cancelled one. It is a pin: it
/// holds before and after, and it is what makes the comparison below a
/// statement about both loops rather than about one.
#[test]
fn the_blocking_loop_leaves_the_first_put_callback_its_own_reply() {
    let got = against_blocking();
    assert_ne!(
        first_completion(&got).status,
        ECA_PUTCBINPROG,
        "blocking loop cancelled a put-callback that was nowhere near C's blockSem \
         deadline: {got:?}"
    );
}

/// And the two loops answer the one script identically — the property that
/// went missing when only one of them kept a per-channel registration.
#[test]
fn both_loops_answer_concurrent_put_callbacks_the_same_way() {
    let blocking = against_blocking();
    let asynchronous = against_async();
    assert_eq!(
        asynchronous, blocking,
        "the two receive loops disagree about concurrent put-callbacks"
    );
}
