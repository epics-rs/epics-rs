//! `collect_subscriber_posts` returns before asking the record anything when
//! C's `monitor_mask` is shut, nothing is subscribed or published, and the
//! type holds no one-shot post state. These are the two boundaries where the
//! type DOES hold such state on an unwatched cycle: the state must still be
//! consumed on that cycle, as C's `monitor()` consumes it with or without a
//! subscriber.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn subscribe(db: &PvDatabase, rec: &str, field: &str) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber(
        field,
        1,
        DbFieldType::Double,
        (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
    )
    .expect("subscription accepted")
}

/// Leave the record in the alarm state its next cycle settles to, so that cycle
/// is no alarm transition and C's `monitor_mask` is decided by the value alone.
fn settle_alarm(db: &PvDatabase, rec: &str) {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.common.udf = 0;
    g.common.sevr = epics_base_rs::server::record::AlarmSeverity::NoAlarm;
    g.common.stat = 0;
}

fn read(db: &PvDatabase, rec: &str, field: &str) -> EpicsValue {
    db.get_record(rec)
        .unwrap()
        .read()
        .resolve_field(field)
        .unwrap_or_else(|| panic!("{rec}.{field} resolves"))
}

/// transform's first-cycle post of A..P (C `firstCalcPosted`,
/// transformRecord.c:794) is spent by the first cycle whether or not anyone
/// watches, and its VAL is inert, so that cycle's value mask is shut. A
/// subscriber attached afterwards must not receive it on the second cycle.
#[epics_macros_rs::epics_test]
async fn an_unwatched_first_transform_cycle_spends_its_first_post() {
    let db = PvDatabase::new();
    db.add_record("T", Box::new(TransformRecord::new()))
        .await
        .unwrap();
    settle_alarm(&db, "T");
    process(&db, "T").await;

    let mut rx = subscribe(&db, "T", "A");
    process(&db, "T").await;
    let mut posts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        posts.push(ev.mask());
    }
    assert!(
        posts.is_empty(),
        "A did not move and the first-cycle post was spent on cycle one: {posts:?}"
    );
}

/// ao's `omod` (aoRecord.c:535) opens the secondary block on a ramping cycle
/// whose MDEL/ADEL shut VAL's own mask, and `oraw = rval` (:542) runs inside
/// it with no subscriber at all.
#[epics_macros_rs::epics_test]
async fn an_unwatched_ramping_ao_advances_oraw() {
    let db = PvDatabase::new();
    let mut a = AoRecord::new(0.0);
    a.put_field("OROC", EpicsValue::Double(1.0)).unwrap();
    a.put_field("MDEL", EpicsValue::Double(1000.0)).unwrap();
    a.put_field("ADEL", EpicsValue::Double(1000.0)).unwrap();
    db.add_record("AO", Box::new(a)).await.unwrap();

    db.put_pv("AO", EpicsValue::Double(10.0)).await.unwrap();
    settle_alarm(&db, "AO");
    process(&db, "AO").await;

    let rval = read(&db, "AO", "RVAL");
    assert_ne!(
        rval,
        EpicsValue::Long(0),
        "the ramp moved RVAL, or the case proves nothing"
    );
    let oraw = db.get_record("AO").unwrap().read().record.get_field("ORAW");
    assert_eq!(
        oraw,
        Some(rval),
        "omod opened the guard, so ORAW follows RVAL with nobody watching"
    );
}

/// The deadband post of an unwatched cycle publishes its classes without
/// reading the value: there is no VAL post to deliver, and the mask a CP edge
/// reads still carries `DBE_VALUE`.
#[test]
fn an_unwatched_deadband_post_publishes_its_mask_without_a_value() {
    use epics_base_rs::server::record::RecordInstance;
    use epics_base_rs::server::records::ai::AiRecord;
    let mut instance = RecordInstance::new("AI", AiRecord::default());
    instance.common.udf = 0;
    instance.record.set_val(EpicsValue::Double(5.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _) = instance.process_local().unwrap();
    assert!(snap.iter().all(|(f, _, _)| f != "VAL"));
    assert!(snap.published_mask().contains(EventMask::VALUE));
}
