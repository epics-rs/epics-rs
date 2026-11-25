//! A simulated ai re-derives UDF from VAL: `readValue`'s YES arm returns 2,
//! which `process()` folds to 0 ahead of `if (status == 0) prec->udf =
//! isnan(prec->val);` (`aiRecord.c:158-161`). So a NaN SIOL value leaves the
//! record undefined, as a device returning 2 does.
//!
//! softIoc (EPICS 7.0.10), `SRC:nan` an ai at NaN, `SRC:fin` at 3.5, one
//! `dbpf <rec>.PROC 1` each; `A:flip` reads `SRC:fin`, then `SRC:nan`:
//!
//! ```text
//! A:nan  SIMM=YES UDF=1 STAT=UDF SEVR=INVALID
//! A:fin  SIMM=YES UDF=0 SEVR=NO_ALARM
//! A:raw  SIMM=RAW UDF=0
//! A:flip UDF=0, then UDF=1 SEVR=INVALID
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC:nan") { field(VAL, "NaN") }
record(ai, "SRC:fin") { field(VAL, "3.5") }
record(ai, "A:nan") { field(SIOL, "SRC:nan") field(SIMM, "YES") }
record(ai, "A:fin") { field(SIOL, "SRC:fin") field(SIMM, "YES") }
record(ai, "A:flip") { field(SIOL, "SRC:fin") field(SIMM, "YES") }
record(ai, "A:raw") { field(SIOL, "SRC:nan") field(SIMM, "RAW") }
"#;

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links(rec, &mut visited).await;
}

/// UDF, STAT, SEVR.
fn published(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
) -> (u8, u16, AlarmSeverity) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    (inst.common.udf, inst.common.stat, inst.common.sevr)
}

#[epics_macros_rs::epics_test]
async fn a_nan_siol_value_leaves_a_simulated_ai_undefined() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let undefined = (1, alarm_status::UDF_ALARM, AlarmSeverity::Invalid);
    let defined = (0, alarm_status::NO_ALARM, AlarmSeverity::NoAlarm);
    let mut wrong = Vec::new();
    for (rec, want) in [("A:nan", undefined), ("A:fin", defined), ("A:raw", defined)] {
        process(&db, rec).await;
        let got = published(&db, rec);
        if got != want {
            wrong.push(format!("{rec}: {got:?}, C {want:?}"));
        }
    }
    process(&db, "A:flip").await;
    let first = published(&db, "A:flip");
    db.put_record_field_from_ca_no_notify("A:flip", "SIOL", EpicsValue::String("SRC:nan".into()))
        .await
        .unwrap();
    process(&db, "A:flip").await;
    let second = published(&db, "A:flip");
    if (first, second) != (defined, undefined) {
        wrong.push(format!(
            "A:flip: {first:?} then {second:?}, C {defined:?} then {undefined:?}"
        ));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
