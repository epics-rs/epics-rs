//! The time a `ts`-filtered CA get reports. C `read_reply` reads value and
//! time from the read log (`db_access.c:158-168`): `{"ts":{}}` runs on the
//! pre chain and stamps that log with the clock (`ts.c:138-163`), and a value
//! mode reads the record's time the log was created with
//! (`dbEvent.c:704-713`). The port ran the pre chain on a log of the value
//! alone and copied only the value back, so a `{"ts":{}}` get reported the
//! record's time.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_base_rs::types::{DBR_TIME_DOUBLE, WallTime};
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use std::time::Duration;

const DB: &str = r#"record(ai, "T") { field(VAL, "1") }"#;

/// A stamp no clock in this process produces.
const STAMP: Duration = Duration::new(1_000_000_000, 333_333_333);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ts_get_reports_the_time_its_read_log_holds() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &Default::default())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    // `T` is never processed, so the planted stamp stays put.
    server
        .database()
        .get_record("T")
        .unwrap()
        .write()
        .common
        .time = WallTime::UNIX_EPOCH + STAMP;
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });
    // SAFETY: the only test in this binary; set before `CaClient::new()`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");
    let get = |name: &'static str| {
        let ch = client.create_channel(name);
        async move {
            ch.wait_connected(fact_budget::FACT_BUDGET)
                .await
                .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
            ch.get_with_dbr_type(DBR_TIME_DOUBLE, 0u32, fact_budget::FACT_BUDGET)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e:?}"))
        }
    };

    let before = WallTime::now();
    let generated = get(r#"T.VAL{"ts":{}}"#).await;
    assert!(
        generated.timestamp >= before,
        "the read's clock, not the record's: {:?}",
        generated.timestamp
    );

    let nsec = get(r#"T.VAL{"ts":{"num":"nsec"}}"#).await;
    assert_eq!(nsec.value, EpicsValue::Double(STAMP.subsec_nanos().into()));
    assert_eq!(nsec.timestamp, WallTime::UNIX_EPOCH + STAMP);
}
