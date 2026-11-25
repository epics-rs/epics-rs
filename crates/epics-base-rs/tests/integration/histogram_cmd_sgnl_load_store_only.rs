//! A `.db` value for histogram CMD or SGNL is stored, not acted on: the loader
//! never calls `special()`, which is where C runs CMD's SPC_CALC switch
//! (histogramRecord.c:245-259) and SGNL's SPC_MOD `add_count` (:261-263).
//! Expected values are C softIoc's `dbgf`/`dbpf` output for this database.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(histogram, "H") { field(CMD, "Clear") }
record(histogram, "HS") { field(CMD, "Stop") }
record(histogram, "HG") { field(SGNL, "5") field(ULIM, "10") field(NELM, "10") }
"#;

fn stored(db: &PvDatabase, rec: &str, field: &str) -> EpicsValue {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(field)
        .unwrap()
}

fn udf(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(format!("{rec}.UDF")).unwrap().to_f64().unwrap()
}

fn bins(db: &PvDatabase) -> Vec<u32> {
    match stored(db, "HG", "VAL") {
        EpicsValue::ULongArray(v) => v.to_vec(),
        v => panic!("{v:?}"),
    }
}

async fn dbpf(db: &PvDatabase, rec: &str, field: &str, value: &str) {
    db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn loaded_cmd_and_sgnl_are_stored_and_dbpf_acts_as_c() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(stored(&db, "H", "CMD"), EpicsValue::Short(1), "Clear");
    assert_eq!(stored(&db, "H", "CSTA"), EpicsValue::Short(1));
    assert_eq!(stored(&db, "HS", "CMD"), EpicsValue::Short(3), "Stop");
    assert_eq!(stored(&db, "HS", "CSTA"), EpicsValue::Short(1));
    assert_eq!(bins(&db), vec![0; 10]);
    assert_eq!(stored(&db, "HG", "MCNT"), EpicsValue::Short(0));
    assert_eq!(udf(&db, "HG"), 1.0);
    assert_eq!(stored(&db, "HG", "SGNL"), EpicsValue::Double(5.0));

    dbpf(&db, "H", "CMD", "Stop").await;
    assert_eq!(stored(&db, "H", "CMD"), EpicsValue::Short(0), "Read");
    assert_eq!(stored(&db, "H", "CSTA"), EpicsValue::Short(0));
    dbpf(&db, "H", "CMD", "Start").await;
    assert_eq!(stored(&db, "H", "CMD"), EpicsValue::Short(0));
    assert_eq!(stored(&db, "H", "CSTA"), EpicsValue::Short(1));
    dbpf(&db, "H", "CMD", "Clear").await;
    assert_eq!(stored(&db, "H", "CMD"), EpicsValue::Short(0));

    dbpf(&db, "HG", "SGNL", "3").await;
    let mut want = vec![0; 10];
    want[2] = 1;
    assert_eq!(bins(&db), want);
    assert_eq!(stored(&db, "HG", "MCNT"), EpicsValue::Short(1));
    assert_eq!(udf(&db, "HG"), 1.0);
    dbpf(&db, "HG", "CMD", "Clear").await;
    assert_eq!(bins(&db), vec![0; 10]);
    assert_eq!(stored(&db, "HG", "CMD"), EpicsValue::Short(0));
    assert_eq!(udf(&db, "HG"), 0.0);
}
