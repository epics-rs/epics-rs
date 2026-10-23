//! Spec §5 test 16: an OUT link into the record itself (numeric, string,
//! and into a non-value field) and a pair linked to each other. The write
//! runs under the source's own held set, so a self target is borrowed with
//! no second acquire: no panic, one post, and no reprocess — C's
//! `processTarget` claims nothing when `psrc == pdst` (`dbDbLink.c:441`).
//! Pinned to oracleStdIoc (C ground truth §5.16, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao,"AS"){field(OUT,"AS.VAL NPP")}
record(ao,"AS2"){field(OUT,"AS2.VAL PP")}
record(stringout,"SS"){field(OUT,"SS.VAL PP")}
record(ao,"A"){field(OUT,"B.VAL PP")}
record(ao,"B"){field(OUT,"A.VAL PP")}
record(ao,"AE"){field(OUT,"AE.EGU NPP")}
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

fn sub(db: &PvDatabase, rec: &str, field: &str, ty: DbFieldType) -> EventReader {
    let r = db.get_record(rec).unwrap();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let mut rx = r.write().add_subscriber(field, 1, ty, mask).unwrap();
    while rx.try_recv().is_ok() {}
    rx
}

fn count(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

#[epics_macros_rs::epics_test]
async fn a_self_or_back_link_writes_once_and_does_not_reprocess() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut as_ = sub(&db, "AS", "VAL", DbFieldType::Double);
    let mut as2 = sub(&db, "AS2", "VAL", DbFieldType::Double);
    let mut ss = sub(&db, "SS", "VAL", DbFieldType::String);
    let mut a = sub(&db, "A", "VAL", DbFieldType::Double);
    let mut b = sub(&db, "B", "VAL", DbFieldType::Double);
    let mut egu = sub(&db, "AE", "EGU", DbFieldType::String);

    db.put_record_field_from_ca("AS", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    assert_eq!((num(&db, "AS"), count(&mut as_)), (5.0, 1));

    db.put_record_field_from_ca("AS2", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    assert_eq!((num(&db, "AS2"), count(&mut as2)), (5.0, 1));
    assert_eq!(num(&db, "AS2.RPRO"), 0.0, "no claim on itself");

    db.put_record_field_from_ca("SS", "VAL", EpicsValue::String("hello".into()))
        .await
        .unwrap();
    assert_eq!((text(&db, "SS"), count(&mut ss)), ("hello".into(), 1));

    db.put_record_field_from_ca("A", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!((num(&db, "A"), num(&db, "B")), (3.0, 3.0));
    assert_eq!((count(&mut a), count(&mut b)), (1, 1), "one pass each way");
    db.put_record_field_from_ca("B", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    assert_eq!((num(&db, "A"), num(&db, "B")), (4.0, 4.0));
    assert_eq!((count(&mut a), count(&mut b)), (1, 1));
    for r in ["A", "B"] {
        assert_eq!(num(&db, &format!("{r}.SEVR")), 0.0, "{r}");
    }

    // PREC 0: the target field's precision rounds 2.5 to "3".
    db.put_record_field_from_ca("AE", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();
    assert_eq!((text(&db, "AE.EGU"), count(&mut egu)), ("3".into(), 1));
    assert_eq!(num(&db, "AE.SEVR"), 0.0);
}
