//! Spec §5 test 9: an OUT put into `.PROC` processes the target whatever
//! its SCAN and whatever the link's PP flag or the value written, while a
//! PP put into another field of a non-passive target only stores
//! (`dbDbLink.c:387-390`: `.PROC` or PP-and-Passive). Pinned to
//! oracleStdIoc (C ground truth §5.9, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc,"T1"){field(SCAN,"10 second") field(CALC,"VAL+1")}
record(calc,"T2"){field(SCAN,"10 second") field(CALC,"A")}
record(calc,"T3"){field(SCAN,"10 second") field(CALC,"VAL+1")}
record(calc,"T4"){field(CALC,"VAL+1")}
record(ao,"P1"){field(OUT,"T1.PROC")}
record(ao,"P2"){field(OUT,"T2.A PP")}
record(ao,"P3"){field(OUT,"T3.PROC NPP")}
record(ao,"P4"){field(OUT,"T4.PROC NPP")}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_proc_put_processes_any_target_and_pp_only_a_passive_one() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    put(&db, "P1", 1.0).await;
    assert_eq!(num(&db, "T1"), 1.0, "a 10 s target, processed");
    put(&db, "P1", 0.0).await;
    assert_eq!(num(&db, "T1"), 2.0, "value 0 processes too");

    put(&db, "P2", 5.0).await;
    assert_eq!(num(&db, "T2.A"), 5.0, "stored");
    assert_eq!(
        num(&db, "T2"),
        0.0,
        "PP into a non-passive target: not processed"
    );
    assert_eq!(num(&db, "T2.UDF"), 1.0);

    put(&db, "P3", 1.0).await;
    assert_eq!(num(&db, "T3"), 1.0, ".PROC NPP processes");
    put(&db, "P4", 0.0).await;
    assert_eq!(
        num(&db, "T4"),
        1.0,
        ".PROC NPP on a passive target, value 0"
    );

    for p in ["P1", "P2", "P3", "P4"] {
        assert_eq!(num(&db, &format!("{p}.SEVR")), 0.0, "{p}");
    }
}
