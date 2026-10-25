//! A CA link to a remote `DBR_ENUM` channel read as `DBR_STRING` serves the
//! server's string off a `DBR_STRING` monitor, which the first such read adds
//! and fails on (`dbCa.c:436-454`). An mbbi state with no name is `""`, not
//! the index digits a label table past its `no_str` gives.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port`. Tests here are
/// `#[serial(epics_env)]` so the process-wide env is not raced.
fn pin_env(port: u16) {
    // SAFETY: serialized by `#[serial(epics_env)]`; no other thread
    // reads/writes these vars concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} is {other:?}"),
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// A failed read is `devSiSoft`'s status, returned from `process`.
async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(CaError::unless_process_status)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_ca_link_reads_an_unnamed_mbbi_state_as_its_empty_string() {
    let mut src = MbbiRecord::new(2);
    src.zrst = "zero".into();
    src.onst = "one".into();
    let server = CaServer::builder()
        .port(0)
        .record("CALINK:ENUMSTR:SRC", src)
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let db = PvDatabase::new();
    let resolver = CaLinkResolver::with_client(client);
    db.register_link_set("ca", Arc::new(resolver.clone())).await;
    db.add_record("S", Box::new(StringinRecord::new("was")))
        .await
        .unwrap();
    db.put_pv_no_process("S.INP", EpicsValue::String("CALINK:ENUMSTR:SRC CA".into()))
        .await
        .unwrap();
    assert!(db.ca_link_init());
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("CALINK:ENUMSTR:SRC", fact_budget::FACT_BUDGET)
            .await,
        "CA link must connect before record processing"
    );

    process(&db, "S").await;
    assert_eq!(text(&db, "S"), "was", "the first read fails");
    assert_eq!(num(&db, "S.STAT"), 14.0, "LINK");
    assert_eq!(num(&db, "S.SEVR"), 3.0, "INVALID");

    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        process(&db, "S").await;
        if num(&db, "S.SEVR") == 0.0 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(num(&db, "S.SEVR"), 0.0, "the string monitor delivers");
    assert_eq!(text(&db, "S"), "");
}
