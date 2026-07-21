//! End-to-end mDNS smoke test.
//!
//! Spawns a Rust IOC with mDNS announce + a Rust client with mDNS
//! discovery, and verifies the client picks up the IOC's address
//! without any explicit `EPICS_CA_ADDR_LIST`.
//!
//! Built only with the `discovery` feature.

#![cfg(feature = "discovery")]

// RTEMS-EXEC-MODEL-ALLOW(1): not built by default - this file is behind the `discovery` feature.

use std::sync::Arc;
use std::time::Duration;

use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::discovery::DiscoveryConfig;
use epics_ca_rs::server::CaServer;

/// Smoke test that may be flaky on hosts without working multicast
/// (some CI containers) — annotated `#[ignore]` so it's opt-in via
/// `cargo test -- --ignored`. Real LAN hosts run it cleanly.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mdns_server_announce_to_client_discover() {
    // Build an IOC with a unique mDNS instance name to avoid colliding
    // with any other epics-ca-rs servers running on the developer's
    // machine.
    let instance = format!(
        "ca-rs-mdns-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
    );
    let server = CaServer::builder()
        .pv("MDNS:VAL", epics_base_rs::types::EpicsValue::Long(7777))
        .port(0)
        .announce_mdns(&instance)
        .announce_txt("version", "test")
        .build()
        .await
        .expect("build");

    let port = server.udp_port();
    let server_arc = Arc::new(server);
    let server_clone = server_arc.clone();
    let server_task = tokio::spawn(async move {
        let _ = server_clone.run().await;
    });

    // Wait for mDNS announce to propagate.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Client: ONLY use mDNS discovery, no EPICS_CA_ADDR_LIST.
    unsafe {
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::remove_var("EPICS_CA_ADDR_LIST");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let client = CaClient::new_with_config(CaClientConfig {
        discovery: Some(DiscoveryConfig::Mdns),
        ..Default::default()
    })
    .await
    .expect("client");

    let ch = client.create_channel("MDNS:VAL");
    ch.wait_connected(Duration::from_secs(8))
        .await
        .expect("connect via mDNS-discovered IOC");

    let (_, value) = ch
        .get_with_timeout(Duration::from_secs(3))
        .await
        .expect("read");
    assert_eq!(value.to_f64().unwrap_or(0.0) as i64, 7777);

    server_task.abort();
}
