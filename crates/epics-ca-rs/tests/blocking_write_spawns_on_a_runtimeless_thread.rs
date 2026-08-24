//! **A CA write that makes the record defer a tail must not kill the blocking
//! driver's dispatch thread.**
//!
//! `BlockingCaServer` runs one plain `std::thread` per client with NO async
//! runtime entered, and its WRITE / WRITE_NOTIFY branch (`blocking.rs`) drives
//! `serve_write_head` through `block_on_sync`, which with no runtime visible
//! selects `park_on` (`epics-libcom-rs/src/runtime/task.rs`). The record
//! processing reached from that head is synchronous all the way down into
//! `PvDatabase`, which defers tails: `ProcessAction::ReprocessAfter` through
//! `schedule_delayed_reprocess`, an `OEVT` post from the OUT stage of
//! `process_record_with_links_body`, and eight more sites on the same chain.
//!
//! Those tails go to `runtime::task::spawn_background`, which lands on the
//! process-global background executor on every backend. Routed to the ambient
//! `runtime::task::spawn` instead — `tokio::spawn` on `tokio_backend` — each of
//! them panics "there is no reactor running" and takes the dispatch thread with
//! it, so a `caput` to an ODLY-style record killed that client's connection on
//! any hosted build. `epics_base_rs::server`'s own
//! `a_deferred_record_tail_never_uses_the_ambient_seam` census keeps every site
//! on the owner; the cases here are the runtime half of that pair.
//!
//! `command_drives_without_spawn` (`blocking.rs`) is the fail-closed allowlist
//! that answers "may this command run on a runtime-less thread", but WRITE /
//! WRITE_NOTIFY are routed by a dedicated branch ahead of it, so nothing
//! consults it for them — and it could not be the owner anyway: whether a tail
//! is deferred is a property of the record's process cycle, not of the CA
//! command.
//!
//! `ReprocessAfter` is what ODLY (calcout), swait's delay and the sequence
//! record's `DLYn` reduce to, so the record below is the narrowest faithful
//! stand-in for any of them; the SDLY `SimOutcome::DeferRead` path reaches the
//! same `schedule_delayed_reprocess` owner.
//!
//! **Backend asymmetry, deliberate.** Under `rtems-exec-model` (exec_backend)
//! the ambient `task::spawn` is itself the background executor, so these cases
//! pass there for a weaker reason than they do on the hosted build. They are
//! left ungated so both suites carry them, but a green run under
//! `rtems-exec-model` is NOT evidence about the hosted build.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CREATE_CHAN, CA_PROTO_VERSION,
    CA_PROTO_WRITE_NOTIFY, CaHeader, pad_string,
};
use epics_ca_rs::server::blocking::BlockingCaServer;

const PV: &str = "ODLY:SEAM";

/// The delayed re-entry the record asks for. Long enough that the reprocess
/// lands after the reply we are waiting for, short enough not to pad the test.
const REPROCESS_DELAY: Duration = Duration::from_millis(30);

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// Which deferred tail the record's first cycle produces. Two variants because
/// they land on two different spawn sites — `ReprocessAfter` on
/// `schedule_delayed_reprocess`, which also has to sleep before it re-enters,
/// and `OEVT` on the output-event post inside
/// `process_record_with_links_body` — and one of them covering the other is
/// exactly the assumption that let this family survive a round.
#[derive(Clone, Copy, PartialEq)]
enum Deferral {
    After,
    OutputEvent,
}

/// A record whose first process defers one re-entry — the shape calcout's
/// ODLY, swait's delay and `seq`'s `DLYn` all reduce to. `process()` itself is
/// synchronous: the only asynchrony is the framework's action, which is the
/// point.
struct ReprocessRecord {
    val: f64,
    /// Bumped on every `process()`, so a test can tell "the action never ran"
    /// apart from "the action ran and the spawn worked".
    processes: Arc<AtomicUsize>,
    deferral: Deferral,
    /// Only the first cycle asks for the re-entry; otherwise the record would
    /// re-arm forever and outlive the test.
    armed: bool,
}

impl Record for ReprocessRecord {
    fn record_type(&self) -> &'static str {
        "reprocess_seam"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.processes.fetch_add(1, Ordering::SeqCst);
        if self.armed {
            self.armed = false;
            Ok(ProcessOutcome {
                result: RecordProcessResult::Complete,
                actions: match self.deferral {
                    Deferral::After => vec![ProcessAction::ReprocessAfter(REPROCESS_DELAY)],
                    Deferral::OutputEvent => vec![],
                },
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

    /// `OEVT` set: the framework posts the named event from a spawned tail on
    /// the OUT stage of this very put.
    fn output_event(&self) -> Option<String> {
        match self.deferral {
            Deferral::OutputEvent => Some(OEVT.to_string()),
            Deferral::After => None,
        }
    }

    /// VAL is `pp(TRUE)`: a put to it processes the Passive record, which is
    /// what puts `execute_process_actions` on the writing thread.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

fn seed_db_with(deferral: Deferral) -> (Arc<PvDatabase>, Arc<AtomicUsize>) {
    let processes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(PvDatabase::new());
    block_on_sync(db.add_record(
        PV,
        Box::new(ReprocessRecord {
            val: 0.0,
            processes: processes.clone(),
            deferral,
            armed: true,
        }),
    ))
    .expect("no async runtime on this test thread")
    .expect("add_record");
    (db, processes)
}

fn seed_db() -> (Arc<PvDatabase>, Arc<AtomicUsize>) {
    seed_db_with(Deferral::After)
}

/// The event name the `OutputEvent` record posts on every cycle.
const OEVT: &str = "SEAM:OEVT";

/// Drive one CA put on this bare thread and wait for the deferred re-entry.
fn put_and_await_reentry(db: &Arc<PvDatabase>, processes: &Arc<AtomicUsize>) {
    let outcome = block_on_sync(db.put_record_field_from_ca(PV, "VAL", EpicsValue::Double(2.5)))
        .expect("no async runtime on this test thread");
    outcome.expect("the put itself must not error");
    assert_eq!(
        processes.load(Ordering::SeqCst),
        1,
        "the put must have processed the record once"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while processes.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        processes.load(Ordering::SeqCst) >= 2,
        "the deferred re-entry never fired"
    );
}

// ---------------------------------------------------------------------------
// A minimal raw CA client. Deliberately NOT `tests/common/raw_ca.rs`: this file
// needs three frames out of that module's twenty, and pulling the whole module
// in for a subset makes every helper it does not use a `dead_code` error.
// ---------------------------------------------------------------------------

const DBR_DOUBLE: u16 = 6;
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

fn start_server(db: Arc<PvDatabase>) -> (Arc<BlockingCaServer>, SocketAddr) {
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral loopback port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    std::thread::spawn(move || srv.serve());
    (server, addr)
}

struct Circuit {
    sock: TcpStream,
    pending: Vec<Vec<u8>>,
}

impl Circuit {
    fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).expect("connect to blocking server");
        sock.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        let mut c = Self {
            sock,
            pending: Vec::new(),
        };
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        c.send(h.to_bytes().as_ref());
        c.expect(CA_PROTO_VERSION, "unsolicited VERSION greeting");
        c
    }

    fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("write frame");
    }

    /// Wait for a frame carrying `cmmd`, keeping every other frame for a later
    /// expectation — a WRITE_NOTIFY can race the monitor update it fans out.
    fn expect(&mut self, cmmd: u16, what: &str) -> Vec<u8> {
        if let Some(i) = self
            .pending
            .iter()
            .position(|f| u16::from_be_bytes([f[0], f[1]]) == cmmd)
        {
            return self.pending.remove(i);
        }
        for _ in 0..16 {
            let mut hdr = [0u8; CaHeader::SIZE];
            self.sock
                .read_exact(&mut hdr)
                .unwrap_or_else(|e| panic!("{what}: {e} (the dispatch thread is gone)"));
            let postsize = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
            let mut frame = hdr.to_vec();
            if postsize > 0 {
                let mut body = vec![0u8; postsize];
                self.sock.read_exact(&mut body).expect("read frame body");
                frame.extend_from_slice(&body);
            }
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return frame;
            }
            self.pending.push(frame);
        }
        panic!("{what}: no matching frame in 16 frames");
    }

    fn create_channel(&mut self, cid: u32, pv: &str) -> u32 {
        let padded = pad_string(pv);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.cid = cid;
        h.available = CA_MINOR_VERSION as u32;
        h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        self.send(&f);
        self.expect(CA_PROTO_ACCESS_RIGHTS, "ACCESS_RIGHTS");
        let cc = self.expect(CA_PROTO_CREATE_CHAN, "CREATE_CHAN reply");
        u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]])
    }
}

fn write_notify_frame(sid: u32, ioid: u32, value: f64) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = ioid;
    h.set_payload_size(8, 1, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&value.to_be_bytes());
    f
}

/// The ioid a reply echoes, at bytes 12..16.
fn ioid_of(frame: &[u8]) -> u32 {
    u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]])
}

/// Capture panic messages from the server's own threads so a dead dispatch
/// thread names itself instead of showing up only as a silent socket.
///
/// Safe as a process-global: nextest runs one process per test.
fn capture_panics() -> Arc<std::sync::Mutex<Vec<String>>> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "<non-string panic payload>".to_string(),
            },
        };
        sink.lock().expect("panic sink poisoned").push(msg);
        previous(info);
    }));
    seen
}

#[test]
fn blocking_write_notify_to_a_reprocessing_record_answers_the_client() {
    let panics = capture_panics();
    let (db, processes) = seed_db();
    let (server, addr) = start_server(db);

    let mut c = Circuit::connect(addr);
    let sid = c.create_channel(0x0DEF, PV);

    c.send(&write_notify_frame(sid, 0x51, 3.5));
    let reply = c.expect(
        CA_PROTO_WRITE_NOTIFY,
        "WRITE_NOTIFY to an ODLY-style record",
    );
    assert_eq!(ioid_of(&reply), 0x51, "the reply answers our request");

    // The action must actually have run — otherwise a green test would only
    // mean the record never asked for a re-entry.
    assert_eq!(
        processes.load(Ordering::SeqCst),
        1,
        "the put must have processed the record once"
    );

    // The delayed re-process must land, which is the half `task::spawn` owns.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while processes.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        processes.load(Ordering::SeqCst) >= 2,
        "the ReprocessAfter re-entry never fired: schedule_delayed_reprocess \
         spawned nothing that ran"
    );

    let seen = panics.lock().expect("panic sink poisoned").clone();
    assert!(
        seen.is_empty(),
        "the server panicked on a runtime-less thread: {seen:?}"
    );

    server.shutdown();
}

/// The narrowest form of the same seam, with no sockets in the way: the CA put
/// entry point driven by `block_on_sync` on a bare thread, exactly as the
/// blocking driver's WRITE branch drives `serve_write_head`.
#[test]
fn a_ca_put_that_schedules_a_reprocess_survives_a_runtimeless_thread() {
    let (db, processes) = seed_db_with(Deferral::After);
    put_and_await_reentry(&db, &processes);
}

/// The same thread, a different spawn site: an `OEVT` post is spawned from the
/// OUT stage of `process_record_with_links_body`, not from
/// `schedule_delayed_reprocess`. Two of the ten sites on this chain, so a fix
/// that moved only the cited one still fails here.
#[test]
fn a_ca_put_that_posts_an_output_event_survives_a_runtimeless_thread() {
    let (db, processes) = seed_db_with(Deferral::OutputEvent);
    let outcome = block_on_sync(db.put_record_field_from_ca(PV, "VAL", EpicsValue::Double(2.5)))
        .expect("no async runtime on this test thread");
    outcome.expect("the put itself must not error");
    assert_eq!(
        processes.load(Ordering::SeqCst),
        1,
        "the put must have processed the record once"
    );
}

/// The control: the identical put with a runtime entered. It separates "the
/// record or the action is broken" from "the calling thread has no runtime to
/// spawn onto", which is the whole claim.
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread")]
async fn the_same_put_under_a_runtime_reprocesses() {
    let (db, processes) = seed_db();
    db.put_record_field_from_ca(PV, "VAL", EpicsValue::Double(2.5))
        .await
        .expect("the put itself must not error");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while processes.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        processes.load(Ordering::SeqCst) >= 2,
        "with a runtime entered the ReprocessAfter re-entry must fire"
    );
}
