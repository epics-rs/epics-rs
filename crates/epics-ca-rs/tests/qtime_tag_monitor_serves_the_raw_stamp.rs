//! A CA monitor on a record with `info(Q:time:tag, "nsec:lsb:N")` serves
//! the record's raw stamp: `db_create_field_log` copies `prec->time`
//! (`dbEvent.c:710`) and rsrv never splits it. The split is pvxs's alone
//! (`iocsource.cpp:239-248`).

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_base_rs::types::WallTime;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;

const DB: &str = r#"
record(ai, "QT") { info(Q:time:tag, "nsec:lsb:20") }
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_qtime_tag_record_monitor_update_carries_the_raw_stamp() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &Default::default())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let db = server.database().clone();
    let _h = tokio::spawn(async move { server.run().await });
    // SAFETY: the only test in this binary; set before `CaClient::new()`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel("QT");
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("QT connects");
    let mut mon = ch.subscribe().await.expect("QT subscribes");
    tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
        .await
        .expect("initial event")
        .expect("monitor open")
        .expect("event");

    ch.put(&EpicsValue::Double(2.0)).await.expect("put QT");
    let update = loop {
        let snap = tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
            .await
            .expect("an update after the put")
            .expect("monitor open")
            .expect("event");
        if snap.value == EpicsValue::Double(2.0) {
            break snap;
        }
    };

    let rec = db.get_record("QT").expect("QT loaded");
    let raw = WallTime::from(rec.read().common.time);
    assert_eq!(update.timestamp, raw);
}
