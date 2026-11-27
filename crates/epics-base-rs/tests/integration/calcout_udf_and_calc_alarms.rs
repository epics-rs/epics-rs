//! calcout's two UDF writes against oracleStdIoc: the CALC pass's
//! `udf = isnan(val)` ahead of `checkAlarms` (`calcoutRecord.c:237-244`), and
//! `execOutput`'s after it (`:613-630`) — `udf = isnan(oval)` on an OCAL
//! success, CALC_ALARM "OCAL calcPerform" on a failure, then `if (udf)`
//! UDF_ALARM. So the field ends at the OCAL verdict, and an equal-severity
//! alarm `checkAlarms` raised keeps the status.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn ioc(fields: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(
            &format!("record(calcout, \"X\") {{\n{fields}\n}}\n"),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// `dbpf X.<field> <v>`: the put and the process `pp(TRUE)` asks for.
async fn dbpf(db: &PvDatabase, field: &str, v: &str) {
    let _ = db
        .put_record_field_from_ca_no_notify("X", field, EpicsValue::String(v.into()))
        .await;
}

/// Until X leaves PACT: the ODLY expiry runs from a callback.
async fn idle(db: &PvDatabase) {
    let cell = db.get_record("X").unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while cell.read().is_processing() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("X never left PACT");
}

/// UDF/STAT/SEVR of X, and one more X field.
fn snap(db: &PvDatabase, field: &str) -> (u8, u16, u16, f64) {
    let extra = db.get_pv(format!("X.{field}")).unwrap().to_f64().unwrap();
    let rec = db.get_record("X").unwrap();
    let g = rec.read();
    (g.common.udf, g.common.stat, g.common.sevr as u16, extra)
}

/// oracle: a NaN CALC with OCAL `1` → `0/UDF/INVALID` (UDFS MAJOR:
/// `0/UDF/MAJOR`); CALC 5 over HIHI 1 with OCAL `0/0` → HHSV INVALID
/// `1/HIHI/INVALID`, HHSV MAJOR `1/UDF/INVALID`, LALM 1 in both.
#[epics_base_rs::epics_test]
async fn the_ocal_pass_writes_udf_after_check_alarms() {
    for (udfs, sevr) in [("INVALID", 3), ("MAJOR", 2)] {
        let db = ioc(&format!(
            "field(CALC, \"0/0\")\nfield(DOPT, \"Use OCAL\")\nfield(OCAL, \"1\")\nfield(UDFS, \"{udfs}\")"
        ))
        .await;
        db.process_record("X").await.unwrap();
        assert_eq!(snap(&db, "OVAL"), (0, 17, sevr, 1.0), "UDFS {udfs}");
    }
    for (hhsv, stat) in [("INVALID", 3), ("MAJOR", 17)] {
        let db = ioc(&format!(
            "field(CALC, \"5\")\nfield(HIHI, \"1\")\nfield(HHSV, \"{hhsv}\")\nfield(DOPT, \"Use OCAL\")\nfield(OCAL, \"0/0\")"
        ))
        .await;
        db.process_record("X").await.unwrap();
        assert_eq!(snap(&db, "LALM"), (1, stat, 3, 1.0), "HHSV {hhsv}");
    }
}

/// oracle: CALC 5 over HIHI 1 (INVALID) → `0/HIHI/INVALID`, and still after
/// OCAL `A[`; a NaN CALC → `0/UDF/INVALID`, then OCAL `A[` → `1/UDF/INVALID`.
#[epics_base_rs::epics_test]
async fn an_ocal_failure_is_raised_after_check_alarms() {
    for (calc, before, after) in [("5", (0, 3, 3), (0, 3, 3)), ("0/0", (0, 17, 3), (1, 17, 3))] {
        let db = ioc(&format!(
            "field(CALC, \"{calc}\")\nfield(HIHI, \"1\")\nfield(HHSV, \"INVALID\")\nfield(DOPT, \"Use OCAL\")\nfield(OCAL, \"1\")"
        ))
        .await;
        db.process_record("X").await.unwrap();
        let (u, st, sv, _) = snap(&db, "OVAL");
        assert_eq!((u, st, sv), before, "CALC {calc}");
        dbpf(&db, "OCAL", "A[").await;
        let (u, st, sv, _) = snap(&db, "OVAL");
        assert_eq!((u, st, sv), after, "CALC {calc}, OCAL A[");
    }
}

/// The ODLY expiry runs only `execOutput` (`:291-297`). oracle: a NaN CALC
/// with OCAL `1` → `0/UDF/INVALID` OVAL 1; CALC 5 over HIHI 1 (INVALID) with
/// OCAL `A` → `0/HIHI/INVALID`, and still after OCAL `A[`.
#[epics_base_rs::epics_test]
async fn the_delayed_exec_output_writes_udf_after_check_alarms() {
    let db = ioc(
        "field(CALC, \"0/0\")\nfield(DOPT, \"Use OCAL\")\nfield(OCAL, \"1\")\nfield(ODLY, \"0.1\")",
    )
    .await;
    db.process_record("X").await.unwrap();
    idle(&db).await;
    assert_eq!(snap(&db, "OVAL"), (0, 17, 3, 1.0));

    let db = ioc(
        "field(CALC, \"5\")\nfield(HIHI, \"1\")\nfield(HHSV, \"INVALID\")\nfield(DOPT, \"Use OCAL\")\nfield(OCAL, \"A\")\nfield(ODLY, \"0.1\")",
    )
    .await;
    db.process_record("X").await.unwrap();
    idle(&db).await;
    assert_eq!(snap(&db, "LALM"), (0, 3, 3, 1.0));
    dbpf(&db, "OCAL", "A[").await;
    idle(&db).await;
    assert_eq!(snap(&db, "LALM"), (0, 3, 3, 1.0));
}
