//! Subscription callbacks run in wire order across subscriptions, and a put
//! resolves after the callbacks for the updates read before its reply.
//!
//! libca runs a circuit's callbacks on its receive thread in the order the
//! frames arrive (`tcpiiu.cpp:1263`), so a caller that puts B after A sees
//! A's callback before B's put returns. A `MonitorHandle` per subscription
//! is drained by its own task, which keeps each subscription's order but
//! loses the order between them.

#![cfg(tokio_backend)]
#![cfg(all(feature = "client-core", not(epics_embedded_target)))]

use std::sync::{Arc, Mutex};

use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient, CallbackMonitor};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const PUTS: u32 = 200;
const DBE_VALUE: u16 = 1;

type Log = Arc<Mutex<Vec<(char, f64)>>>;

/// SAFETY: the test is `#[serial]` and sets the env before `CaClient::new`
/// snapshots it.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

async fn logged(ch: &CaChannel, name: char, log: &Log) -> CallbackMonitor {
    let log = log.clone();
    ch.subscribe_callback(DBE_VALUE, move |update| {
        let value = update.expect("update").value.to_f64().expect("scalar");
        log.lock().unwrap().push((name, value));
    })
    .await
    .expect("subscribe")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn subscription_callbacks_run_in_wire_order() {
    let server = CaServer::builder()
        .port(0)
        .record("ORDER:A", AoRecord::new(0.0))
        .record("ORDER:B", AoRecord::new(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let a = client.create_channel("ORDER:A");
    let b = client.create_channel("ORDER:B");
    for ch in [&a, &b] {
        ch.wait_connected(fact_budget::FACT_BUDGET)
            .await
            .expect("connect");
    }
    let log = Log::default();
    let _a = logged(&a, 'A', &log).await;
    let _b = logged(&b, 'B', &log).await;
    tokio::time::timeout(fact_budget::FACT_BUDGET, async {
        while log.lock().unwrap().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("initial values");
    log.lock().unwrap().clear();

    let mut expected = Vec::new();
    for put in 1..=PUTS {
        let value = f64::from(put) + 0.5;
        a.put_nowait(&EpicsValue::Double(value))
            .await
            .expect("put A");
        b.put_with_timeout(&EpicsValue::Double(value), fact_budget::FACT_BUDGET)
            .await
            .expect("put B");
        expected.extend([('A', value), ('B', value)]);
        assert_eq!(
            *log.lock().unwrap(),
            expected,
            "put {put}: B's put resolved before the callbacks ran in wire order"
        );
    }
}
