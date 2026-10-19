//! A string put through a CA link to a `DBR_ENUM` channel goes as one
//! `DBR_STRING` (`dbCa.c:561-575`), so the target resolves the state name,
//! on a link served in-process as on a remote one.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "SRC") { field(OUT, "TGT CA") }
record(mbbo, "TGT") { field(ZRST, "zero") field(ONST, "one") field(TWST, "two") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_state_name_put_through_a_local_ca_link_selects_that_state() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::String("two".into()))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while num(&db, "TGT") != 2.0 && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(num(&db, "TGT"), 2.0);
    assert_eq!(num(&db, "SRC.SEVR"), 0.0);
}

/// iocInit's open pass, then each external link connected: C opens every CA
/// link at `dbCaAddLink` and refuses its puts until it connects.
async fn open_links(db: &PvDatabase) {
    db.setup_external_link_opens().await;
    for name in db.external_link_pv_names().await {
        let deadline = Instant::now() + Duration::from_secs(10);
        while db.external_link_metadata(name.as_bytes()).is_none() {
            assert!(Instant::now() < deadline, "{name} connects");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    }
}
