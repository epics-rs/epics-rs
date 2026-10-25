//! A CA link to a `DBR_ENUM` channel read as `DBR_STRING` serves the
//! server's string off a `DBR_STRING` monitor, which the first such read adds
//! and fails on (`dbCa.c:436-454`), on a link served in-process as on a
//! remote one. An mbbi state with no name is `""`, not the index digits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbbi, "M") { field(ZRST, "zero") field(ONST, "one") field(VAL, "2") }
record(stringin, "S") { field(INP, "M CA") field(VAL, "was") }
"#;

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

#[epics_macros_rs::epics_test]
async fn a_local_ca_link_reads_an_unnamed_mbbi_state_as_its_empty_string() {
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

    process(&db, "S").await;
    assert_eq!(text(&db, "S"), "was", "the first read fails");
    assert_eq!(num(&db, "S.STAT"), 14.0, "LINK");
    assert_eq!(num(&db, "S.SEVR"), 3.0, "INVALID");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        process(&db, "S").await;
        if num(&db, "S.SEVR") == 0.0 || Instant::now() > deadline {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(num(&db, "S.SEVR"), 0.0, "the string monitor delivers");
    assert_eq!(text(&db, "S"), "");
}

/// iocInit's open pass, then each external link connected.
async fn open_links(db: &PvDatabase) {
    db.setup_external_link_opens().await;
    for record in db.all_record_names() {
        for (field, _, link) in db.record_link_fields(&record) {
            let Some(link) = link.external() else {
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while db.external_link_metadata(link).is_none() {
                assert!(Instant::now() < deadline, "{record}.{field} connects");
                epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}
