//! A stringin INP read is `dbGetLink(..., DBR_STRING, ...)` of ONE element:
//! a float/double source renders at its PREC (`getDoubleString`), an array
//! source converts element 0 and fails the read at NORD 0, and a long-string
//! source (`DBF_STRING` to `cvt_dbaddr`) copies its text capped at 39 bytes.
//! Expected values are C softIoc's for the same database.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn build() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
            record(ai, "S:AI") { field(PREC, "2") field(VAL, "5") }
            record(ai, "S:AI0") { field(VAL, "3.14159") }
            record(lsi, "S:LSI") { field(SIZV, "100") }
            record(waveform, "S:WF") { field(FTVL, "CHAR") field(NELM, "60") }
            record(waveform, "S:WFD") { field(FTVL, "DOUBLE") field(NELM, "4") field(PREC, "2") }
            record(waveform, "S:WFE") { field(FTVL, "DOUBLE") field(NELM, "4") }
            record(stringin, "R:AI") { field(INP, "S:AI") }
            record(stringin, "R:AI0") { field(INP, "S:AI0") }
            record(stringin, "R:LSI") { field(INP, "S:LSI") }
            record(stringin, "R:WF") { field(INP, "S:WF") }
            record(stringin, "R:WFD") { field(INP, "S:WFD") }
            record(stringin, "R:WFE") { field(INP, "S:WFE") field(VAL, "keep") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_pv(
        "S:LSI.VAL",
        EpicsValue::String("0123456789abcdefghijABCDEFGHIJ0123456789xyz".into()),
    )
    .await
    .unwrap();
    db.put_pv(
        "S:WF.VAL",
        EpicsValue::CharArray(b"0ABCDEFGH".to_vec().into()),
    )
    .await
    .unwrap();
    db.put_pv("S:WFD.VAL", EpicsValue::DoubleArray(vec![66.4, 2.0].into()))
        .await
        .unwrap();
    db
}

async fn read(db: &PvDatabase, rec: &str) -> (String, EpicsValue) {
    let mut v = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut v).await.unwrap();
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    let val = match inst.record.get_field("VAL").unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}.VAL is not a string: {other:?}"),
    };
    (val, inst.get_common_field("SEVR").unwrap())
}

#[epics_macros_rs::epics_test]
async fn a_dbr_string_link_read_renders_as_c_does() {
    let db = build().await;
    for (rec, want) in [
        ("R:AI", "5.00"),
        ("R:AI0", "3"),
        ("R:LSI", "0123456789abcdefghijABCDEFGHIJ012345678"),
        ("R:WF", "48"),
        ("R:WFD", "66.40"),
    ] {
        let (val, sevr) = read(&db, rec).await;
        assert_eq!(val, want, "{rec}.VAL");
        assert_eq!(sevr, EpicsValue::Short(0), "{rec}.SEVR");
    }
    let (val, sevr) = read(&db, "R:WFE").await;
    assert_eq!(val, "keep", "a NORD 0 source leaves VAL");
    assert_eq!(
        sevr,
        EpicsValue::Short(3),
        "a NORD 0 source is an INVALID read"
    );
}
