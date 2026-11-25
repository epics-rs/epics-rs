//! An in-process monitor's `{json}` suffix is its channel's filter chain,
//! as C's `dbChannelCreate` parses it (`dbChannel.c:513-516`, `chf_parse`).
//! The port split the suffix off for the record lookup and dropped it, so
//! the monitor ran unfiltered.

use std::collections::HashMap;

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::pv::SubscribeRefused;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_filter_suffix_filters_the_subscription() {
    let db = IocBuilder::new()
        .db_string(r#"record(ao, "ao") { }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut sub = DbSubscription::subscribe(&db, r#"ao.VAL{"dbnd":{"d":1}}"#)
        .await
        .unwrap();
    while sub.try_recv_event().is_ok() {}
    for v in [0.5, 1.0, 2.0] {
        db.put_record_field_from_ca_no_notify("ao", "VAL", EpicsValue::Double(v))
            .await
            .unwrap();
    }
    let mut seen = Vec::new();
    while let Ok(event) = sub.try_recv_event() {
        seen.push(event.value().as_value().cloned());
    }
    // dbnd passes the first change, then only one that moves 1 or more.
    assert_eq!(
        seen,
        [Some(EpicsValue::Double(0.5)), Some(EpicsValue::Double(2.0))]
    );

    // C prints `dbChannelCreate: Channel filter 'nosuch' not found` and
    // returns NULL.
    assert!(matches!(
        DbSubscription::subscribe(&db, r#"ao.VAL{"nosuch":{}}"#).await,
        Err(SubscribeRefused::BadFilter)
    ));
}
