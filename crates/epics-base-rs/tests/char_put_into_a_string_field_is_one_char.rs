//! A `DBR_CHAR` put under a plain name into a `DBF_STRING` field is C's char
//! row: `dbPut` clamps the count to the field's one element and
//! `putCharString` renders that char as a number.
//!
//! Measured against `softIoc` R7.0.10 with a count-1 `DBR_CHAR` CA put: `'0'`
//! into `calc.DESC` reads back `"48"`, `'A'` into `stringout.VAL` `"65"`. A
//! longer char put never reaches `dbPut` over CA (`ECA_BADCOUNT`); in-process
//! it takes the same clamp at `dbAccess.c:1360`. The text form of a string
//! field is its `$` name, which converts before this row.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(calc, "OC") { field(CALC, "1") }
record(stringout, "OS") { }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn put_chars(db: &PvDatabase, rec: &str, field: &str, chars: &[u8]) -> String {
    db.put_record_field_from_ca_no_notify(rec, field, EpicsValue::CharArray(chars.to_vec().into()))
        .await
        .expect("put");
    match db.get_pv(format!("{rec}.{field}")).expect("get") {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{rec}.{field} is {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn one_char_stores_its_number() {
    let db = build().await;
    assert_eq!(put_chars(&db, "OC", "DESC", b"0").await, "48");
    assert_eq!(put_chars(&db, "OS", "VAL", b"A").await, "65");
    assert_eq!(put_chars(&db, "OS", "DESC", &[0]).await, "0");
}

#[epics_macros_rs::epics_test]
async fn a_longer_put_keeps_only_its_first_char() {
    let db = build().await;
    assert_eq!(put_chars(&db, "OC", "DESC", b"012\0").await, "48");
    assert_eq!(put_chars(&db, "OS", "VAL", &[b'x'; 100]).await, "120");
}
