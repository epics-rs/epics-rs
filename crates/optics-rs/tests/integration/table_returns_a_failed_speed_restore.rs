//! A failed speed put ends table's put loops as C's do, against
//! oracleTableIoc with `dbpf T.X 1` then `dbpf T.X 2`: `ProcessOutputLinks`
//! returns at the failed speed put, so no drive is put, and
//! `RestoreMotorSpeeds`' failed put is what `process()` returns, with PACT
//! still set and no readback, monitor or forward link (`tableRecord.c:573-576`,
//! `:937-950`, `:998-1006`). The second put finds the record active.
//!
//! `T:VF` fails every process; `dbDbPutValue` returns its status through
//! `processTarget` (`dbDbLink.c:387-392`).

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(ao, "T:RB") {
}
record(ao, "T:VI") {
  field(VAL, "2")
}
record(sub, "T:VF") {
  field(SNAM, "minusOne")
}
record(ao, "T:VOK") {
}
record(calc, "T:F") {
  field(INPA, "T:F")
  field(CALC, "A+1")
}
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
  field(M0XL, "T:RB")
  field(R0XI, "T:RB")
  field(E0XI, "T:RB")
  field(V0XI, "T:VI")
  field(V0XL, "$(V)")
  field(FLNK, "T:F")
}
"#;

async fn ioc(v0xl: &str) -> Arc<PvDatabase> {
    let macros = HashMap::from([("V".to_string(), v0xl.to_string())]);
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_subroutine("minusOne", |_| Ok(-1))
        .db_string(DB, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn get(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[tokio::test]
async fn a_failed_restore_is_process_status_with_pact_held() {
    let db = ioc("T:VF PP").await;
    let put = db
        .put_record_field_from_ca_no_notify("T", "X", EpicsValue::Double(1.0))
        .await;
    assert!(
        matches!(put, Err(CaError::ProcessStatus(s)) if s.get() == -1),
        "RestoreMotorSpeeds' status, got {put:?}"
    );
    assert!(db.get_record("T").unwrap().read().is_processing(), "PACT 1");
    // oracle: T:F 0, T:RB 0, T:VF 2, T.XL 0, T.XRB 0.
    assert_eq!(get(&db, "T:F"), 0.0, "no forward link");
    assert_eq!(get(&db, "T:RB"), 0.0, "ProcessOutputLinks put no drive");
    assert_eq!(get(&db, "T:VF"), 2.0, "the restore was put");
    assert_eq!(get(&db, "T.XL"), 0.0, "no user-coordinate save");
    assert_eq!(get(&db, "T.XRB"), 0.0, "no readback");

    // oracle: "special: Record is active", T:F 0, T:RB 0, T.X 2.
    let _ = db
        .put_record_field_from_ca_no_notify("T", "X", EpicsValue::Double(2.0))
        .await;
    assert_eq!(get(&db, "T:F"), 0.0);
    assert_eq!(get(&db, "T:RB"), 0.0);
    assert_eq!(get(&db, "T.X"), 2.0);
}

#[tokio::test]
async fn a_landed_restore_finishes_the_cycle() {
    let db = ioc("T:VOK PP").await;
    let put = db
        .put_record_field_from_ca_no_notify("T", "X", EpicsValue::Double(1.0))
        .await;
    assert!(put.is_ok(), "got {put:?}");
    assert!(
        !db.get_record("T").unwrap().read().is_processing(),
        "PACT 0"
    );
    // oracle: T:F 1, T:RB 1, T:VOK 2, T.XL 1, T.XRB 1.00125157279.
    assert_eq!(get(&db, "T:F"), 1.0);
    assert_eq!(get(&db, "T:RB"), 1.0);
    assert_eq!(get(&db, "T:VOK"), 2.0);
    assert_eq!(get(&db, "T.XL"), 1.0);
    assert!((get(&db, "T.XRB") - 1.00125157279).abs() < 1e-10);

    db.put_record_field_from_ca_no_notify("T", "X", EpicsValue::Double(2.0))
        .await
        .unwrap();
    // oracle: T:F 2, T:RB 1, T.X 1.
    assert_eq!(get(&db, "T:F"), 2.0);
    assert_eq!(get(&db, "T:RB"), 1.0);
    assert_eq!(get(&db, "T.X"), 1.0);
}
