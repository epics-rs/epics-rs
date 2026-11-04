//! A subscribe cancelled before it hands back its `MonitorHandle` is
//! cancelled on the server once its first event arrives.
//!
//! With no handle there is no drop to send the `Unsubscribe`; the closed
//! consumer channel is all the coordinator has. It used to keep the
//! subscription and log "monitor dropped (consumer handle closed)" for every
//! event the server went on sending.

#![cfg(tokio_backend)]
#![cfg(all(feature = "client-core", not(epics_embedded_target)))]

use std::task::Poll;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const PV: &str = "GONE:X";

/// SAFETY: the test is `#[serial]` and sets the env before `CaClient::new`
/// snapshots it.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

fn monitors_on_val(db: &PvDatabase) -> usize {
    db.get_record(PV)
        .expect("record")
        .read()
        .subscriber_count("VAL")
}

#[tokio::test]
#[serial]
async fn a_cancelled_subscribe_is_cancelled_on_the_server() {
    let server = CaServer::builder()
        .port(0)
        .record(PV, AoRecord::new(1.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let db = server.database().clone();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel(PV);
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("connect");

    // One poll sends the subscribe; on this current-thread runtime the
    // coordinator cannot reply before the future is dropped.
    {
        let mut sub = std::pin::pin!(ch.subscribe());
        let first = std::future::poll_fn(|cx| Poll::Ready(sub.as_mut().poll(cx))).await;
        assert!(first.is_pending());
    }

    // Its EVENT_ADD went out first, so once this one's initial value is in,
    // the server holds both.
    let mut live = ch.subscribe().await.expect("live subscribe");
    tokio::time::timeout(fact_budget::FACT_BUDGET, live.recv())
        .await
        .expect("live initial value")
        .expect("open")
        .expect("value");

    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while monitors_on_val(&db) != 1 {
        assert!(
            Instant::now() < deadline,
            "the cancelled subscribe is still on the server"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
