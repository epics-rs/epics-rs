//! `seq` posts from its own copies, as C's does: a group posts DOn from
//! `processCallback` when its DOLn read moved it (`seqRecord.c:257-268`), and
//! `asyncFinish` posts SELN when it differs from OLDN, then copies it
//! (`:230-232`). An unmoved field posts nothing.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SRC") { field(VAL, "0") }
record(seq, "S") { field(SELM, "All") field(DOL0, "SRC NPP") }
record(ao, "SEL") { field(VAL, "0") }
record(seq, "N") { field(SELM, "Specified") field(SELL, "SEL NPP") field(SELN, "0") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Process `rec` and wait for `asyncFinish` to clear PACT.
async fn run_chain(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if !db.get_record(rec).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{rec} never finished its chain");
}

#[epics_macros_rs::epics_test]
async fn a_moved_dov_posts_once_and_an_unmoved_one_not_at_all() {
    let db = build().await;
    let mut sub = DbSubscription::subscribe(&db, "S.DO0").await.unwrap();
    while sub.try_recv_event().is_ok() {}

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    run_chain(&db, "S").await;
    let ev = sub.try_recv_event().expect("a moved DO0 posts");
    assert_eq!(ev.value().as_value(), Some(&EpicsValue::Double(5.0)));
    assert_eq!(ev.mask(), EventMask::VALUE | EventMask::LOG);
    assert!(sub.try_recv_event().is_err(), "DO0 posts once");

    run_chain(&db, "S").await;
    assert!(
        sub.try_recv_event().is_err(),
        "an unmoved DO0 posts nothing"
    );
}

#[epics_macros_rs::epics_test]
async fn a_moved_seln_posts_once_and_oldn_follows() {
    let db = build().await;
    let mut sub = DbSubscription::subscribe(&db, "N.SELN").await.unwrap();
    while sub.try_recv_event().is_ok() {}

    run_chain(&db, "N").await;
    assert!(sub.try_recv_event().is_err(), "SELN == OLDN posts nothing");

    db.put_record_field_from_ca("SEL", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    run_chain(&db, "N").await;
    let ev = sub.try_recv_event().expect("a moved SELN posts");
    assert_eq!(ev.value().as_value(), Some(&EpicsValue::UShort(1)));
    assert_eq!(ev.mask(), EventMask::VALUE | EventMask::LOG);
    assert!(sub.try_recv_event().is_err(), "SELN posts once");
    assert_eq!(db.get_pv("N.OLDN").unwrap(), EpicsValue::UShort(1));

    run_chain(&db, "N").await;
    assert!(
        sub.try_recv_event().is_err(),
        "an unmoved SELN posts nothing"
    );
}
