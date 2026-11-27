//! A scalar `DBR_ENUM` read (`nRequest` NULL) takes `dbGet`'s scalar
//! convert of element 0 (`dbAccess.c:967`): an empty source is
//! `S_db_onlyOne`, and element 0 of a string array is `cvt_st_e` against
//! the source, which a waveform cannot answer (no `put_enum_str`).
//!
//! oracleSwaitIoc (calc R3-7), swait reading SIML at `DBR_ENUM`
//! (`swaitRecord.c:402`):
//!
//! ```text
//! SWS  SIML WE   [5,6]       SIMM 5   NO_ALARM       VAL 0
//! SWS  SIML WE   []          SIMM 5   LINK/INVALID   VAL 0
//! SWS2 SIML WES2 ["1","2"]   SIMM NO  LINK/INVALID   VAL 1
//! ```

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "WES2") { field(FTVL, "STRING") field(NELM, "4") }
record(swait, "SWS") { field(SIML, "WE") field(CALC, "1") }
record(swait, "SWS2") { field(CALC, "1") field(SIML, "WES2 NPP") field(SIOL, "1") }
"#;

/// Process `rec` and return `(SIMM, STAT, SEVR as u16, VAL)`.
async fn read(db: &PvDatabase, rec: &str) -> (f64, u16, u16, f64) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links(rec, &mut visited).await;
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    let f = |name: &str| inst.record.get_field(name).unwrap().to_f64().unwrap();
    (
        f("SIMM"),
        inst.common.stat,
        inst.common.sevr as u16,
        f("VAL"),
    )
}

#[epics_macros_rs::epics_test]
async fn a_scalar_enum_read_converts_element_0_or_fails() {
    let (db, _) = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_pv("WE", EpicsValue::DoubleArray(vec![5.0, 6.0].into()))
        .await
        .unwrap();
    db.put_pv(
        "WES2",
        EpicsValue::StringArray(vec![PvString::from("1"), PvString::from("2")].into()),
    )
    .await
    .unwrap();
    let full = read(&db, "SWS").await;
    db.put_pv("WE", EpicsValue::DoubleArray(Vec::new().into()))
        .await
        .unwrap();
    let emptied = read(&db, "SWS").await;
    let strings = read(&db, "SWS2").await;
    assert_eq!(
        [full, emptied, strings],
        [(5.0, 0, 0, 0.0), (5.0, 14, 3, 0.0), (0.0, 14, 3, 1.0)],
        "(SIMM, STAT, SEVR, VAL) of SWS filled, SWS emptied, SWS2"
    );
}
