//! Spec §5 test 18: a subscribed calc written through OUT links. A PP put
//! into `A` posts A, then the process posts VAL and A with the new alarm;
//! NPP puts into HOPR and EGU post nothing on a value/alarm monitor and do
//! not process, and the next metadata read shows them. Pinned to
//! oracleStdIoc (C ground truth §5.18, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calc,"CT"){field(CALC,"A") field(PREC,"2") field(EGU,"mm") field(HOPR,"10") field(LOPR,"-10") field(HIGH,"5") field(HSV,"MINOR")}
record(ao,"SA"){field(OUT,"CT.A PP")}
record(ao,"SE"){field(OUT,"CT.HOPR NPP")}
record(stringout,"SU"){field(OUT,"CT.EGU NPP")}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} = {other:?}"),
    }
}

/// `(value, stat, sevr)` per post.
fn drain(rx: &mut EventReader) -> Vec<(f64, u16, u16)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        let v = ev.value().as_value().and_then(EpicsValue::to_f64).unwrap();
        out.push((v, ev.stat(), ev.sevr()));
    }
    out
}

#[epics_macros_rs::epics_test]
async fn a_calc_target_posts_the_value_and_serves_the_written_metadata() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let (mut val, mut a) = {
        let r = db.get_record("CT").unwrap();
        let mut g = r.write();
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        let mut v = g
            .add_subscriber("VAL", 1, DbFieldType::Double, mask)
            .unwrap();
        let mut a = g.add_subscriber("A", 2, DbFieldType::Double, mask).unwrap();
        while v.try_recv().is_ok() {}
        while a.try_recv().is_ok() {}
        (v, a)
    };

    db.put_record_field_from_ca("SA", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();
    assert_eq!(
        drain(&mut a),
        vec![(7.0, 17, 3), (7.0, 4, 1)],
        "A from the put (pre-process alarm), then from monitor()"
    );
    assert_eq!(drain(&mut val), vec![(7.0, 4, 1)]);

    db.put_record_field_from_ca("SE", "VAL", EpicsValue::Double(50.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("SU", "VAL", EpicsValue::String("cm".into()))
        .await
        .unwrap();
    assert_eq!(drain(&mut val), vec![], "NPP metadata puts post no value");
    assert_eq!(drain(&mut a), vec![]);
    assert_eq!(num(&db, "CT.HOPR"), 50.0);
    assert_eq!(text(&db, "CT.EGU"), "cm");
    assert_eq!(num(&db, "CT"), 7.0, "not processed");

    db.put_record_field_from_ca("SA", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(drain(&mut a), vec![(1.0, 4, 1), (1.0, 0, 0)]);
    assert_eq!(drain(&mut val), vec![(1.0, 0, 0)]);
    assert_eq!(
        (num(&db, "CT.HOPR"), text(&db, "CT.EGU")),
        (50.0, "cm".into())
    );
}
