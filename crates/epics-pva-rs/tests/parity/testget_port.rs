//! Port of pvxs `test/testget.cpp::testConnector`.
//!
//! pvxs verifies that connect()/onConnect()/onDisconnect() callbacks
//! fire correctly across server start/stop. We test the corresponding
//! `PvaClient::connect(...).on_connect(...).exec()` pattern against
//! our own SharedSource server.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::nt::NTScalar;
use epics_pva_rs::pvdata::ScalarType;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig, SharedPV, SharedSource};

#[tokio::test]
async fn pvxs_connect_onconnect_fires_after_server_start() {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
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

    let server = PvaServer::start(src, cfg).expect("test server must start");
    let port = server.report().tcp_port;
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
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}
