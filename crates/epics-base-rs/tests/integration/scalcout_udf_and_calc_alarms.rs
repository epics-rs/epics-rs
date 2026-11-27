//! sCalcout's two `sCalcPerform` failures, CALC (`sCalcoutRecord.c:357-367`)
//! and OCAL inside `execOutput` (`:768-774`), against oracleStdIoc. Only the
//! CALC pass decides UDF, and the OCAL alarm is raised after `checkAlarms`, so
//! an equal-severity UDF or limit alarm keeps the status.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

async fn ioc(db: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// `dbpf X.<field> <v>`: the put and the process `pp(TRUE)` asks for.
async fn dbpf(db: &PvDatabase, pv: &str, field: &str, v: EpicsValue) {
    let _ = db.put_record_field_from_ca_no_notify(pv, field, v).await;
}

/// UDF/STAT/SEVR of X, and one more X field.
fn snap(db: &PvDatabase, field: &str) -> (u8, u16, u16, f64) {
    let extra = db.get_pv(format!("X.{field}")).unwrap().to_f64().unwrap();
    let rec = db.get_record("X").unwrap();
    let g = rec.read();
    (g.common.udf, g.common.stat, g.common.sevr as u16, extra)
}

/// oracle: HHSV INVALID → `0/HIHI/INVALID`, LALM 5; HHSV MAJOR →
/// `0/CALC/INVALID`, LALM 5. OVAL -1 in both.
#[epics_base_rs::epics_test]
async fn an_ocal_failure_is_raised_after_the_ladder_and_leaves_udf_to_calc() {
    for (hhsv, stat) in [("INVALID", 3), ("MAJOR", 12)] {
        let db = ioc(&format!(
            r#"record(scalcout, "X") {{
  field(CALC, "7")
  field(HIHI, "5")
  field(HHSV, "{hhsv}")
  field(DOPT, "Use OCAL")
  field(OCAL, "A[")
}}"#
        ))
        .await;
        db.process_record("X").await.unwrap();
        assert_eq!(snap(&db, "LALM"), (0, stat, 3, 5.0), "HHSV {hhsv}");
        assert_eq!(snap(&db, "OVAL").3, -1.0);
    }
}

/// A cycle whose fetch fails still runs `execOutput`; its OCAL failure is
/// that cycle's alarm, not the next one's. oracle: `1/LINK/INVALID` OVAL -1,
/// then with S defined and OCAL `A`, `0/NO_ALARM/NO_ALARM` OVAL 3.
#[epics_base_rs::epics_test]
async fn an_ocal_failure_on_a_gated_cycle_stays_in_that_cycle() {
    let db = ioc(r#"record(stringin, "S") {
  field(VAL, "abc")
}
record(scalcout, "X") {
  field(INPA, "S NPP")
  field(CALC, "7")
  field(DOPT, "Use OCAL")
  field(OCAL, "A[")
}"#)
    .await;
    db.process_record("X").await.unwrap();
    assert_eq!(snap(&db, "OVAL"), (1, 14, 3, -1.0));
    db.put_pv("S", EpicsValue::String("3".into()))
        .await
        .unwrap();
    dbpf(&db, "X", "OCAL", EpicsValue::String("A".into())).await;
    assert_eq!(snap(&db, "OVAL"), (0, 0, 0, 3.0));
}

/// `checkAlarms` returns early only on `udf == TRUE` (`sCalcoutRecord.c:705`),
/// so a UDF byte of 2 runs the ladder and latches LALM. oracle: `UDF 2` →
/// `2/CALC/INVALID` LALM -1, `UDF 1` → `1/CALC/INVALID` LALM -1.
#[epics_base_rs::epics_test]
async fn the_ladder_runs_unless_udf_is_exactly_one() {
    let db = ioc(r#"record(scalcout, "X") {
  field(CALC, "A[")
}"#)
    .await;
    dbpf(&db, "X", "UDF", EpicsValue::Double(2.0)).await;
    assert_eq!(snap(&db, "LALM"), (2, 12, 3, -1.0));
    dbpf(&db, "X", "UDF", EpicsValue::Double(1.0)).await;
    assert_eq!(snap(&db, "LALM"), (1, 12, 3, -1.0));
}
