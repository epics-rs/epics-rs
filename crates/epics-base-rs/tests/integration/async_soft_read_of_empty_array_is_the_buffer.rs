//! devXxSoftCallback's `getCallback` reads INP with `no_elements = 1` into
//! the devPvt buffer and `read_xxx` copies the buffer into VAL
//! (`devAiSoftCallback.c:56-65`). A source of no elements writes nothing, so
//! VAL takes the last reading — `add_record`'s calloc'd zero before the first
//! — with no alarm.
//!
//! softIoc (EPICS 7.0.10), readers with VAL "9"/"x" at load, `WE` a DOUBLE
//! and `WES` a STRING waveform:
//!
//! ```text
//! WE [] WES []                 AIA LIA I64A MIDA 0  SIA ""   NO_ALARM UDF 0
//! WE [5,6] WES ["s1","s2"]     5                    "s1"
//! WE [] WES []                 5                    "s1"     NO_ALARM
//! ```

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "WES") { field(FTVL, "STRING") field(NELM, "4") }
record(ai, "AIA") { field(DTYP, "Async Soft Channel") field(INP, "WE") field(VAL, "9") }
record(longin, "LIA") { field(DTYP, "Async Soft Channel") field(INP, "WE") field(VAL, "9") }
record(int64in, "I64A") { field(DTYP, "Async Soft Channel") field(INP, "WE") field(VAL, "9") }
record(mbbiDirect, "MIDA") { field(DTYP, "Async Soft Channel") field(INP, "WE") field(VAL, "9") }
record(stringin, "SIA") { field(DTYP, "Async Soft Channel") field(INP, "WES") field(VAL, "x") }
"#;

const READERS: [&str; 5] = ["AIA", "LIA", "I64A", "MIDA", "SIA"];

/// Process every reader, wait for its notify completion, and return
/// `(VAL as text, STAT, UDF)` of each.
async fn phase(db: &PvDatabase, we: Vec<f64>, wes: &[&str]) -> Vec<(String, u16, u8)> {
    db.put_pv("WE", EpicsValue::DoubleArray(we.into()))
        .await
        .unwrap();
    db.put_pv(
        "WES",
        EpicsValue::StringArray(wes.iter().map(|s| PvString::from(*s)).collect()),
    )
    .await
    .unwrap();
    let mut got = Vec::new();
    for reader in READERS {
        db.process_record(reader).await.unwrap();
        let rec = db.get_record(reader).unwrap();
        let until = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while rec.read().is_processing() && std::time::Instant::now() < until {
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
        }
        let inst = rec.read();
        let val = inst.record.get_field("VAL").unwrap();
        let val = match val {
            EpicsValue::String(s) => s.to_string(),
            other => other.to_f64().unwrap().to_string(),
        };
        got.push((val, inst.common.stat, inst.common.udf));
    }
    got
}

#[epics_macros_rs::epics_test]
async fn an_empty_source_delivers_the_devpvt_buffer() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let at = |vals: [&str; 5]| -> Vec<(String, u16, u8)> {
        vals.iter().map(|v| (v.to_string(), 0, 0)).collect()
    };
    let never = phase(&db, Vec::new(), &[]).await;
    let full = phase(&db, vec![5.0, 6.0], &["s1", "s2"]).await;
    let emptied = phase(&db, Vec::new(), &[]).await;
    assert_eq!(
        (never, full, emptied),
        (
            at(["0", "0", "0", "0", ""]),
            at(["5", "5", "5", "5", "s1"]),
            at(["5", "5", "5", "5", "s1"]),
        ),
        "(VAL, STAT, UDF) of AIA LIA I64A MIDA SIA per phase"
    );
}
