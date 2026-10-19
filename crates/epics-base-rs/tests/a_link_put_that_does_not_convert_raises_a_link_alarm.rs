//! A put through a CA link converts on the record thread, C
//! `dbCaPutLinkCallback` into `pputNative` (`dbCa.c:572-590`), and a string
//! that is not a number of the channel's type is that call's non-zero status,
//! which `dbPutLink` turns into LINK/INVALID (`dbLink.c:443-446`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "SRC") { field(OUT, "TGT CA") }
record(ao, "TGT") { field(VAL, "7") }
"#;

const LINK_ALARM: f64 = 14.0;
const INVALID_ALARM: f64 = 3.0;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put_src(db: &PvDatabase, text: &str) {
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::String(text.into()))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_string_that_is_not_a_number_alarms_and_writes_nothing() {
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

    // A number converts and lands, once the link is up.
    let deadline = Instant::now() + Duration::from_secs(10);
    while num(&db, "TGT") != 12.5 && Instant::now() < deadline {
        put_src(&db, "12.5").await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(num(&db, "TGT"), 12.5);
    assert_eq!(num(&db, "SRC.SEVR"), 0.0);

    put_src(&db, "abc").await;
    assert_eq!(num(&db, "SRC.STAT"), LINK_ALARM);
    assert_eq!(num(&db, "SRC.SEVR"), INVALID_ALARM);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    assert_eq!(num(&db, "TGT"), 12.5, "the unconverted put wrote TGT");

    // The empty string is the scalar row's zero, not a refusal.
    put_src(&db, "").await;
    assert_eq!(num(&db, "SRC.SEVR"), 0.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while num(&db, "TGT") != 0.0 && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(num(&db, "TGT"), 0.0);
}

/// iocInit's open pass, then each external link connected: C opens every CA
/// link at `dbCaAddLink` and refuses its puts until it connects.
async fn open_links(db: &epics_base_rs::server::database::PvDatabase) {
    db.setup_external_link_opens().await;
    for name in db.external_link_pv_names().await {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while db.external_link_metadata(name.as_bytes()).is_none() {
            assert!(std::time::Instant::now() < deadline, "{name} connects");
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}
