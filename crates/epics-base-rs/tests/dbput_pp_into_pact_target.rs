//! Spec §5 test 8: a PP OUT put into a target that is PACT (calcout in its
//! ODLY). `dbDbPutValue` stores and posts the field, and `processTarget`
//! (`dbDbLink.c:441-460`) does not process the busy target: it sets RPRO
//! only when the source's `putf` is set, so a CA-put source reprocesses the
//! target after the delay and a scanned source does not. Pinned to
//! oracleStdIoc (C ground truth §5.8, its DB with ODLY 0.3).

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calcout,"COA"){field(CALC,"A") field(ODLY,"0.3")}
record(ao,"SRC"){field(OUT,"COA.A PP")}
record(calcout,"COB"){field(CALC,"A") field(ODLY,"0.3")}
record(ao,"SRC2"){field(OUT,"COB.A PP")}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// VAL and A posts, as their values.
struct Watch {
    val: EventReader,
    a: EventReader,
}

fn watch(db: &PvDatabase, rec: &str) -> Watch {
    let r = db.get_record(rec).unwrap();
    let mut g = r.write();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let mut val = g
        .add_subscriber("VAL", 1, DbFieldType::Double, mask)
        .unwrap();
    let mut a = g.add_subscriber("A", 2, DbFieldType::Double, mask).unwrap();
    drop(g);
    while val.try_recv().is_ok() {}
    while a.try_recv().is_ok() {}
    Watch { val, a }
}

fn drain(rx: &mut EventReader) -> Vec<f64> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.value().as_value().and_then(EpicsValue::to_f64).unwrap());
    }
    out
}

async fn wait_idle(db: &PvDatabase, rec: &str) {
    for _ in 0..100 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        if num(db, &format!("{rec}.PACT")) == 0.0 {
            return;
        }
    }
    panic!("{rec} still PACT");
}

#[epics_macros_rs::epics_test]
async fn a_pp_put_into_a_pact_target_stores_and_sets_rpro_only_with_putf() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    // A CA-put source: putf set, so the busy target owes a reprocess.
    let mut coa = watch(&db, "COA");
    db.put_record_field_from_ca("COA", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "COA.PACT"), 1.0, "COA waits out its ODLY");
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();
    assert_eq!(num(&db, "COA.A"), 7.0);
    assert_eq!(drain(&mut coa.a), vec![7.0], "A posted by the put");
    assert_eq!(drain(&mut coa.val), Vec::<f64>::new(), "not processed");
    assert_eq!(num(&db, "COA.RPRO"), 1.0);
    wait_idle(&db, "COA").await;
    wait_idle(&db, "COA").await;
    assert_eq!(num(&db, "COA.RPRO"), 0.0);
    assert_eq!(num(&db, "COA"), 7.0, "the reprocess computed from A");

    // A scanned source: no putf, so no RPRO and no reprocess.
    let mut cob = watch(&db, "COB");
    db.put_record_field_from_ca("COB", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "COB.PACT"), 1.0);
    db.put_pv("SRC2.VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    db.process_record("SRC2").await.unwrap();
    assert_eq!(num(&db, "COB.A"), 5.0);
    assert_eq!(drain(&mut cob.a).last(), Some(&5.0), "A posted by the put");
    assert_eq!(num(&db, "COB.RPRO"), 0.0);
    wait_idle(&db, "COB").await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(500)).await;
    assert_eq!(num(&db, "COB.PACT"), 0.0);
    assert_eq!(num(&db, "COB"), 0.0, "COB computed from A before the put");
}
