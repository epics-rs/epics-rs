//! end-to-end coverage for the PVA `PUT_GET` (cmd 12) and
//! `PROCESS` (cmd 16) operations.
//!
//! - `PUT_GET` round trip: the client PUTs a value and gets the
//!   (server-side post-processed) value back in one operation. The
//!   test source doubles the value on every put, so the readback
//!   proves the GET leg sees the post-put state, not the wire input.
//! - `PROCESS` triggers a server-side processing hook: the test
//!   source increments a counter inside `process()` and a subsequent
//!   GET observes the incremented value.
//! - ACF-deny coverage: a peer with READ-only access (no WRITE rule)
//!   issuing PUT_GET or PROCESS — both are WRITE-class operations —
//!   is rejected with an error status and the source's mutating
//!   hooks never run.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use epics_base_rs::server::access_security::{AccessGate, AsgAslResolver, parse_acf};
use epics_pva_rs::PvaError;
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

/// A writable NTScalar source. `put_value` stores **twice** the
/// incoming value (a stand-in for a record that post-processes on
/// write), so a PUT_GET readback that returns the doubled value
/// proves the GET leg ran after the PUT leg. `process()` increments
/// the stored value by 100, simulating a record processing chain.
#[derive(Clone)]
struct DoublingSource {
    value: Arc<Mutex<i32>>,
    process_count: Arc<AtomicU32>,
}

impl DoublingSource {
    fn new() -> Self {
        Self {
            value: Arc::new(Mutex::new(1)),
            process_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

fn nt_scalar_int_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
    }
}

impl ChannelSource for DoublingSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        async { vec!["dut".into()] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    fn get_introspection(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        async { Some(nt_scalar_int_desc()) }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let v = *self.value.lock();
        async move {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let store = self.value.clone();
        async move {
            // Extract the `.value` int from the incoming structure and
            // store twice it — the post-processing stand-in.
            let incoming = match &value {
                PvField::Structure(s) => s.fields.iter().find_map(|(k, v)| {
                    (k == "value").then_some(v).and_then(|v| match v {
                        PvField::Scalar(ScalarValue::Int(i)) => Some(*i),
                        _ => None,
                    })
                }),
                PvField::Scalar(ScalarValue::Int(i)) => Some(*i),
                _ => None,
            }
            .ok_or_else(|| "put value has no int .value field".to_string())?;
            *store.lock() = incoming * 2;
            Ok(())
        }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { true }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        async { None }
    }
    fn process(&self, _: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let store = self.value.clone();
        let count = self.process_count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            *store.lock() += 100;
            Ok(())
        }
    }
}

static NEXT_PORT: AtomicU16 = AtomicU16::new(49200);
fn alloc_port() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    (base, base + 1)
}

fn client_to(port: u16) -> PvaClient {
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build()
}

fn int_value(v: &PvField) -> i32 {
    match v {
        PvField::Structure(s) => s
            .fields
            .iter()
            .find_map(|(k, f)| {
                (k == "value").then_some(f).and_then(|f| match f {
                    PvField::Scalar(ScalarValue::Int(i)) => Some(*i),
                    _ => None,
                })
            })
            .expect("no int .value field"),
        PvField::Scalar(ScalarValue::Int(i)) => *i,
        other => panic!("unexpected PvField shape: {other:?}"),
    }
}

/// PUT_GET round trip: PUT 21, source stores 42 (doubled), readback
/// returns 42 — proving the GET leg observed the post-put state.
#[tokio::test]
async fn put_get_round_trip() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let src = DoublingSource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);
    let (_intro, value) =
        tokio::time::timeout(Duration::from_secs(3), client.pvput_get("dut", "21"))
            .await
            .expect("pvput_get timed out")
            .expect("pvput_get failed");

    assert_eq!(
        int_value(&value),
        42,
        "PUT_GET readback should be the doubled (post-put) value"
    );
    // The source's stored value confirms the PUT leg ran.
    assert_eq!(
        *src.value.lock(),
        42,
        "source should hold the doubled value"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// PUT_GET then a plain GET observe the same post-put value — the
/// PUT_GET op leaves the channel in a consistent state.
#[tokio::test]
async fn put_get_then_get_consistent() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let src = DoublingSource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);
    let (_intro, pg_value) =
        tokio::time::timeout(Duration::from_secs(3), client.pvput_get("dut", "50"))
            .await
            .expect("pvput_get timed out")
            .expect("pvput_get failed");
    assert_eq!(int_value(&pg_value), 100);

    let got = tokio::time::timeout(Duration::from_secs(3), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    assert_eq!(
        int_value(&got),
        100,
        "a follow-up GET must see the same value PUT_GET returned"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// getGet (`QOS_GET`, 0x40) and getPut (`QOS_GET_PUT`, 0x80) — the two
/// read-only ChannelPutGet subcommands — must return the current value
/// WITHOUT running a put leg. Before the fix the server unconditionally
/// decoded a put BitSet/value for every PUT_GET data frame, so these
/// payload-less frames failed to decode instead of returning data.
#[tokio::test]
async fn get_get_and_get_put_read_without_writing() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let src = DoublingSource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);

    // getGet on the fresh source: returns the current value (1) and runs
    // no put leg, so the stored value is unchanged.
    let (_intro, gg) = tokio::time::timeout(Duration::from_secs(3), client.pvget_get("dut"))
        .await
        .expect("pvget_get timed out")
        .expect("pvget_get failed");
    assert_eq!(int_value(&gg), 1, "getGet returns the current value");
    assert_eq!(
        *src.value.lock(),
        1,
        "getGet must not write (value unchanged)"
    );

    // getPut likewise reads the put-side data and never writes.
    let (_intro, gp) = tokio::time::timeout(Duration::from_secs(3), client.pvget_put("dut"))
        .await
        .expect("pvget_put timed out")
        .expect("pvget_put failed");
    assert_eq!(int_value(&gp), 1, "getPut returns the current value");
    assert_eq!(
        *src.value.lock(),
        1,
        "getPut must not write (value unchanged)"
    );

    // After a real putGet (PUT 21 → source stores doubled 42), getGet
    // reflects the stored value and does NOT double again — proof its
    // read-only leg never invoked the doubling put hook.
    let (_i, pg) = tokio::time::timeout(Duration::from_secs(3), client.pvput_get("dut", "21"))
        .await
        .expect("pvput_get timed out")
        .expect("pvput_get failed");
    assert_eq!(int_value(&pg), 42);
    let (_i, gg2) = tokio::time::timeout(Duration::from_secs(3), client.pvget_get("dut"))
        .await
        .expect("pvget_get timed out")
        .expect("pvget_get failed");
    assert_eq!(
        int_value(&gg2),
        42,
        "getGet after putGet reflects the stored value, not a re-doubled one"
    );
    assert_eq!(
        *src.value.lock(),
        42,
        "getGet must not mutate the stored value"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// PUT_GET (cmd 12) is a Rust extension. With `serve_put_get`
/// disabled (the strict pvxs-compat posture) the server rejects every
/// cmd-12 frame — putGet, getGet, getPut — with a deterministic error
/// `Status` instead of running the round trip. The client fails fast
/// with a server error, NOT a `PvaError::Timeout`: a gated-off server
/// replies immediately, so the op never waits out its `op_timeout` the
/// way pvxs's silent `handle_PUT_GET` stub would force it to. The gate
/// fires before any put leg, so the source is never mutated.
#[tokio::test]
async fn put_get_rejected_when_serve_put_get_disabled() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        serve_put_get: false,
        ..Default::default()
    };
    let src = DoublingSource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);

    // putGet (write leg + readback).
    let pg = tokio::time::timeout(Duration::from_secs(3), client.pvput_get("dut", "21"))
        .await
        .expect("pvput_get hung instead of returning a deterministic error");
    let pg_err = pg.expect_err("putGet must be rejected when serve_put_get=false");
    assert!(
        !matches!(pg_err, PvaError::Timeout),
        "putGet rejection must be a deterministic server error, not a timeout: {pg_err:?}"
    );

    // getGet (read-only cmd-12 subcommand) routes through the same gate.
    let gg = tokio::time::timeout(Duration::from_secs(3), client.pvget_get("dut"))
        .await
        .expect("pvget_get hung instead of returning a deterministic error");
    let gg_err = gg.expect_err("getGet must be rejected when serve_put_get=false");
    assert!(
        !matches!(gg_err, PvaError::Timeout),
        "getGet rejection must be a deterministic server error, not a timeout: {gg_err:?}"
    );

    // getPut (read-only cmd-12 subcommand) — same gate.
    let gp = tokio::time::timeout(Duration::from_secs(3), client.pvget_put("dut"))
        .await
        .expect("pvget_put hung instead of returning a deterministic error");
    let gp_err = gp.expect_err("getPut must be rejected when serve_put_get=false");
    assert!(
        !matches!(gp_err, PvaError::Timeout),
        "getPut rejection must be a deterministic server error, not a timeout: {gp_err:?}"
    );

    assert_eq!(
        *src.value.lock(),
        1,
        "a gated-off PUT_GET must not write the source value"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// PROCESS triggers the server-side processing hook: the counter
/// increments and the stored value gains 100.
#[tokio::test]
async fn process_triggers_hook() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let src = DoublingSource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);

    // Baseline: value=1, process_count=0.
    let before = tokio::time::timeout(Duration::from_secs(3), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    assert_eq!(int_value(&before), 1);
    assert_eq!(src.process_count.load(Ordering::SeqCst), 0);

    // PROCESS — no value transferred, but the hook runs.
    tokio::time::timeout(Duration::from_secs(3), client.pvprocess("dut"))
        .await
        .expect("pvprocess timed out")
        .expect("pvprocess failed");
    assert_eq!(
        src.process_count.load(Ordering::SeqCst),
        1,
        "PROCESS should fire the source's process hook exactly once"
    );

    // The hook added 100; a GET observes it.
    let after = tokio::time::timeout(Duration::from_secs(3), client.pvget("dut"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    assert_eq!(
        int_value(&after),
        101,
        "GET after PROCESS must see the hook's effect"
    );

    // A second PROCESS fires again.
    tokio::time::timeout(Duration::from_secs(3), client.pvprocess("dut"))
        .await
        .expect("pvprocess timed out")
        .expect("pvprocess failed");
    assert_eq!(src.process_count.load(Ordering::SeqCst), 2);

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

// ---------------------------------------------------------------------
// ACF-deny negative coverage for PUT_GET and PROCESS.
//
// Both PUT_GET (its PUT leg) and PROCESS are WRITE-class operations.
// `DenySource` installs a `Required` AccessGate whose ACF grants only
// READ — every peer can read, none may write. A client issuing
// PUT_GET or PROCESS must be rejected with an error status, and the
// source's mutating hooks (`put_value`, `process`) must never run.
// ---------------------------------------------------------------------

/// Like `DoublingSource` but ACF-gated: a `Required` gate with an
/// ASG that has a READ rule only — no WRITE rule, so `put_value_checked`
/// and `process_checked` deny every peer. `put_hits` / `process_hits`
/// count whether the mutating hooks ever ran (they must not).
#[derive(Clone)]
struct DenySource {
    value: Arc<Mutex<i32>>,
    put_hits: Arc<AtomicU32>,
    process_hits: Arc<AtomicU32>,
    gate: Arc<AccessGate>,
}

impl DenySource {
    fn new() -> Self {
        // READ-only ASG: every peer reads, none writes.
        let cfg = parse_acf("ASG(DEFAULT) {\n    RULE(1, READ)\n}\n").expect("acf parse");
        let cell = Arc::new(tokio::sync::RwLock::new(Some(cfg)));
        let resolver: AsgAslResolver =
            Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        Self {
            value: Arc::new(Mutex::new(1)),
            put_hits: Arc::new(AtomicU32::new(0)),
            process_hits: Arc::new(AtomicU32::new(0)),
            gate: Arc::new(AccessGate::required(cell, resolver)),
        }
    }
}

impl ChannelSource for DenySource {
    fn access(&self) -> &AccessGate {
        &self.gate
    }
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        async { vec!["dut".into()] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    fn get_introspection(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        async { Some(nt_scalar_int_desc()) }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let v = *self.value.lock();
        async move {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        _value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        // Records whether the mutating hook was reached. The ACF gate
        // must block this before it runs — the count must stay 0.
        self.put_hits.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { true }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        async { None }
    }
    fn process(&self, _: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        self.process_hits.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    }
}

/// an ACF-denied peer issuing PUT_GET is rejected — the PUT leg
/// is WRITE-class, the gate denies it, and `pvput_get` returns an
/// error. The source's `put_value` hook never runs.
#[tokio::test]
async fn put_get_denied_for_read_only_peer() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let src = DenySource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .user("intruder")
        .host("h.example")
        .build();

    let result = tokio::time::timeout(Duration::from_secs(3), client.pvput_get("dut", "21"))
        .await
        .expect("pvput_get timed out");

    assert!(
        result.is_err(),
        "PUT_GET from a READ-only peer must be rejected, got Ok: {result:?}"
    );
    assert_eq!(
        src.put_hits.load(Ordering::SeqCst),
        0,
        "put_value hook must NOT run when the ACF gate denies WRITE"
    );
    assert_eq!(
        *src.value.lock(),
        1,
        "denied PUT_GET must leave the source value untouched"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// an ACF-denied peer issuing PROCESS is rejected — PROCESS is
/// WRITE-class, the gate denies it, and `pvprocess` returns an error.
/// The source's `process` hook never runs.
#[tokio::test]
async fn process_denied_for_read_only_peer() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let src = DenySource::new();
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .user("intruder")
        .host("h.example")
        .build();

    let result = tokio::time::timeout(Duration::from_secs(3), client.pvprocess("dut"))
        .await
        .expect("pvprocess timed out");

    assert!(
        result.is_err(),
        "PROCESS from a READ-only peer must be rejected, got Ok: {result:?}"
    );
    assert_eq!(
        src.process_hits.load(Ordering::SeqCst),
        0,
        "process hook must NOT run when the ACF gate denies WRITE"
    );

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}
