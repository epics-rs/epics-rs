//! Each CA link is its own `caLink` with its own channel (`dbCa.c:373-401`,
//! `:1209-1236`): two links naming one remote PV send that server two
//! CREATE_CHAN and two EVENT_ADD, as softIoc R7.0.10 does.

#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

fn monitors(sdb: &PvDatabase) -> usize {
    sdb.get_record("RMT")
        .unwrap()
        .read()
        .subscriber_count("VAL")
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn two_links_on_one_remote_name_open_two_channels() {
    let server = CaServer::builder()
        .port(0)
        .db_string(r#"record(ai, "RMT") { field(VAL, "4") }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let sdb = server.database().clone();
    let stats = server.stats();
    let _server = tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"record(calc, "H") { field(INPA, "RMT CP") field(INPB, "RMT CP") field(CALC, "A+B") }"#,
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
    db.initialize_link_locality().await;
    // The build ran with no `ca` lset: its `dbCaAddLink` pass is this one.
    db.setup_external_link_opens().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;

    let deadline = Instant::now() + Duration::from_secs(10);
    while (stats.active_channels(), monitors(&sdb)) != (2, 2) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A third open would land well inside this.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stats.active_channels(), 2, "CREATE_CHAN, one per link");
    assert_eq!(monitors(&sdb), 2, "EVENT_ADD, one per link");
}
