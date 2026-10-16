//! stringout VAL/SEVR/UDF pinned to softIoc R7.0.10.1: after init, after
//! a process, and after a VAL put mid-run (the put processes the Passive
//! record, which writes VAL once to OUT).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "SINK") {}
record(stringout, "SINK2") {}
record(stringout, "SO") { field(VAL, "train") field(OUT, "SINK") }
record(stringout, "SB") { field(OUT, "SINK2") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b).into_owned(),
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv}: {other:?}"),
    }
}

/// (VAL, UDF, SEVR)
fn state(db: &PvDatabase, rec: &str) -> (String, f64, f64) {
    (
        text(db, &format!("{rec}.VAL")),
        num(db, &format!("{rec}.UDF")),
        num(db, &format!("{rec}.SEVR")),
    )
}

#[epics_macros_rs::epics_test]
async fn stringout_matches_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(state(&db, "SO"), ("train".into(), 0.0, 0.0), "SO init");
    assert_eq!(state(&db, "SB"), (String::new(), 1.0, 3.0), "SB init");

    process(&db, "SO").await;
    process(&db, "SB").await;
    assert_eq!(state(&db, "SO"), ("train".into(), 0.0, 0.0), "SO proc");
    assert_eq!(state(&db, "SB"), (String::new(), 1.0, 3.0), "SB proc");
    assert_eq!(text(&db, "SINK.VAL"), "train");

    db.put_record_field_from_ca("SB", "VAL", EpicsValue::String("hey".into()))
        .await
        .unwrap();
    assert_eq!(state(&db, "SB"), ("hey".into(), 0.0, 0.0), "SB put");
    assert_eq!(text(&db, "SINK2.VAL"), "hey");
}
