//! A QSRV group is no `dbChannelTest` channel, so softIocPVX refuses a
//! `local:true` link to it (`pvxs/ioc/pvalink_lset.cpp:70`).
#![cfg(all(feature = "qsrv-core", feature = "pvalink"))]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_bridge_rs::pvalink::{PvaLinkError, install_pvalink_resolver};
use epics_bridge_rs::qsrv::BridgeProvider;

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.
#[tokio::test]
async fn a_local_link_refuses_a_qsrv_group_pv() {
    // Backing records for the group's two members.
    let db = Arc::new(PvDatabase::new());
    db.add_record("GRP:level", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("GRP:count", Box::new(AiRecord::new(2.0)))
        .await
        .unwrap();

    // Register a QSRV group composite PV named `LOCAL:GROUP`.
    const GROUP_JSON: &str = r#"{
            "LOCAL:GROUP": {
                "+id": "epics:nt/NTGroup:1.0",
                "level": { "+channel": "GRP:level.VAL", "+type": "plain" },
                "count": { "+channel": "GRP:count.VAL", "+type": "plain" }
            }
        }"#;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load group");
    provider.process_groups().await;
    assert!(
        provider.has_group_pv("LOCAL:GROUP"),
        "group PV must be registered in the provider"
    );

    let resolver = install_pvalink_resolver(&db).await;

    let r = resolver.open_link("pva://LOCAL:GROUP?local=true").await;
    assert!(matches!(r, Err(PvaLinkError::NotLocal(_))), "{:?}", r.err());
}
