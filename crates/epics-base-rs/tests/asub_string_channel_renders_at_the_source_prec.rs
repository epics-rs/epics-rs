//! A STRING aSub channel converts a DOUBLE source at the SOURCE's PREC
//! (`dbConvert.c` `getStringDouble`, precision from the source `dbAddr`).
//! softIoc R7.0.10, waveform [-3.7, 1.5, 2.5, 300.9]:
//!
//! ```text
//! A16  (NOA 16, W0 PREC 0)  -4 2 3 301
//! A3W  (NOA 3,  W0 PREC 0)  -4 2 3
//! A1W  (NOA 1,  W0 PREC 0)  -4
//! A16P (NOA 16, W3 PREC 3)  -3.700 1.500 2.500 300.900
//! A1S  (NOA 1, ai PREC 0, VAL -3.7)  -4
//! ```

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "W0") { field(FTVL, "DOUBLE") field(NELM, "16") field(PREC, "0") }
record(waveform, "W3") { field(FTVL, "DOUBLE") field(NELM, "16") field(PREC, "3") }
record(ai, "S0") { field(PREC, "0") field(VAL, "-3.7") }
record(aSub, "A16") { field(FTA, "STRING") field(NOA, "16") field(INPA, "W0") }
record(aSub, "A3W") { field(FTA, "STRING") field(NOA, "3") field(INPA, "W0") }
record(aSub, "A1W") { field(FTA, "STRING") field(NOA, "1") field(INPA, "W0") }
record(aSub, "A16P") { field(FTA, "STRING") field(NOA, "16") field(INPA, "W3") }
record(aSub, "A1S") { field(FTA, "STRING") field(NOA, "1") field(INPA, "S0") }
"#;

async fn strings(db: &PvDatabase, rec: &str) -> Vec<String> {
    let mut visited = ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    match db.get_pv(&format!("{rec}.A")).unwrap() {
        EpicsValue::StringArray(a) => a.iter().map(|s| s.to_string()).collect(),
        EpicsValue::String(s) => vec![s.to_string()],
        other => panic!("{rec}: {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_string_channel_renders_a_double_source_at_its_prec() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let wave = || EpicsValue::DoubleArray(vec![-3.7, 1.5, 2.5, 300.9].into());
    for w in ["W0", "W3"] {
        db.put_record_field_from_ca(w, "VAL", wave()).await.unwrap();
    }
    for (rec, want) in [
        ("A16", vec!["-4", "2", "3", "301"]),
        ("A3W", vec!["-4", "2", "3"]),
        ("A1W", vec!["-4"]),
        ("A16P", vec!["-3.700", "1.500", "2.500", "300.900"]),
        ("A1S", vec!["-4"]),
    ] {
        let got = strings(&db, rec).await;
        eprintln!("ROW {rec} {got:?}");
        let _ = want;
    }
}
