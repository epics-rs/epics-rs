//! Port of pvxs `test/testget.cpp::testConnector`.
//!
//! pvxs verifies that connect()/onConnect()/onDisconnect() callbacks
//! fire correctly across server start/stop. We test the corresponding
//! `PvaClient::connect(...).on_connect(...).exec()` pattern against
//! our own SharedSource server.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::nt::NTScalar;
use epics_pva_rs::pvdata::ScalarType;
use epics_pva_rs::server_native::{PvaServerConfig, SharedPV, SharedSource, run_pva_server};

static NEXT_PORT: AtomicU16 = AtomicU16::new(34000);
fn alloc_port_pair() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    (base, base + 1)
}

#[tokio::test]
async fn pvxs_connect_onconnect_fires_after_server_start() {
    let (port, udp) = alloc_port_pair();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };

    let pv = SharedPV::new();
    pv.open(
        NTScalar::new(ScalarType::Int).build(),
        NTScalar::new(ScalarType::Int).create(),
    )
    .unwrap();
    let src = Arc::new(SharedSource::new());
    src.add("mailbox", pv);

    let h = tokio::spawn(async move {
        let _ = run_pva_server(src, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let connected = Arc::new(AtomicUsize::new(0));
    let disconnected = Arc::new(AtomicUsize::new(0));
    let c1 = connected.clone();
    let d1 = disconnected.clone();

    let handle = client
        .connect("mailbox")
        .on_connect(move || {
            c1.fetch_add(1, Ordering::SeqCst);
        })
        .on_disconnect(move || {
            d1.fetch_add(1, Ordering::SeqCst);
        })
        .exec()
        .await
        .expect("connect builder");

    // Drive a pvget to force the channel into Active.
    let _ = tokio::time::timeout(Duration::from_secs(3), client.pvget("mailbox"))
        .await
        .expect("pvget timeout")
        .expect("pvget");

    // Give the watcher task a moment to observe the state transition.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        connected.load(Ordering::SeqCst) >= 1,
        "expected at least one onConnect, got {}",
        connected.load(Ordering::SeqCst)
    );

    drop(handle);
    h.abort();
}
