//! A scalar input link reads with `nRequest = NULL`, and `dbGet` answers that
//! request on a source holding no elements with `S_db_onlyOne`
//! (`dbAccess.c:967-973`): a LINK/INVALID read that stores nothing.
//!
//! oracleSwaitIoc (EPICS 7.0.10, calc R3-7), `WE` a DOUBLE waveform filled
//! with [5,6], every reader processed, then `WE` emptied and every reader
//! processed again:
//!
//! ```text
//! C VAL 6  CO VAL 6  SC VAL 6  AC VAL 6  SEL VAL 5  SELH VAL 5  HIST SGNL 5
//! TR A 0 B 1  TRB A 1                      all LINK/INVALID
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::{
    acalcout::AcalcoutRecord, scalcout::ScalcoutRecord, transform::TransformRecord,
};
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(calc, "C") { field(INPA, "WE NPP") field(CALC, "A+1") }
record(calcout, "CO") { field(INPA, "WE NPP") field(CALC, "A+1") }
record(scalcout, "SC") { field(INPA, "WE NPP") field(CALC, "A+1") }
record(acalcout, "AC") { field(NELM, "4") field(INPA, "WE NPP") field(CALC, "A+1") }
record(transform, "TR") { field(INPA, "WE NPP") field(CLCB, "a+1") }
record(transform, "TRB") { field(INPB, "WE NPP") field(CLCA, "b+1") }
record(sel, "SEL") { field(SELM, "Specified") field(SELN, "0") field(INPA, "WE NPP") }
record(sel, "SELH") { field(SELM, "High Signal") field(INPA, "WE NPP") field(INPB, "2") }
record(histogram, "HIST") { field(SVL, "WE NPP") field(NELM, "4") }
"#;

/// (record, field, C after the empty read)
const C: &[(&str, &str, f64)] = &[
    ("C", "VAL", 6.0),
    ("CO", "VAL", 6.0),
    ("SC", "VAL", 6.0),
    ("AC", "VAL", 6.0),
    ("TR", "A", 0.0),
    ("TR", "B", 1.0),
    ("TRB", "A", 1.0),
    ("SEL", "VAL", 5.0),
    ("SELH", "VAL", 5.0),
    ("HIST", "SGNL", 5.0),
];

async fn process_all(db: &PvDatabase, we: Vec<f64>) {
    db.put_record_field_from_ca_no_notify("WE", "VAL", EpicsValue::DoubleArray(we.into()))
        .await
        .unwrap();
    for rec in ["C", "CO", "SC", "AC", "TR", "TRB", "SEL", "SELH", "HIST"] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
    }
}

#[epics_macros_rs::epics_test]
async fn a_scalar_read_of_an_emptied_array_is_a_link_alarm() {
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    process_all(&db, vec![5.0, 6.0]).await;
    process_all(&db, Vec::new()).await;
    let mut wrong = Vec::new();
    for &(rec, field, want) in C {
        let r = db.get_record(rec).unwrap();
        let inst = r.read();
        let got = inst.record.get_field(field).unwrap().to_f64().unwrap();
        let alarm = (inst.common.stat, inst.common.sevr);
        if got != want || alarm != (alarm_status::LINK_ALARM, AlarmSeverity::Invalid) {
            wrong.push(format!(
                "{rec}.{field} {got} {alarm:?}, C {want} LINK/INVALID"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
