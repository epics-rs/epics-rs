//! R11-61 — an aCalcout array input link (INAA..INLL) must deliver the SOURCE
//! ARRAY into AA..LL.
//!
//! C `aCalcoutRecord.c::fetch_values` (1076-1099) reads each configured INAA..INLL
//! with `dbGetLink(plink, DBR_DOUBLE, *pavalue, 0, &nRequest)` where `nRequest` is
//! the record's element count, then zero-fills the tail. The framework's
//! multi-input apply loop collapsed every fetched value through
//! `EpicsValue::to_f64()`, which answers `None` for every array variant — so the
//! array never reached `put_field` and AA..LL stayed empty. `CALC="SUM(AA)"` on a
//! waveform input therefore computed 0.
//!
//! The boundaries covered here:
//!   * array source -> array field  (AA), full array delivered
//!   * array source -> scalar field (A),  element 0 delivered (C's one-element
//!     destination: `dbGetLink(..., DBR_DOUBLE, pvalue, 0, 0)`)
//!   * scalar source -> scalar field, unchanged (negative control)

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

async fn wf_source(db: &PvDatabase, name: &str, data: Vec<f64>) {
    let mut wf = WaveformRecord::new(data.len() as i32, DbFieldType::Double);
    wf.put_field("VAL", EpicsValue::DoubleArray(data)).unwrap();
    db.add_record(name, Box::new(wf)).await.unwrap();
}

/// INAA -> a 5-element waveform, CALC="SUM(AA)". Compiled C (aCalcPerform,
/// arraySize 5, AA=[1..5]): dresult=15.
#[epics_macros_rs::epics_test]
async fn r11_61_array_input_link_populates_aa() {
    let db = PvDatabase::new();
    wf_source(&db, "WF", vec![1.0, 2.0, 3.0, 4.0, 5.0]).await;

    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(5)).unwrap();
    a.put_field("CALC", EpicsValue::String("SUM(AA)".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("INAA", EpicsValue::String("WF".into()))
        .unwrap();
    db.add_record("ACALC", Box::new(a)).await.unwrap();

    process(&db, "ACALC").await;

    assert_eq!(
        field(&db, "ACALC", "AA").await,
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
        "INAA must deliver the whole waveform into AA"
    );
    assert_eq!(field(&db, "ACALC", "VAL").await.to_f64().unwrap(), 15.0);
}

/// A source shorter than the record's element count zero-fills the tail
/// (C: `for (j=nRequest; j<numElements; j++) (*pavalue)[j] = 0;`).
#[epics_macros_rs::epics_test]
async fn r11_61_short_array_source_zero_fills_the_tail() {
    let db = PvDatabase::new();
    wf_source(&db, "WF2", vec![7.0, 8.0]).await;

    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(5)).unwrap();
    a.put_field("CALC", EpicsValue::String("SUM(AA)".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("INAA", EpicsValue::String("WF2".into()))
        .unwrap();
    db.add_record("ACALC2", Box::new(a)).await.unwrap();

    process(&db, "ACALC2").await;

    assert_eq!(
        field(&db, "ACALC2", "AA").await,
        EpicsValue::DoubleArray(vec![7.0, 8.0, 0.0, 0.0, 0.0])
    );
    assert_eq!(field(&db, "ACALC2", "VAL").await.to_f64().unwrap(), 15.0);
}

/// An array source feeding a SCALAR value field is a one-element destination in
/// C — it takes element 0, not nothing.
#[epics_macros_rs::epics_test]
async fn r11_61_array_source_into_scalar_field_takes_element_zero() {
    let db = PvDatabase::new();
    wf_source(&db, "WF3", vec![4.0, 99.0, 99.0]).await;

    let mut c = CalcRecord::new("A+1");
    c.put_field("INPA", EpicsValue::String("WF3".into()))
        .unwrap();
    db.add_record("C1", Box::new(c)).await.unwrap();

    process(&db, "C1").await;

    assert_eq!(field(&db, "C1", "A").await.to_f64().unwrap(), 4.0);
    assert_eq!(field(&db, "C1", "VAL").await.to_f64().unwrap(), 5.0);
}

/// Negative control: a scalar source still lands as a scalar Double.
#[epics_macros_rs::epics_test]
async fn r11_61_scalar_source_unchanged() {
    let db = PvDatabase::new();
    db.add_record(
        "AI1",
        Box::new(epics_base_rs::server::records::ai::AiRecord::new(2.5)),
    )
    .await
    .unwrap();

    let mut c = CalcRecord::new("A*2");
    c.put_field("INPA", EpicsValue::String("AI1".into()))
        .unwrap();
    db.add_record("C2", Box::new(c)).await.unwrap();

    process(&db, "C2").await;

    assert_eq!(field(&db, "C2", "A").await, EpicsValue::Double(2.5));
    assert_eq!(field(&db, "C2", "VAL").await.to_f64().unwrap(), 5.0);
}
