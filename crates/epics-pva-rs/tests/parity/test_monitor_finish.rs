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

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

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
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let holder = self.tx.clone();
        async move {
            // Hand the subscriber a fresh receiver. The matching sender is
            // returned to the test so it can push values and then drop —
            // triggering the server's MONITOR FINISH emission.
            let (sub_tx, sub_rx) = mpsc::channel::<PvField>(8);
            *holder.lock().await = Some(sub_tx);
            Some(sub_rx.into())
        }
    }
}

#[tokio::test]
async fn monitor_finish_returns_ok_when_source_closes() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };

    let (source, _orig_tx) = FiniteSource::new();
    let source_for_drive = source.clone();
    let server = PvaServer::start(Arc::new(source_for_drive), cfg).expect("test server must start");
    let port = server.report().tcp_port;
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

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// Regression: a clean end-of-stream MONITOR FINISH must surface as
/// `MonitorEvent::Finished` even when the subscriber set
/// `mask_disconnected = true`.
///
/// pvxs gates only the `Disconnect()` push by `maskDiscon`
/// (clientmon.cpp:397); `Finished()` is pushed unconditionally on a
/// clean end-of-stream (clientmon.cpp:701-707). Before the fix our
/// `pvmonitor_events` `Ok(())` arm wrapped the `Finished` push in
/// `if !mask.mask_disconnected`, so a caller masking disconnects also
/// lost the legitimate end-of-stream signal.
#[tokio::test]
async fn monitor_finish_event_delivered_despite_mask_disconnected() {
    use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };

    let (source, _orig_tx) = FiniteSource::new();
    let source_for_drive = source.clone();
    let server = PvaServer::start(Arc::new(source_for_drive), cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let got_finished = Arc::new(AtomicBool::new(false));
    let got_disconnected = Arc::new(AtomicBool::new(false));
    let got_data = Arc::new(AtomicUsize::new(0));
    let finished_cb = got_finished.clone();
    let disconnected_cb = got_disconnected.clone();
    let data_cb = got_data.clone();

    // Drive the source the same way as the plain-FINISH test: push a few
    // updates, then drop ALL senders so the subscriber rx hits None and the
    // server emits MONITOR FINISH (subcmd 0x10).
    let driver = source.clone();
    let driver_handle = tokio::spawn(async move {
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
            drop(tx);
        }
    });

    // mask_disconnected=true is the regression condition: pre-fix this
    // also suppressed the clean-end Finished event.
    let mask = MonitorEventMask {
        mask_connected: true,
        mask_disconnected: true,
    };

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .pvmonitor_events("dut", None, mask, move |event| match event {
                MonitorEvent::Connected { .. } => {}
                MonitorEvent::Data { .. } => {
                    data_cb.fetch_add(1, Ordering::SeqCst);
                }
                MonitorEvent::Disconnected => {
                    disconnected_cb.store(true, Ordering::SeqCst);
                }
                MonitorEvent::Finished => {
                    finished_cb.store(true, Ordering::SeqCst);
                }
            })
            .await
    })
    .await
    .expect("pvmonitor_events timed out");

    let _ = driver_handle.await;

    result.expect("monitor should end cleanly with Ok(())");
    assert!(
        got_finished.load(Ordering::SeqCst),
        "Finished must be delivered on a clean end-of-stream even with mask_disconnected=true"
    );
    assert!(
        !got_disconnected.load(Ordering::SeqCst),
        "a clean end-of-stream is Finished, not Disconnected"
    );
    assert!(
        got_data.load(Ordering::SeqCst) >= 1,
        "subscriber should have received at least one value before FINISH"
    );
}
