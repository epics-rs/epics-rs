//! F11: end-to-end coverage for the PVA `PUT_GET` (cmd 12) and
//! `PROCESS` (cmd 16) operations.
//!
//! - `PUT_GET` round trip: the client PUTs a value and gets the
//!   (server-side post-processed) value back in one operation. The
//!   test source doubles the value on every put, so the readback
//!   proves the GET leg sees the post-put state, not the wire input.
//! - `PROCESS` triggers a server-side processing hook: the test
//!   source increments a counter inside `process()` and a subsequent
//!   GET observes the incremented value.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, PvaServer, PvaServerConfig};

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
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
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
    fn process(&self, _: &str) -> impl std::future::Future<Output = Result<(), String>> + Send {
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
    let server = PvaServer::start(Arc::new(src.clone()), cfg);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);
    let (_intro, value) = tokio::time::timeout(Duration::from_secs(3), client.pvput_get("dut", "21"))
        .await
        .expect("pvput_get timed out")
        .expect("pvput_get failed");

    assert_eq!(
        int_value(&value),
        42,
        "PUT_GET readback should be the doubled (post-put) value"
    );
    // The source's stored value confirms the PUT leg ran.
    assert_eq!(*src.value.lock(), 42, "source should hold the doubled value");

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
    let server = PvaServer::start(Arc::new(src.clone()), cfg);
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
    let server = PvaServer::start(Arc::new(src.clone()), cfg);
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
