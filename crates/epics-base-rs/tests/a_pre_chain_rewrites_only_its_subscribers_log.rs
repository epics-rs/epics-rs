//! A pre-chain filter rewrites its own subscriber's copy of the log
//! (`dbChannelRunPreChain`, `dbEvent.c:901`); every other subscriber of the
//! same post reads the log as posted.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn ts_then_utag_leaves_the_other_subscribers_time_and_utag() {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::default()))
        .await
        .unwrap();
    // Device time that nothing sets: the posted TIME is far from now.
    db.put_record_field_from_ca("AO", "TSE", EpicsValue::Short(-2))
        .await
        .unwrap();
    let mask = (EventMask::VALUE | EventMask::LOG).bits();
    let mut filtered =
        DbSubscription::subscribe_with_mask(&db, r#"AO.VAL{"ts":{},"utag":{}}"#, 0, mask)
            .await
            .unwrap();
    let mut plain = DbSubscription::subscribe_with_mask(&db, "AO", 0, mask)
        .await
        .unwrap();

    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(1.5))
        .await
        .unwrap();

    let rec = db.get_record("AO").unwrap();
    let (time, utag) = {
        let r = rec.read();
        (r.common.time, r.common.utag as i32)
    };
    let stamped = filtered.try_recv_event().expect("the put posts VAL");
    let posted = plain.try_recv_event().expect("the put posts VAL");
    assert_ne!(stamped.time(), time, "ts stamps its subscriber's log");
    assert_eq!(posted.time(), time);
    assert_eq!(posted.utag(), utag);
    assert_eq!(stamped.utag(), utag);
}
