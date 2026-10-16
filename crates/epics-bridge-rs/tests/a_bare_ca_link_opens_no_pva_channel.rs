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
    // SAFETY: the only test in this binary, before any runtime thread reads
    // the environment. Nothing is searched for: the CA link stays down.
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
    let r = R.get().unwrap();
    for _ in 0..3 {
        db.process_record("N").await.unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let diags: Vec<_> = r
        .channel_diagnostics()
        .into_iter()
        .map(|d| format!("{d:?}"))
        .collect();
    assert!(
        diags.iter().any(|d| d.contains("PROBE:NOWHERE:P")),
        "the pva link opened its channel: {diags:#?}"
    );
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
