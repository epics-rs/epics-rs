//! `DbSubscription::recv_snapshot` reads options as C's `dbGet` does: from
//! the record at delivery (`getOptions`, `dbAccess.c:336-414`), with the
//! value, alarm, time and utag from the queued log.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_queued_event_is_read_with_the_records_current_precision() {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::default()))
        .await
        .unwrap();
    let mut sub = DbSubscription::subscribe(&db, "AO").await.unwrap();
    let mut events = DbSubscription::subscribe(&db, "AO").await.unwrap();

    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(1.5))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    db.put_record_field_from_ca("AO", "PREC", EpicsValue::Short(4))
        .await
        .unwrap();
    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();

    let posted = events.try_recv_event().expect("the first put posts VAL");
    let first = sub
        .try_recv_snapshot()
        .expect("the first put posts VAL")
        .expect("the read succeeds");
    let second = sub
        .try_recv_snapshot()
        .expect("the second put posts VAL")
        .expect("the read succeeds");
    assert_eq!(
        first.precision(),
        Some(4),
        "PREC is the record's, not the log's 0"
    );
    assert_eq!(first.value, EpicsValue::Double(1.5));
    assert_eq!(first.timestamp, posted.time());
    assert_ne!(first.timestamp, second.timestamp);
}

/// A `ts` string log is `DBF_STRING`, which serves no precision
/// (`dbAccess.c:386-395`), whatever the record's own field supplies.
#[epics_macros_rs::epics_test]
async fn a_ts_string_log_is_read_without_the_fields_precision() {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::default()))
        .await
        .unwrap();
    let mask = (EventMask::VALUE | EventMask::LOG).bits();
    let mut sub =
        DbSubscription::subscribe_with_mask(&db, r#"AO.VAL{"ts":{"str":"epics"}}"#, 0, mask)
            .await
            .unwrap();

    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(1.5))
        .await
        .unwrap();

    let read = sub
        .try_recv_snapshot()
        .expect("the put posts VAL")
        .expect("the read succeeds");
    assert!(
        matches!(read.value, EpicsValue::String(_)),
        "{:?}",
        read.value
    );
    assert!(!read.properties.precision);
}
