#![cfg(all(feature = "pvalink", feature = "calink-core", tokio_backend))]
//! A bare (CA) link is the `ca` lset's alone: C picks one lset per link and
//! never searches across them (`dbLink.c:107-130`), so in softIocPVX, which
//! has both, a CA link that is down or still connecting is dbCa's and opens
//! no pva channel for its name.

use std::collections::HashMap;
use std::sync::OnceLock;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_bridge_rs::pvalink::PvaLinkResolver;
use epics_ca_rs::calink::CaLinkResolver;

#[tokio::test(flavor = "multi_thread")]
async fn a_down_ca_link_opens_no_pva_channel() {
    // SAFETY: under nextest, the repo's runner, each test runs in its own
    // process; this is before any runtime thread reads the environment.
    // Nothing is searched for: the CA link stays down.
    unsafe {
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_ADDR_LIST", "");
    }
    static R: OnceLock<PvaLinkResolver> = OnceLock::new();
    static CA: OnceLock<CaLinkResolver> = OnceLock::new();
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            let _ = R.set(epics_bridge_rs::pvalink::install_pvalink_resolver(&db).await);
            Vec::new()
        })
        .register_link_type("ca", move |db| async move {
            let _ = CA.set(epics_ca_rs::calink::install_calink_resolver(&db, reactor).await);
            Vec::new()
        })
        .db_string(
            r#"record(calc, "N") { field(INPA, "PROBE:NOWHERE:A CA") field(CALC, "A") }
               record(ai, "P") { field(INP, {pva:"PROBE:NOWHERE:P"}) }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.process_cp_links_on_connect().await;
    let r = R.get().unwrap();
    for _ in 0..3 {
        db.process_record("N").await.unwrap();
    }
    let diags = || -> Vec<String> {
        r.channel_diagnostics()
            .into_iter()
            .map(|d| format!("{d:?}"))
            .collect()
    };
    let deadline = epics_base_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while !diags().iter().any(|d| d.contains("PROBE:NOWHERE:P")) {
        assert!(
            std::time::Instant::now() < deadline,
            "the pva link opened no channel: {:#?}",
            diags()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // absence-window: a pva channel for the CA link's name opens no later than P's.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let diags = diags();
    assert!(
        !diags.iter().any(|d| d.contains("PROBE:NOWHERE:A")),
        "a CA link opened a pva channel: {diags:#?}"
    );
    assert_eq!(
        CA.get().unwrap().link_report(),
        [("PROBE:NOWHERE:A".to_string(), false)],
        "dbcar: the CA link is dbCa's, down"
    );
}
