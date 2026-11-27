//! An in-process monitor on a `$` channel is C's `dbChannelCreate` on it
//! (`dbChannel.c:486-505`): a string or link field is read as its chars, and
//! any other field is refused. The port took `VAL$` for a field name, so the
//! subscription succeeded and never posted.

use std::collections::HashMap;

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::pv::SubscribeRefused;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(lso, "e") { field(SIZV, "8") }
record(ai, "x") { }
"#;

#[epics_macros_rs::epics_test]
async fn a_dollar_channel_posts_chars_or_is_refused() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut sub = DbSubscription::subscribe(&db, "e.VAL$").await.unwrap();
    while sub.try_recv_event().is_ok() {}
    db.put_record_field_from_ca_no_notify("e", "VAL", EpicsValue::String("v0".into()))
        .await
        .unwrap();
    let event = sub.try_recv_event().expect("the put posts to e.VAL$");
    // camonitor e.VAL$ on softIoc: `3 118 48 0`, LEN elements.
    assert_eq!(
        event.value().as_value(),
        Some(&EpicsValue::CharArray(b"v0\0".to_vec().into()))
    );

    // A string field is chars too: camonitor x.DESC$ shows `3 97 98 0`
    // after `caput x.DESC ab`, where x.DESC shows the string.
    let mut sub = DbSubscription::subscribe(&db, "x.DESC$").await.unwrap();
    while sub.try_recv_event().is_ok() {}
    db.put_record_field_from_ca_no_notify("x", "DESC", EpicsValue::String("ab".into()))
        .await
        .unwrap();
    let event = sub.try_recv_event().expect("the put posts to x.DESC$");
    assert_eq!(
        event.value().as_value(),
        Some(&EpicsValue::CharArray(b"ab\0".to_vec().into()))
    );

    // dbChannel.c:500-503: S_dbLib_fieldNotFound.
    assert!(matches!(
        DbSubscription::subscribe(&db, "x.VAL$").await,
        Err(SubscribeRefused::FieldNotFound)
    ));
}
