//! A `dbPut`'s "did this change anything" test must read the field through
//! the store the put wrote it to.
//!
//! C compares nothing: `dbPut` posts `DBE_PROPERTY` whenever the field is
//! `prop(YES)` and the put succeeded (`dbAccess.c:1395-1396`). The port skips
//! the post for an idempotent write (epics-base faac1df1), which turns the
//! comparison into a correctness surface — and `Record::get_field` sees only
//! the record type's own struct. A good deal of what C keeps per record type
//! lives on `CommonFields` here, the whole analog-alarm ladder among it, so a
//! `caput CALC.HIHI` compared `None` to `None` and reported "unchanged".
//!
//! The boundary is the STORE, not the record type: one case for a field the
//! record owns (the comparison always worked) and one for a field
//! `CommonFields` owns (it never did).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn calc_db() -> PvDatabase {
    let db = PvDatabase::new();
    let mut calc = CalcRecord::default();
    calc.calc = "0".into();
    db.add_record("CALC", Box::new(calc)).await.unwrap();
    db.ioc_init().await;
    db
}

/// `HIHI` lives on `CommonFields`. This is the case that posted nothing.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_common_stored_property_field_posts_the_property_event() {
    let db = calc_db().await;
    let rec = db.get_record("CALC").expect("record exists");
    let mut rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::PROPERTY.bits())
        .expect("subscriber");

    db.put_pv("CALC.HIHI", EpicsValue::Double(90.0))
        .await
        .unwrap();

    rx.try_recv()
        .expect("HIHI is prop(YES): a put that changes it posts DBE_PROPERTY");
}

/// The other half of the same gate: an idempotent write still posts nothing,
/// which is the whole reason the comparison exists.
#[epics_macros_rs::epics_test]
async fn a_put_that_rewrites_the_same_value_still_posts_nothing() {
    let db = calc_db().await;
    db.put_pv("CALC.HIHI", EpicsValue::Double(90.0))
        .await
        .unwrap();

    let rec = db.get_record("CALC").expect("record exists");
    let mut rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::PROPERTY.bits())
        .expect("subscriber");

    db.put_pv("CALC.HIHI", EpicsValue::Double(90.0))
        .await
        .unwrap();

    assert!(
        rx.try_recv().is_err(),
        "faac1df1: a write that changes nothing posts nothing"
    );
}

/// `EGU` lives on the calc's own struct — the case the comparison already saw,
/// kept so a future reader change cannot quietly lose it.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_record_owned_property_field_still_posts() {
    let db = calc_db().await;
    let rec = db.get_record("CALC").expect("record exists");
    let mut rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::PROPERTY.bits())
        .expect("subscriber");

    db.put_pv("CALC.EGU", EpicsValue::String("mm".into()))
        .await
        .unwrap();

    rx.try_recv()
        .expect("EGU is prop(YES) and record-owned: the post must survive");
}
