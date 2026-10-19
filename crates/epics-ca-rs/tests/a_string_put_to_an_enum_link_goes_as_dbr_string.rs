//! A string put through a CA link to a `DBR_ENUM` channel goes out as one
//! `DBR_STRING`, and the server resolves the state name
//! (`dbCa.c:561-575`): a link client that converted it to the enum index
//! itself would have no state names to look it up in.

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

async fn until_rmt(sdb: &PvDatabase, want: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while num(sdb, "RMT") != want {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_state_name_put_through_a_ca_link_selects_that_state() {
    let server = CaServer::builder()
        .port(0)
        .db_string(
            r#"record(mbbo, "RMT") { field(ZRST, "zero") field(ONST, "one") field(TWST, "two") }"#,
            &HashMap::new(),
        )
        .unwrap()
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
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("RMT", fact_budget::FACT_BUDGET)
            .await,
        "the remote link connects"
    );

    // A plain put (`CA_PUT`): `dbPutField` without a callback.
    db.put_pv("SRC", EpicsValue::String("two".into()))
        .await
        .unwrap();
    db.process_record("SRC").await.unwrap();
    assert!(until_rmt(&sdb, 2.0).await, "RMT is {}", num(&sdb, "RMT"));
    assert_eq!(num(&db, "SRC.SEVR"), 0.0);

    // A put callback (`CA_PUT_CALLBACK`): a put-notify through the link.
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::String("one".into()))
        .await
        .unwrap();
    assert!(until_rmt(&sdb, 1.0).await, "RMT is {}", num(&sdb, "RMT"));
    assert_eq!(num(&db, "SRC.SEVR"), 0.0);
}
