//! A put through a remote CA link converts on the record thread into the
//! channel's native type, C `dbCaPutLinkCallback` (`dbCa.c:572-590`): a
//! string that is not a number is the status `dbPutLink` turns into
//! LINK/INVALID (`dbLink.c:443-446`), and the server gets no write.

#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_string_that_is_not_a_number_alarms_and_sends_nothing() {
    let server = CaServer::builder()
        .port(0)
        .pv("RMT", EpicsValue::Double(7.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let sdb = server.database().clone();
    let _server = tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"record(stringout, "SRC") { field(OUT, "RMT CA") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    db.initialize_link_locality().await;
    // The build ran with no `ca` lset: its `dbCaAddLink` pass is this one.
    db.setup_external_link_opens().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("RMT", fact_budget::FACT_BUDGET)
            .await,
        "the remote link connects"
    );

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::String("abc".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "SRC.STAT"), 14.0, "LINK");
    assert_eq!(num(&db, "SRC.SEVR"), 3.0, "INVALID");
    // A put that did go out would land well inside this.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(num(&sdb, "RMT"), 7.0, "the unconverted put wrote RMT");

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::String("12.5".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "SRC.SEVR"), 0.0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while num(&sdb, "RMT") != 12.5 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(num(&sdb, "RMT"), 12.5);
}
