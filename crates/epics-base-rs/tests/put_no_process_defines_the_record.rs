//! A value-field put defines the record on EVERY put route.
//!
//! C `dbPut` ends with `isValueField = dbIsValueField(pfldDes); if
//! (isValueField) precord->udf = FALSE;` (`dbAccess.c:1413-1414`) — one rule,
//! reached by every caller, `dbPutField` and the autosave restore path alike.
//! It clears `udf` and NOTHING else: STAT/SEVR keep the born `UDF_ALARM` until
//! the record's own cycle recomputes them.
//!
//! `put_pv_no_process` — the port's restore-shaped route, which writes without
//! driving a process — was the one put body that omitted it, so a restored
//! record stayed undefined and raised UDF_ALARM on its first cycle.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

async fn ai_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("A", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db
}

fn udf(db: &PvDatabase, name: &str) -> u8 {
    db.get_record(name).unwrap().read().common.udf
}

/// BOUNDARY: the value field. `dbIsValueField` is true, so the put defines the
/// record.
#[epics_macros_rs::epics_test]
async fn a_value_field_put_clears_udf() {
    let db = ai_db().await;
    assert_eq!(udf(&db, "A"), 1, "a fresh record is born undefined");

    db.put_pv_no_process("A.VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();

    assert_eq!(udf(&db, "A"), 0, "C dbAccess.c:1414");
}

/// BOUNDARY: the bare record name addresses the same value field.
#[epics_macros_rs::epics_test]
async fn a_bare_name_put_clears_udf() {
    let db = ai_db().await;

    db.put_pv_no_process("A", EpicsValue::Double(5.0))
        .await
        .unwrap();

    assert_eq!(udf(&db, "A"), 0);
}

/// BOUNDARY: any other field. `dbIsValueField` is false, so the record stays
/// undefined — a display-limit restore does not define a record.
#[epics_macros_rs::epics_test]
async fn a_non_value_field_put_leaves_udf_set() {
    let db = ai_db().await;

    db.put_pv_no_process("A.HOPR", EpicsValue::Double(100.0))
        .await
        .unwrap();

    assert_eq!(udf(&db, "A"), 1, "HOPR is not the value field");
}

/// BOUNDARY: the clear is UDF alone. C's `goto done` path touches no alarm
/// field, so a put that drives no process leaves the born UDF_ALARM standing
/// until the record's own cycle recomputes it.
#[epics_macros_rs::epics_test]
async fn the_put_clears_udf_and_no_alarm_field() {
    let db = ai_db().await;
    {
        let rec = db.get_record("A").unwrap();
        let mut inst = rec.write();
        inst.common.sevr = AlarmSeverity::Invalid;
        inst.common.stat = epics_base_rs::server::recgbl::alarm_status::UDF_ALARM;
    }

    db.put_pv_no_process("A.VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();

    let rec = db.get_record("A").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.udf, 0);
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "dbPut clears udf only; the alarm waits for the process cycle"
    );
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::UDF_ALARM
    );
}
