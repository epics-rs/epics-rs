//! A put-callback resolves after the subscription update it posted is in
//! the subscription.
//!
//! The server sends a record's WRITE_NOTIFY behind the monitor updates the
//! put posted, and libca runs both callbacks on the receive thread in that
//! order (`tcpiiu.cpp:1263`), so a caller that reads its monitor after the
//! put returns sees the value it wrote. The client answered the put on the
//! read loop while the update still crossed the coordinator, and the
//! monitor sometimes still held the previous value.

#![cfg(tokio_backend)]
#![cfg(all(feature = "client-core", not(epics_embedded_target)))]

use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaClient, MonitorHandle};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const PV: &str = "ORDER:X";
const PUTS: u32 = 200;

/// SAFETY: the test is `#[serial]` and sets the env before `CaClient::new`
/// snapshots it.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// The latest value the subscription holds now, without waiting.
async fn latest(monitor: &mut MonitorHandle) -> Option<f64> {
    let mut latest = None;
    loop {
        let next = tokio::select! {
            biased;
            next = monitor.recv() => next,
            () = std::future::ready(()) => return latest,
        };
        let snapshot = next.expect("open").expect("value");
        latest = snapshot.value.to_f64();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_put_callback_resolves_after_its_update() {
    let server = CaServer::builder()
        .port(0)
        .record(PV, AoRecord::new(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel(PV);
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("connect");
    let mut monitor = ch.subscribe().await.expect("subscribe");
    tokio::time::timeout(fact_budget::FACT_BUDGET, monitor.recv())
        .await
        .expect("initial value")
        .expect("open")
        .expect("value");

    for put in 1..=PUTS {
        let value = f64::from(put) + 0.5;
        ch.put_with_timeout(&EpicsValue::Double(value), fact_budget::FACT_BUDGET)
            .await
            .expect("put");
        assert_eq!(
            latest(&mut monitor).await,
            Some(value),
            "put {put} resolved before its update reached the subscription"
        );
    }
}
