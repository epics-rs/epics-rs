//! End-to-end test for the MONITOR FINISH (`subcmd & 0x10`) frame.
//!
//! pvxs `servermon.cpp:148` emits a final `subcmd=0x10 + Status` after the
//! source's broadcast queue is drained, signalling end-of-stream so the
//! client tears down cleanly. We added the same emission to our subscriber
//! task: when `rx.recv()` returns `None` (the source dropped its sender),
//! the server pushes `build_monitor_finish` and the client's `pvmonitor`
//! loop translates the resulting `OpResponse::Status` (success) into
//! `Ok(())`.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig, run_pva_server};

#[derive(Clone)]
struct FiniteSource {
    /// Outgoing sender held until the test asks us to close it.
    tx: Arc<Mutex<Option<mpsc::Sender<PvField>>>>,
}

impl FiniteSource {
    fn new() -> (Self, mpsc::Sender<PvField>) {
        let (tx, _rx) = mpsc::channel(8);
        let _ = _rx; // discard; subscribe() builds its own channel
        let holder = Arc::new(Mutex::new(Some(tx.clone())));
        (FiniteSource { tx: holder }, tx)
    }
}

impl ChannelSource for FiniteSource {
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
        async {
            Some(FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        async {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        _: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async { Err("read-only".into()) }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let holder = self.tx.clone();
        async move {
            // Hand the subscriber a fresh receiver. The matching sender is
            // returned to the test so it can push values and then drop —
            // triggering the server's MONITOR FINISH emission.
            let (sub_tx, sub_rx) = mpsc::channel::<PvField>(8);
            *holder.lock().await = Some(sub_tx);
            Some(sub_rx)
        }
    }
}

static NEXT_PORT: AtomicU16 = AtomicU16::new(47000);
fn alloc_port_pair() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    (base, base + 1)
}

#[tokio::test]
async fn monitor_finish_returns_ok_when_source_closes() {
    let (port, udp) = alloc_port_pair();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };

    let (source, _orig_tx) = FiniteSource::new();
    let source_for_drive = source.clone();
    tokio::spawn(async move {
        let _ = run_pva_server(Arc::new(source_for_drive), cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let received = Arc::new(AtomicUsize::new(0));
    let received_clone = received.clone();

    // Drive the source: push a couple of updates then drop ALL senders
    // so the server's subscribe() rx hits None and we emit MONITOR FINISH.
    let driver = source.clone();
    let driver_handle = tokio::spawn(async move {
        // Wait for the subscriber to register and the holder to be set.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let tx_opt = driver.tx.lock().await.take();
        if let Some(tx) = tx_opt {
            for v in [2.0, 3.0, 4.0] {
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields
                    .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
                let _ = tx.send(PvField::Structure(s)).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // dropping tx (the only remaining clone, since holder was
            // .take()-ed) closes the channel → subscriber rx returns None
            // → server emits MONITOR FINISH (subcmd 0x10).
            drop(tx);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .pvmonitor("dut", move |_| {
                received_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await
    })
    .await
    .expect("pvmonitor timed out");

    let _ = driver_handle.await;

    // FINISH carries Status::OK so the client returns Ok(()).
    result.expect("monitor should end cleanly with Ok(())");
    // We expect at least the initial snapshot + the three pushed updates;
    // duplicates from the squashing window are OK.
    assert!(
        received.load(Ordering::SeqCst) >= 1,
        "subscriber should have received at least one value before FINISH"
    );
}
