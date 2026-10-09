//! lso VAL/LEN/SEVR/UDF pinned to softIoc R7.0.10.1: after init, after
//! a process, and after a VAL put mid-run (the put processes the Passive
//! record, which writes VAL once to OUT).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "SINK") {}
record(stringout, "SINK2") {}
record(lso, "LO") { field(DOL, {const:"train"}) field(OUT, "SINK") field(SIZV, "80") }
record(lso, "LB") { field(OUT, "SINK2") field(SIZV, "80") }
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
async fn lso_matches_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(state(&db, "LO"), ("train".into(), 0.0, 3.0), "LO init");
    assert_eq!(num(&db, "LO.LEN"), 6.0);
    assert_eq!(state(&db, "LB"), (String::new(), 1.0, 3.0), "LB init");
    assert_eq!(num(&db, "LB.LEN"), 0.0);

    process(&db, "LO").await;
    process(&db, "LB").await;
    assert_eq!(state(&db, "LO"), ("train".into(), 0.0, 0.0), "LO proc");
    assert_eq!(num(&db, "LO.LEN"), 6.0);
    assert_eq!(state(&db, "LB"), (String::new(), 1.0, 3.0), "LB proc");
    assert_eq!(text(&db, "SINK.VAL"), "train");

    db.put_record_field_from_ca("LB", "VAL", EpicsValue::String("hello".into()))
        .await
        .unwrap();
    assert_eq!(state(&db, "LB"), ("hello".into(), 0.0, 0.0), "LB put");
    assert_eq!(num(&db, "LB.LEN"), 6.0);
    assert_eq!(text(&db, "SINK2.VAL"), "hello");

    db.put_record_field_from_ca("LO", "VAL", EpicsValue::String("abc".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "LO.LEN"), 4.0);
    assert_eq!(text(&db, "SINK.VAL"), "abc");
}
