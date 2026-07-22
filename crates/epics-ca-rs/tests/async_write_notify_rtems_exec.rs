//! Host proof that an **asynchronous** record's `WRITE_NOTIFY` completion runs
//! on the std-thread background executor — the RTEMS execution model — with
//! **zero tokio**, selected on a hosted target by the `rtems-exec-model` feature.
//!
//! This is the follow-up that closes the gap the synchronous e2e
//! (`blocking_rtems_e2e.rs`) documents: there, `WRITE_NOTIFY` on an `ao`
//! completed *synchronously on the client thread* via `park_on`, so nothing on
//! the CA path routed through the background executor. Here the target is a
//! genuine **async** record (`calcout` with `ODLY > 0`, `OOPT = Every Time`),
//! whose output is deferred by the output-delay watchdog. The deferral goes
//! through `PvDatabase::schedule_delayed_reprocess` — a single
//! `runtime::task::spawn` of a future that `runtime::task::sleep`s for `ODLY`
//! then fires the continuation — the deadlock-free async path (not the nested
//! `spawn` + `spawn_blocking` device-async path).
//!
//! # What routes where
//!
//! * **Server front-end** — [`BlockingCaServer`], all `std::thread`s, driven by
//!   `block_on_sync` → `park_on`. No tokio runtime, no `runtime::task::spawn`.
//!   The per-client thread that handles the `WRITE_NOTIFY` *parks* on the
//!   put-notify completion oneshot while the record is async.
//! * **Async completion** — under the `rtems-exec-model` feature, `build.rs`
//!   sets the `exec_backend` cfg, so the `runtime::task` seam
//!   (`spawn`/`sleep`) routes into the process-global `BackgroundExecutor`
//!   (callback pool `cbLow`/`cbMedium`/`cbHigh` + delayed timer + scanOnce),
//!   *not* tokio. `spawn` lands on the callback pool at the default `Medium`
//!   band, so the ODLY continuation — the `calcout` re-process that finally
//!   writes the OUT link — runs on the **`cbMedium`** worker thread. When it
//!   clears PACT the parked server thread wakes and sends the `WRITE_NOTIFY`
//!   reply.
//! * **Client** — a raw `std::net::TcpStream` speaking CA by hand. The async
//!   `CaClient` cannot be the driver here: its `tokio::net` search/circuit
//!   tasks are spawned through the same seam, so under `exec_backend` they land
//!   on a callback-pool worker with no tokio reactor and panic
//!   ("there is no reactor running") — which is also why the async front-end is
//!   host-only and RTEMS ships only the blocking driver. A raw socket client is
//!   both necessary here and the faithful stand-in for the remote C client an
//!   RTEMS IOC actually serves.
//!
//! # The assertion that pins the model
//!
//! A custom `threadCap` record is the `calcout`'s OUT target (`… PP`). Its
//! `process()` records `std::thread::current().name()`. Because the OUT write
//! happens *only* in the deferred continuation, the capture can only fire on
//! the thread that runs that continuation. The test asserts that thread is
//! **`cbMedium`** — a background-executor worker — proving the async completion
//! did not silently regress to a tokio worker.
//!
//! The whole file is `#[cfg(feature = "rtems-exec-model")]`: with the feature
//! off it compiles to nothing, so the hosted default test set is unchanged.
#![cfg(feature = "rtems-exec-model")]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - this file IS the feature-ON e2e; it runs and passes there.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DBR_LONG, DbFieldType, EpicsValue};
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CH_FAIL,
    CA_PROTO_CREATE_CHAN, CA_PROTO_HOST_NAME, CA_PROTO_VERSION, CA_PROTO_WRITE_NOTIFY, CaHeader,
    ECA_NORMAL, build_put_frame, pad_string,
};
use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search};
use serial_test::serial;

/// Shared sink for the thread name(s) the OUT-target record processes on.
#[derive(Clone, Default)]
struct ThreadCapture {
    names: Arc<Mutex<Vec<String>>>,
}

/// Minimal custom record: the `calcout`'s OUT target. On every `process()` it
/// records the name of the thread it is running on — the single observable that
/// pins which executor drove the deferred completion.
struct CaptureRecord {
    val: f64,
    capture: ThreadCapture,
}

static CAP_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

impl Record for CaptureRecord {
    fn record_type(&self) -> &'static str {
        "threadCap"
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        CAP_FIELDS
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            // The calcout OUT link converts OVAL to this field's native
            // (Double) type; accept it, but the value is incidental — the
            // capture below is the point of this record.
            if let EpicsValue::Double(v) = value {
                self.val = v;
            }
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let name = thread::current().name().unwrap_or("<unnamed>").to_string();
        self.capture.names.lock().unwrap().push(name);
        Ok(ProcessOutcome::complete())
    }
}

/// Load a real IOC: an async `calcout` (ODLY output delay) whose OUT link
/// processes the custom `threadCap` capture record.
async fn build_async_db(capture: ThreadCapture) -> Arc<PvDatabase> {
    // CALC "A+1" with A defaulting to 0 → VAL = 1 every cycle; OOPT defaults to
    // 0 ("Every Time") so should_output() is always true; ODLY = 0.05s defers
    // the OUT write by the output-delay watchdog, making the record async.
    let db_text = "record(calcout, \"RTEMS:NOTIFY\") {\n\
         field(CALC, \"A+1\")\n\
         field(ODLY, \"0.05\")\n\
         field(OUT, \"RTEMS:CAP.VAL PP\")\n\
         }\n\
         record(threadCap, \"RTEMS:CAP\") {\n\
         field(VAL, \"0\")\n\
         }\n";

    let factory_capture = capture.clone();
    let (db, _autosave) = IocBuilder::new()
        .register_record_type("threadCap", move || {
            Box::new(CaptureRecord {
                val: 0.0,
                capture: factory_capture.clone(),
            })
        })
        .db_string(db_text, &HashMap::new())
        .expect("load db string")
        .build()
        .await
        .expect("iocInit");
    db
}

/// Build one CA identity frame (`CLIENT_NAME` / `HOST_NAME`): a plain 16-byte
/// header plus a NUL-terminated, 8-byte-aligned name payload. Mirrors the
/// client's `build_identity_frame`, replicated here because it is `pub(crate)`.
fn identity_frame(cmd: u16, value: &str) -> Vec<u8> {
    let payload = pad_string(value);
    let mut hdr = CaHeader::new(cmd);
    hdr.postsize = payload.len() as u16;
    let mut frame = hdr.to_bytes().to_vec();
    frame.extend_from_slice(&payload);
    frame
}

/// Read exactly one CA frame (header + payload) from the circuit. None of the
/// replies this test reads (VERSION, ACCESS_RIGHTS, CREATE_CHAN, WRITE_NOTIFY)
/// carries an extended header or a nonzero payload, so extended-form parsing is
/// asserted-absent rather than handled.
fn read_frame(stream: &mut TcpStream) -> CaResult<(CaHeader, Vec<u8>)> {
    let mut header_bytes = [0u8; 16];
    stream
        .read_exact(&mut header_bytes)
        .map_err(epics_base_rs::error::CaError::Io)?;
    let postsize = u16::from_be_bytes([header_bytes[2], header_bytes[3]]);
    assert_ne!(
        postsize, 0xFFFF,
        "no reply this test reads uses an extended header"
    );
    let hdr = CaHeader::from_bytes(&header_bytes)?;
    let body_len = hdr.actual_postsize();
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        stream
            .read_exact(&mut body)
            .map_err(epics_base_rs::error::CaError::Io)?;
    }
    Ok((hdr, body))
}

/// End-to-end: a raw CA client drives a `WRITE_NOTIFY` that forces an async
/// `calcout` to process; the deferred completion runs on the std-thread
/// background executor (`cbMedium`), which unblocks the parked server thread to
/// send the reply. Asserts both the round-trip AND the completing thread.
///
/// `#[serial(epics_env)]` for parity with the other CA server tests (this one
/// touches no `EPICS_CA_*` env, but shares the CA test-serialisation group).
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn async_write_notify_completion_runs_on_background_executor() {
    // Eagerly start the process-global background executor — C `callbackInit`
    // parity, and under `exec_backend` this is the facility the seam routes to.
    // Reachable here only because the `rtems-exec-model` feature is on.
    epics_base_rs::runtime::task::background_init();

    let capture = ThreadCapture::default();

    // (1) Real async IOC.
    let db = build_async_db(capture.clone()).await;

    // (2) BlockingCaServer front-end on ephemeral 127.0.0.1 ports (never 5064).
    let acf = epics_base_rs::server::access_security::new_acf_cell(None);
    let server = Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), acf).unwrap());
    let tcp_port = server.tcp_port();

    let udp_sock = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let srv_tcp = server.clone();
    let tcp_thread = thread::spawn(move || srv_tcp.serve());
    let srv_udp = server.clone();
    let udp_thread = thread::spawn(move || srv_udp.serve_udp_search(udp_sock));

    // (3) Raw CA client on a real socket. Blocking, with a read timeout so a
    //     stalled completion surfaces as a test failure, not an infinite hang.
    let mut stream = TcpStream::connect(("127.0.0.1", tcp_port)).expect("connect to CA server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // Handshake: VERSION (carrying our minor version), CLIENT_NAME, HOST_NAME.
    let mut version = CaHeader::new(CA_PROTO_VERSION);
    version.count = CA_MINOR_VERSION; // server reads client_minor_version = m_count
    stream.write_all(&version.to_bytes()).unwrap();
    stream
        .write_all(&identity_frame(CA_PROTO_CLIENT_NAME, "rtems-exec-e2e"))
        .unwrap();
    stream
        .write_all(&identity_frame(CA_PROTO_HOST_NAME, "localhost"))
        .unwrap();

    // CREATE_CHAN to the calcout's PROC field: cid = our client CID, available =
    // our minor version (the server's upgrade-only version negotiation).
    let client_cid: u32 = 1;
    let chan_name = "RTEMS:NOTIFY.PROC";
    let name_payload = pad_string(chan_name);
    let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
    create.cid = client_cid;
    create.available = CA_MINOR_VERSION as u32;
    create.postsize = name_payload.len() as u16;
    let mut create_frame = create.to_bytes().to_vec();
    create_frame.extend_from_slice(&name_payload);
    stream.write_all(&create_frame).unwrap();

    // Read replies until our channel is created; capture the server SID and the
    // negotiated server minor version (for the put frame's header form).
    let mut sid = None;
    let mut server_minor = CA_MINOR_VERSION;
    while sid.is_none() {
        let (hdr, _body) = read_frame(&mut stream).expect("read create-chan reply");
        match hdr.cmmd {
            CA_PROTO_VERSION => server_minor = hdr.count,
            CA_PROTO_ACCESS_RIGHTS => {}
            CA_PROTO_CREATE_CHAN if hdr.cid == client_cid => sid = Some(hdr.available),
            CA_PROTO_CREATE_CH_FAIL => panic!("server rejected CREATE_CHAN for {chan_name}"),
            _ => {}
        }
    }
    let sid = sid.expect("server assigned a channel SID");

    // Clear the capture immediately before the trigger, so any processing during
    // iocInit is excluded and only the completion-cycle capture remains.
    capture.names.lock().unwrap().clear();

    // WRITE_NOTIFY to PROC (value 1, DBR_LONG). Any write to PROC force-processes
    // the record; because the record is async (ODLY defer) the WRITE_NOTIFY reply
    // is withheld until the deferred continuation clears PACT.
    let ioid: u32 = 0x5A5A;
    let put_payload = 1i32.to_be_bytes().to_vec();
    let write_notify = build_put_frame(
        CA_PROTO_WRITE_NOTIFY,
        sid,
        DBR_LONG,
        1,
        Some(ioid),
        put_payload,
        server_minor,
    )
    .expect("build WRITE_NOTIFY frame");
    stream.write_all(&write_notify).unwrap();

    // Await the WRITE_NOTIFY reply for our ioid. Arriving at all proves the async
    // completion drove the parked server thread to send it; cid carries the ECA
    // status.
    let mut status = None;
    while status.is_none() {
        let (hdr, _body) = read_frame(&mut stream).expect("read WRITE_NOTIFY reply");
        if hdr.cmmd == CA_PROTO_WRITE_NOTIFY && hdr.available == ioid {
            status = Some(hdr.cid);
        }
    }
    assert_eq!(
        status,
        Some(ECA_NORMAL),
        "WRITE_NOTIFY round-tripped to completion with ECA_NORMAL"
    );

    // The core assertion: the deferred completion — the calcout ODLY
    // continuation that wrote the OUT link and thereby processed RTEMS:CAP — ran
    // on the std-thread background executor's cbMedium worker, NOT a tokio
    // worker. This is what proves the RTEMS async execution model on the host.
    let captured = capture.names.lock().unwrap().clone();
    assert!(
        !captured.is_empty(),
        "the OUT-target record must have processed during the async completion"
    );
    for name in &captured {
        assert_eq!(
            name, "cbMedium",
            "async WRITE_NOTIFY completion must run on the background-executor \
             cbMedium worker, not a tokio runtime worker; captured threads: {captured:?}"
        );
    }

    // Teardown.
    drop(stream);
    server.shutdown();
    tcp_thread.join().expect("accept thread joins");
    udp_thread
        .join()
        .expect("udp thread joins")
        .expect("udp responder exits cleanly");
}
