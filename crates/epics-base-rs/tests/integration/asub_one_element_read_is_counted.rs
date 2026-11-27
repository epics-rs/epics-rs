//! aSub `fetch_values` reads every INPx with `&nRequest = NOx`
//! (`aSubRecord.c:283-284`), one element included, so an empty source is a
//! read of no elements, `NEx = 0` with no alarm, not `S_db_onlyOne`.
//!
//! softIoc (EPICS 7.0.10), NOA=1 channels, sources filled then emptied:
//!
//! ```text
//! WE [5,6] WES ["s1","s2"]   AS1 A 5 NEA 1   AS1S A "s1" NEA 1   NO_ALARM
//! WE [] WES []               AS1 NEA 0       AS1S NEA 0          NO_ALARM
//! ```

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "WES") { field(FTVL, "STRING") field(NELM, "4") }
record(aSub, "AS1") { field(FTA, "DOUBLE") field(NOA, "1") field(INPA, "WE NPP") }
record(aSub, "AS1S") { field(FTA, "STRING") field(NOA, "1") field(INPA, "WES NPP") }
"#;

/// Fill the sources, process both readers, and return `(A, NEA, STAT, SEVR)`
/// of each.
async fn phase(db: &PvDatabase, we: Vec<f64>, wes: &[&str]) -> Vec<(EpicsValue, f64, u16, u16)> {
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
    for rec in ["AS1", "AS1S"] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
        let r = db.get_record(rec).unwrap();
        let inst = r.read();
        got.push((
            inst.record.get_field("A").unwrap(),
            inst.record.get_field("NEA").unwrap().to_f64().unwrap(),
            inst.common.stat,
            inst.common.sevr as u16,
        ));
    }
    got
}

#[epics_macros_rs::epics_test]
async fn an_empty_source_is_a_read_of_no_elements() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let full = phase(&db, vec![5.0, 6.0], &["s1", "s2"]).await;
    let empty: Vec<_> = phase(&db, Vec::new(), &[])
        .await
        .into_iter()
        .map(|(_, nea, stat, sevr)| (nea, stat, sevr))
        .collect();
    assert_eq!(
        (full, empty),
        (
            vec![
                (EpicsValue::Double(5.0), 1.0, 0, 0),
                (EpicsValue::String("s1".into()), 1.0, 0, 0),
            ],
            vec![(0.0, 0, 0); 2],
        ),
        "(A, NEA, STAT, SEVR) of AS1 AS1S filled, then (NEA, STAT, SEVR) emptied"
    );
}
