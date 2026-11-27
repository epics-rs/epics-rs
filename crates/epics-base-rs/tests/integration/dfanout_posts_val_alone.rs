//! A dfanout process cycle posts VAL and nothing else: `monitor()` has one
//! `db_post_events`, on `&prec->val` (`dfanoutRecord.c:283-298`). SELN moved
//! by its SELL read, or any other watched field, reaches no subscriber.

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SEL") { field(VAL, "0") }
record(dfanout, "D") { field(SELM, "Specified") field(SELL, "SEL NPP") field(SELN, "0") }
"#;

#[epics_macros_rs::epics_test]
async fn a_moved_seln_posts_nothing_and_val_still_posts() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut seln = DbSubscription::subscribe(&db, "D.SELN").await.unwrap();
    let mut val = DbSubscription::subscribe(&db, "D.VAL").await.unwrap();
    while seln.try_recv_event().is_ok() {}
    while val.try_recv_event().is_ok() {}

    db.put_record_field_from_ca("SEL", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("D", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(db.get_pv("D.SELN").unwrap(), EpicsValue::UShort(1));
    assert!(val.try_recv_event().is_ok(), "VAL posts");
    assert!(seln.try_recv_event().is_err(), "SELN posts nothing");
}
