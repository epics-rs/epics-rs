//! A put of a CP pvalink moves the holder's scan to the new channel. pvxs
//! `dbPutFieldLink` runs `pvaRemoveLink`, whose `~pvaLink` takes the link
//! off the old channel's `links` (`pvalink_link.cpp:27-35`), then
//! `pvaOpenLink`, which attaches it to the new channel and scans the holder
//! once if that channel is already connected (`pvalink_lset.cpp:146-171`).
//!
//! Tokio only: the PVA server is.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const HOLDER: &str = r#"
record(calc, "H") { field(INPA, "pva://T:OLD?proc=CP") field(CALC, "A") field(FLNK, "CNT") }
record(calc, "CNT") { field(INPA, "CNT NPP") field(CALC, "A+1") }
"#;

fn value(db: &PvDatabase, name: &str) -> f64 {
    match db.get_pv(name).unwrap() {
        EpicsValue::Double(v) => v,
        other => panic!("{name} is {other:?}"),
    }
}

async fn wait_for(db: &PvDatabase, name: &str, want: f64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while value(db, name) != want {
        assert!(
            Instant::now() < deadline,
            "{name} stayed {} instead of {want}",
            value(db, name)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn post(db: &PvDatabase, name: &str, v: f64) {
    db.put_record_field_from_ca_no_notify(name, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cp_pvalink_put_scans_on_the_new_channel_only() {
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // SAFETY: the one test in this binary, before the PVA client exists.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }

    let (remote, _) = IocBuilder::new()
        .db_string(
            r#"
            record(ao, "T:OLD") { field(VAL, "1") }
            record(ao, "T:NEW") { field(VAL, "2") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let _server = PvaServer::start(
        Arc::new(PvDatabaseSource::new(remote.clone())),
        PvaServerConfig {
            tcp_port: port,
            ..PvaServerConfig::isolated()
        },
    )
    .expect("target PVA server starts");

    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(HOLDER, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    wait_for(&db, "H", 1.0).await;

    db.put_record_field_from_ca_no_notify(
        "H",
        "INPA",
        EpicsValue::String("pva://T:NEW?proc=CP".into()),
    )
    .await
    .unwrap();
    wait_for(&db, "H", 2.0).await;

    post(&remote, "T:NEW", 5.0).await;
    wait_for(&db, "H", 5.0).await;

    let scans = value(&db, "CNT");
    post(&remote, "T:OLD", 9.0).await;
    // A sentinel on NEW after OLD's post: once it lands, OLD's monitor has
    // had as long as NEW's to reach the holder.
    tokio::time::sleep(Duration::from_millis(500)).await;
    post(&remote, "T:NEW", 6.0).await;
    wait_for(&db, "H", 6.0).await;
    assert_eq!(
        value(&db, "CNT"),
        scans + 1.0,
        "a post on the old channel processed the holder"
    );

    // A put to a link no lset opens still removes the pva one.
    db.put_record_field_from_ca_no_notify("H", "INPA", EpicsValue::String("3".into()))
        .await
        .unwrap();
    let scans = value(&db, "CNT");
    post(&remote, "T:NEW", 7.0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        value(&db, "CNT"),
        scans,
        "a post on a removed link's channel processed the holder"
    );
}
