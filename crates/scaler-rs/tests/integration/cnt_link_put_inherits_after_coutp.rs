//! An MS OUT link writing `SCAL.CNT` from a MAJOR source. C `dbDbPutValue`
//! runs `dbPut` first — whose `special()` puts COUTP (`scalerRecord.c:625`)
//! — and only then inherits the source's alarm into the scaler
//! (`dbDbLink.c:378-383`). So COUTP's MS target sees the scaler's alarm
//! before the inherit, NO_ALARM here; inheriting first would hand it the
//! source's MAJOR as LINK/MAJOR.

// RTEMS-EXEC-MODEL-ALLOW(1): no CA server; the reactor is `#[tokio::test]`'s own, which the exec backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "TGT") {}
record(scaler, "SCAL") { field(FREQ, "10000000") field(TP, "1") field(COUTP, "TGT.VAL PP MS") }
record(ao, "SRC") { field(HIHI, "10") field(HHSV, "MAJOR") field(OUT, "SCAL.CNT NPP MS") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[tokio::test]
async fn coutp_sees_the_scaler_alarm_from_before_the_inherit() {
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(20.0))
        .await
        .unwrap();
    assert_eq!(
        (num(&db, "SRC.STAT"), num(&db, "SRC.SEVR")),
        (3.0, 2.0),
        "HIHI/MAJOR"
    );
    assert_eq!(num(&db, "TGT.UDF"), 0.0, "COUTP processed TGT");
    assert_eq!(
        (num(&db, "TGT.STAT"), num(&db, "TGT.SEVR")),
        (0.0, 0.0),
        "COUTP carried the scaler's pre-inherit alarm"
    );
}
