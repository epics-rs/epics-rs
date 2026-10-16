//! A runtime DOL put moves the aao's closed-loop fetch gate both ways with no
//! link text parsed on the cycle: constant -> link -> constant -> link.
//! softIoc R7.0.10.1, SRC = waveform {7,8,9}, AAO closed_loop DOL "[1,2,3]",
//! each step `caput AAO.DOL$ <text>` then `caput AAO.PROC 1`:
//!
//! ```text
//! init                  [1,2,3]  NORD=3
//! proc (constant)       [1,2,3]  NORD=3   not re-applied
//! DOL=SRC, proc         [7,8,9]  NORD=3
//! DOL=[5], SRC=[3], proc [7,8,9] NORD=3   constant: no fetch, put loads nothing
//! DOL=SRC, proc         [3]      NORD=1
//! ```

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "SRC") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, [7,8,9]) }
record(aao, "AAO") { field(NELM, "4") field(FTVL, "DOUBLE") field(OMSL, "closed_loop") field(DOL, "[1,2,3]") }
"#;

async fn proc_then_val(db: &PvDatabase) -> (Vec<f64>, f64) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AAO", &mut visited)
        .await
        .unwrap();
    let nord = db.get_pv("AAO.NORD").unwrap().to_f64().unwrap();
    let val = match db.get_pv("AAO.VAL").unwrap() {
        EpicsValue::DoubleArray(v) => v.iter().copied().take(nord as usize).collect(),
        other => panic!("AAO.VAL is {other:?}"),
    };
    (val, nord)
}

async fn set_dol(db: &PvDatabase, text: &str) {
    db.put_record_field_from_ca("AAO", "DOL", EpicsValue::String(text.into()))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_dol_put_moves_the_closed_loop_fetch_both_ways() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(proc_then_val(&db).await, (vec![1.0, 2.0, 3.0], 3.0));

    set_dol(&db, "SRC").await;
    assert_eq!(proc_then_val(&db).await, (vec![7.0, 8.0, 9.0], 3.0));

    set_dol(&db, "[5]").await;
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(proc_then_val(&db).await, (vec![7.0, 8.0, 9.0], 3.0));

    set_dol(&db, "SRC").await;
    assert_eq!(proc_then_val(&db).await, (vec![3.0], 1.0));
}
