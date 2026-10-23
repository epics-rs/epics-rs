//! A CP channel closed with its last link and opened again by a later put
//! is a new pvxs `pvaLinkChannel`, with its own monitor driving its links'
//! scans (`ioc/pvalink_channel.cpp:95-99`, `:314-325`).
//!
//! Tokio only: the pvalink runtime is.
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

const REMOTE: &str = r#"
record(ai, "SRC") { field(VAL, "1") }
record(ai, "OTHER") { field(VAL, "2") }
"#;

const DB: &str = r#"
record(ai, "H") { field(INP, {pva:{pv:"SRC", proc:"CP"}}) }
"#;

/// Waits until `H` holds `want`.
async fn until_h(db: &PvDatabase, want: f64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while db.get_pv("H").ok().and_then(|v| v.to_f64()) != Some(want) {
        assert!(Instant::now() < deadline, "H never read {want}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn put_inp(db: &PvDatabase, link: &str) {
    db.put_record_field_from_ca("H", "INP", EpicsValue::String(link.into()))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cp_channel_reopened_after_closing_scans_its_holder() {
    let (remote, _) = IocBuilder::new()
        .db_string(REMOTE, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let server = PvaServer::start(
        Arc::new(PvDatabaseSource::new(remote.clone())),
        PvaServerConfig::isolated(),
    )
    .expect("source PVA server starts");
    // SAFETY: before the reader IOC builds its PVA client; nothing else in
    // this test binary touches the environment.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = install_pvalink_resolver(&db).await;
    db.ioc_init().await;
    db.process_cp_links_on_connect().await;
    until_h(&db, 1.0).await;

    put_inp(&db, r#"{pva:{pv:"OTHER", proc:"CP"}}"#).await;
    until_h(&db, 2.0).await;
    let channels: Vec<String> = resolver
        .channel_diagnostics()
        .into_iter()
        .map(|c| c.pv_name)
        .collect();
    assert_eq!(channels, ["OTHER"], "SRC closed with its last link");

    put_inp(&db, r#"{pva:{pv:"SRC", proc:"CP"}}"#).await;
    until_h(&db, 1.0).await;
    remote
        .put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    until_h(&db, 5.0).await;
}
