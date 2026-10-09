//! Event-record names pinned to softIoc R7.0.10.1: a soft INP read lands the
//! source's DBR_STRING ("5" from a PREC=0 calc) and posts it, a numeric name
//! wakes both `EVNT "5"` and `EVNT "5.0"`, a string name wakes its exact
//! match, and a VAL put mid-run is replaced by the next INP read.
//!
//! ```text
//! E  init ""/UDF 1   proc -> "5" UDF 0 NO_ALARM   C5 1  C50 1
//! F  VAL "foo"       proc -> CF 1
//! G  INP -> "bar"    proc twice -> CB 2
//! E  put VAL "foo", proc -> VAL "5", C5 2, CF 1
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "S") { field(CALC, "5") field(PREC, "0") }
record(event, "E") { field(INP, "S") }
record(event, "F") { field(VAL, "foo") }
record(stringin, "T") { field(VAL, "bar") }
record(event, "G") { field(INP, "T") }
record(calc, "C5") { field(SCAN, "Event") field(EVNT, "5") field(CALC, "VAL+1") }
record(calc, "C50") { field(SCAN, "Event") field(EVNT, "5.0") field(CALC, "VAL+1") }
record(calc, "CF") { field(SCAN, "Event") field(EVNT, "foo") field(CALC, "VAL+1") }
record(calc, "CB") { field(SCAN, "Event") field(EVNT, "bar") field(CALC, "VAL+1") }
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
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv}: {other:?}"),
    }
}

/// The post is spawned, so poll until the counters reach `want`, then hold a
/// settle window so a spurious extra post would land before the assert.
async fn counters(db: &PvDatabase, want: [f64; 4]) {
    let read = || ["C5", "C50", "CF", "CB"].map(|r| num(db, r));
    for _ in 0..400 {
        if read() == want {
            break;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(read(), want, "C5/C50/CF/CB");
}

#[epics_macros_rs::epics_test]
async fn event_names_post_like_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!((text(&db, "E"), num(&db, "E.UDF")), (String::new(), 1.0));
    process(&db, "S").await;
    process(&db, "E").await;
    assert_eq!(text(&db, "E"), "5");
    assert_eq!((num(&db, "E.UDF"), num(&db, "E.SEVR")), (0.0, 0.0));
    counters(&db, [1.0, 1.0, 0.0, 0.0]).await;

    process(&db, "F").await;
    counters(&db, [1.0, 1.0, 1.0, 0.0]).await;

    process(&db, "G").await;
    process(&db, "G").await;
    assert_eq!(text(&db, "G"), "bar");
    counters(&db, [1.0, 1.0, 1.0, 2.0]).await;

    db.put_record_field_from_ca("E", "VAL", EpicsValue::String("foo".into()))
        .await
        .unwrap();
    process(&db, "E").await;
    assert_eq!(text(&db, "E"), "5");
    counters(&db, [2.0, 2.0, 1.0, 2.0]).await;
}
