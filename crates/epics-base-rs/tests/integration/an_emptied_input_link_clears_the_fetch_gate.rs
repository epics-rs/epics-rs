//! The input stage's empty result must carry this cycle's `fetch_values`
//! status, not the last one's. C `calcRecord.c::fetch_values` (427-443)
//! returns the status of the reads it made THIS pass; an unset link is a
//! `dbConstGetValue` success, so a calc whose dead INPA is cleared computes
//! again on its next process and the LINK alarm goes with the failure.
//!
//! The port hands a record with nothing to read `InputStage::none` without
//! running the fetch, and this is the boundary that result has to hold on:
//! a failing INPA one cycle, an empty one the next.

use std::collections::HashMap;

use epics_base_rs::server::database::{ProcessMode, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut v = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut v).await.unwrap();
}

fn state(db: &PvDatabase, rec: &str) -> (f64, u8, u16, AlarmSeverity) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    let val = inst
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
        .unwrap();
    (val, inst.common.udf, inst.common.stat, inst.common.sevr)
}

#[epics_macros_rs::epics_test]
async fn a_calc_whose_dead_inpa_is_emptied_computes_on_its_next_cycle() {
    let db = build(
        r#"
record(calc, "C") {
    field(CALC, "A+1")
    field(INPA, "NOSUCH:RECORD")
}
"#,
    )
    .await;

    process(&db, "C").await;
    let (val, udf, stat, sevr) = state(&db, "C");
    assert_eq!(
        val, 0.0,
        "a failed fetch gates the compute: VAL keeps its value"
    );
    assert_eq!(
        (stat, sevr),
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid)
    );
    assert_eq!(udf, 1, "the compute never ran, so VAL stays undefined");

    // C `dbPutField` is the one door that changes a link field.
    db.put_field_from_client(
        "C",
        "INPA",
        EpicsValue::String("".into()),
        ProcessMode::Inhibit,
        false,
    )
    .await
    .unwrap();
    process(&db, "C").await;
    let (val, udf, stat, sevr) = state(&db, "C");
    assert_eq!(
        val, 1.0,
        "nothing to fetch is a fetch success: A+1 on A's seeded 0"
    );
    assert_eq!(
        (stat, sevr),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "the LINK alarm went with the failed read"
    );
    assert_eq!(udf, 0, "the compute defined VAL");
}
