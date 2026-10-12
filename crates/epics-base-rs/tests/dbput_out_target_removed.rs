//! Spec §5 test 19: the OUT target goes away between cycles. The name map's
//! revision moves, so the next write re-resolves the link instead of using
//! the address it lent before: the target is no local record, the put
//! fails, and the source takes LINK/INVALID "field OUT" (`dbLink.c:443-446`).
//! Re-pointing OUT restores normal writes. C cannot delete a record; its
//! closest analogue, re-pointing OUT to a missing name, is ground truth
//! §5.19 and gives the same source alarm.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao,"TG"){}
record(ao,"TG2"){}
record(ao,"SR"){field(OUT,"TG.VAL PP")}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn alarm(db: &PvDatabase, rec: &str) -> (f64, f64, String) {
    let amsg = match db.get_pv(&format!("{rec}.AMSG")).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}.AMSG = {other:?}"),
    };
    (
        num(db, &format!("{rec}.STAT")),
        num(db, &format!("{rec}.SEVR")),
        amsg,
    )
}

async fn put(db: &PvDatabase, rec: &str, field: &str, v: EpicsValue) {
    let _ = db.put_record_field_from_ca(rec, field, v).await;
}

#[epics_macros_rs::epics_test]
async fn a_removed_target_is_re_resolved_and_alarms_the_source() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    put(&db, "SR", "VAL", EpicsValue::Double(1.0)).await;
    assert_eq!(num(&db, "TG"), 1.0);
    assert_eq!(alarm(&db, "SR"), (0.0, 0.0, String::new()));

    let tg = db.get_record("TG").unwrap();
    assert!(db.remove_record("TG").await);
    put(&db, "SR", "VAL", EpicsValue::Double(2.0)).await;
    assert_eq!(alarm(&db, "SR"), (14.0, 3.0, "field OUT".into()));
    assert_eq!(
        tg.read().record.val().and_then(|v| v.to_f64()),
        Some(1.0),
        "the removed record is not written through the old address"
    );

    put(&db, "SR", "OUT", EpicsValue::String("TG2.VAL PP".into())).await;
    put(&db, "SR", "VAL", EpicsValue::Double(3.0)).await;
    assert_eq!(num(&db, "TG2"), 3.0);
    assert_eq!(alarm(&db, "SR"), (0.0, 0.0, String::new()));
}
