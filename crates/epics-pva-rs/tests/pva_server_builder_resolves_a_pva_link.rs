//! `PvaServerBuilder` declares `pva` before its `.db` loads, as an IOC linking
//! pvxs7x.dbd has `link("pva", "lsetPVX")` beside `pvxsBaseRegistrar`, so a
//! `{pva:…}` link loads and reads as it does on softIocPVA.

#![cfg(feature = "client")]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_pva_rs::server::PvaServer;
use tokio::sync::watch;

fn number(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name)
        .ok()
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{name} not readable as a number"))
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pva_link_loads_and_reads_through_the_builder() {
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", "0");
    }
    let source = PvaServer::builder()
        .port(0)
        .db_string(
            r#"record(ao, "PL:SRC") { field(VAL, "4") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let (tx, mut rx) = watch::channel(None);
    let run = tokio::spawn(async move { source.run_reporting(Some(tx)).await });
    let tcp = loop {
        if let Some(h) = rx.borrow_and_update().clone() {
            break h.report().tcp_port;
        }
        rx.changed().await.expect("server reports its ports");
    };
    // The link's client is built at link-set install, inside `build`, and
    // finds the source by name server only.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{tcp}"));
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }

    let ioc = PvaServer::builder()
        .db_string(
            r#"record(ai, "PL:IN") { field(INP, {pva:"PL:SRC"}) }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .expect("the builder loads a {pva:} link");
    let db = ioc.database();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Until the channel connects the read fails, as C `dbGetLink` does on
        // a disconnected pva link, and the record is LINK/INVALID.
        let _ = db
            .process_record_with_links("PL:IN", &mut ProcStack::new())
            .await;
        let got = (number(db, "PL:IN"), number(db, "PL:IN.SEVR"));
        if got == (4.0, 0.0) {
            break;
        }
        assert!(Instant::now() < deadline, "PL:IN read {got:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    run.abort();
    let _ = run.await;
}
