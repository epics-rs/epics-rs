//! lso's simulated output is C `dbPutLinkLS(&prec->siol, ...)`
//! (`lsoRecord.c:286`), as its OUT is: a `DBF_CHAR` target takes the
//! characters, any other one `DBR_STRING`. Pinned to softIoc R7.0.10.1:
//! `dbpf L1 "hello world"` leaves SO_T "hello world", `dbpf L2 "abc"`
//! leaves WF_T `DBF_CHAR[4] "abc"`, both lso NO_ALARM.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "SO_T") {}
record(waveform, "WF_T") { field(FTVL, "CHAR") field(NELM, "64") }
record(lso, "L1") { field(SIZV, "64") field(SIMM, "YES") field(SIOL, "SO_T") }
record(lso, "L2") { field(SIZV, "64") field(SIMM, "YES") field(SIOL, "WF_T") }
"#;

#[epics_macros_rs::epics_test]
async fn lso_siol_puts_a_long_string_as_softioc_does() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca("L1", "VAL", EpicsValue::String("hello world".into()))
        .await
        .unwrap();
    db.put_record_field_from_ca("L2", "VAL", EpicsValue::String("abc".into()))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("SO_T").unwrap(),
        EpicsValue::String("hello world".into())
    );
    assert_eq!(
        db.get_pv("WF_T").unwrap(),
        EpicsValue::CharArray(b"abc\0".to_vec().into())
    );
    assert_eq!(db.get_pv("WF_T.NORD").unwrap().to_f64(), Some(4.0));
    for rec in ["L1", "L2"] {
        assert_eq!(
            db.get_pv(format!("{rec}.SEVR")).unwrap().to_f64(),
            Some(0.0),
            "{rec}"
        );
    }
}
