//! C opens every CA link at `dbCaAddLink` and `dbCaPutLinkCallback` refuses a
//! put while it is not connected (`dbCa.c:529-532`): a remote link the
//! resolver has not opened is refused with LINK/INVALID, and nothing is sent.

#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_put_before_the_link_opens_alarms_and_sends_nothing() {
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
            r#"record(ao, "SRC") { field(OUT, "RMT CA") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let _resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    // No iocInit open pass: the link is not opened.
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    let num = |db: &epics_base_rs::server::database::PvDatabase, pv: &str| {
        db.get_pv(pv).unwrap().to_f64().unwrap()
    };
    assert_eq!(num(&db, "SRC.STAT"), 14.0, "LINK");
    assert_eq!(num(&db, "SRC.SEVR"), 3.0, "INVALID");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(num(&sdb, "RMT"), 7.0, "the refused put wrote RMT");
}
