//! A put of VAL processes the record, and only a dset that returns 2 (Soft
//! Channel) keeps the put value; Raw Soft Channel converts from RVAL. Every
//! number is softIoc's `dbgf` after `dbpf <rec>.VAL <v>` (R7.0.10-146).

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "AIR") { field(DTYP, "Raw Soft Channel") field(RVAL, "3") }
record(ai, "AIS") { field(DTYP, "Soft Channel") field(RVAL, "3") }
record(bi, "BIR") { field(DTYP, "Raw Soft Channel") field(RVAL, "0") field(ZNAM, "zero") field(ONAM, "one") }
record(bi, "BIS") { field(DTYP, "Soft Channel") field(RVAL, "0") field(ZNAM, "zero") field(ONAM, "one") }
record(mbbi, "MBR") { field(DTYP, "Raw Soft Channel") field(RVAL, "3") field(NOBT, "4")
    field(ZRST, "a") field(ONST, "b") field(TWST, "c") field(THST, "d")
    field(ZRVL, "0") field(ONVL, "1") field(TWVL, "2") field(THVL, "3") }
record(mbbi, "MBS") { field(DTYP, "Soft Channel") field(RVAL, "3") field(NOBT, "4")
    field(ZRST, "a") field(ONST, "b") field(TWST, "c") field(THST, "d")
    field(ZRVL, "0") field(ONVL, "1") field(TWVL, "2") field(THVL, "3") }
record(mbbiDirect, "MDR") { field(DTYP, "Raw Soft Channel") field(RVAL, "3") field(NOBT, "8") }
record(mbbiDirect, "MDS") { field(DTYP, "Soft Channel") field(RVAL, "3") field(NOBT, "8") }
"#;

/// (record, VAL put, softIoc VAL after the put).
const C_PINS: &[(&str, f64, f64)] = &[
    ("AIR", 7.0, 3.0),
    ("AIS", 7.0, 7.0),
    ("BIR", 1.0, 0.0),
    ("BIS", 1.0, 1.0),
    ("MBR", 2.0, 3.0),
    ("MBS", 2.0, 2.0),
    ("MDR", 7.0, 3.0),
    ("MDS", 7.0, 7.0),
];

#[epics_macros_rs::epics_test]
async fn raw_soft_val_put_converts_ai_3_bi_0_mbbi_3_mbbidirect_3_soft_keeps_put() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for &(rec, put, want) in C_PINS {
        db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(put))
            .await
            .unwrap();
        let got = db.get_pv(rec).unwrap().to_f64().unwrap();
        assert_eq!(got, want, "dbpf {rec}.VAL {put}");
    }
}
