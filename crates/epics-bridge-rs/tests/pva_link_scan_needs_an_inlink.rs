//! Only an input link scans its record on a monitor event: pvxs
//! `pvaLink::scanOnUpdate` answers `scanOnUpdateNo` unless the link's field
//! is `DBF_INLINK` (`pvalink_link.cpp:122-133`). softIocPVX 1.5.3 scans none
//! of a dfanout `OUTA`, a seq `LNK1` or an `FLNK` holding
//! `{pva:{pv:…,proc:'CP'}}`. Each holder links its own PV, so a wrong scan
//! of an output holder cannot write back into another holder's channel.
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

const HOLDERS: &str = r#"
record(calc, "H:INP") { field(INPA, "{pva:{pv:'T:A',proc:'CP'}}") field(CALC, "A") field(FLNK, "N:INP") }
record(dfanout, "H:DF") { field(OUTA, "{pva:{pv:'T:DF',proc:'CP'}}") field(FLNK, "N:DF") }
record(seq, "H:SEQ") { field(LNK1, "{pva:{pv:'T:SEQ',proc:'CP'}}") field(FLNK, "N:SEQ") }
record(calc, "H:FL") { field(INPA, "H:FL NPP") field(CALC, "A+1") field(FLNK, "{pva:{pv:'T:FL',proc:'CP'}}") }
record(calc, "N:INP") { field(INPA, "N:INP NPP") field(CALC, "A+1") }
record(calc, "N:DF") { field(INPA, "N:DF NPP") field(CALC, "A+1") }
record(calc, "N:SEQ") { field(INPA, "N:SEQ NPP") field(CALC, "A+1") }
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

#[tokio::test(flavor = "multi_thread")]
async fn only_an_input_pvalink_scans_on_update() {
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
            record(ao, "T:A") { field(VAL, "1") }
            record(ao, "T:DF") { field(VAL, "1") }
            record(ao, "T:SEQ") { field(VAL, "1") }
            record(ao, "T:FL") { field(VAL, "1") }
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
        .db_string(HOLDERS, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    wait_for(&db, "H:INP", 1.0).await;

    for v in [2.0, 3.0, 4.0] {
        // The INP holder's channel last: its scan is the sentinel.
        for pv in ["T:DF", "T:SEQ", "T:FL", "T:A"] {
            remote
                .put_record_field_from_ca_no_notify(pv, "VAL", EpicsValue::Double(v))
                .await
                .unwrap();
        }
        wait_for(&db, "H:INP", v).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(value(&db, "N:INP"), 4.0, "INP CP: the connect and 3 posts");
    for counter in ["N:DF", "N:SEQ", "H:FL"] {
        assert_eq!(value(&db, counter), 0.0, "{counter} was scanned");
    }
}
