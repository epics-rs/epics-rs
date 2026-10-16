//! The soft INP of a record with no other read is fetched under the
//! reader's own guard when it is a plain NPP link to another local record.
//! One case per side of that boundary: each must deliver the same value
//! the guard-released general path does.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

async fn val_after_process(db: &PvDatabase, name: &str) -> f64 {
    let mut visited = ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    match db.get_pv(name).unwrap() {
        EpicsValue::Double(v) => v,
        other => panic!("{name}: expected Double, got {other:?}"),
    }
}

async fn ai_reading(db: &PvDatabase, name: &str, inp: &str) {
    db.add_record(name, Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record(name).unwrap();
    let mut inst = rec.write();
    inst.put_common_field("INP", EpicsValue::String(inp.into()))
        .unwrap();
    inst.common.udf = 0;
}

#[epics_macros_rs::epics_test]
async fn npp_local_source_is_read_under_the_held_guard() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    ai_reading(&db, "DST", "SRC NPP").await;
    assert_eq!(val_after_process(&db, "DST").await, 7.0);
}

#[epics_macros_rs::epics_test]
async fn pp_source_is_processed_before_the_read() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(CalcRecord::new("42")))
        .await
        .unwrap();
    ai_reading(&db, "DST", "SRC PP").await;
    assert_eq!(val_after_process(&db, "DST").await, 42.0);
}

#[epics_macros_rs::epics_test]
async fn self_link_reads_the_readers_own_field() {
    let db = PvDatabase::new();
    ai_reading(&db, "SELF", "SELF.HOPR").await;
    {
        let rec = db.get_record("SELF").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("HOPR", EpicsValue::Double(9.0))
            .unwrap();
    }
    assert_eq!(val_after_process(&db, "SELF").await, 9.0);
}

#[epics_macros_rs::epics_test]
async fn link_to_no_record_fails_the_read() {
    use epics_base_rs::server::recgbl::alarm_status;
    let db = PvDatabase::new();
    ai_reading(&db, "DST", "NOWHERE NPP").await;
    assert_eq!(val_after_process(&db, "DST").await, 0.0);
    let rec = db.get_record("DST").unwrap();
    assert_eq!(rec.read().common.stat, alarm_status::LINK_ALARM);
}
