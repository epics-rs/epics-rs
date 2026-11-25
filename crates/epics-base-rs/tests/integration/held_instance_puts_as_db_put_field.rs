//! `HeldInstance`'s puts are C `dbPut`/`dbPutField` (dbAccess.c:1245-1290)
//! made from inside a record's hold: the status comes back at once, the
//! target's links run with the holder's record lent out, and an active
//! target takes RPRO with no LCNT count.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::error::DbResult;
use epics_base_rs::server::database::{FieldPut, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "H") { }
record(ao, "T") { field(FLNK, "N") }
record(calc, "N") { field(CALC, "VAL+1") }
record(ao, "D") { field(DISP, "1") }
record(ao, "S") { field(SCAN, "10 second") field(FLNK, "M") }
record(calc, "M") { field(CALC, "VAL+1") }
record(calcout, "CO") { field(CALC, "A") field(ODLY, "0.5") }
"#;

/// A non-link field's `dbPutField` status: an entry's hold makes it in
/// the hold.
fn done(put: FieldPut) -> DbResult {
    match put {
        FieldPut::Done(r) => r,
        FieldPut::Unheld(put) => panic!("not a link field: {put:?}"),
    }
}

fn get(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

#[epics_macros_rs::epics_test]
async fn put_field_processes_the_target_and_its_flnk() {
    let db = ioc().await;
    let h = db.async_handle();
    let t_val = h.name_to_dbaddr("T.VAL", None).unwrap();
    let r = h
        .with_held("H", |held| {
            held.db_put_field(&t_val, EpicsValue::Double(4.0))
        })
        .map(done);
    assert_eq!(r, Some(Ok(())));
    assert_eq!(get(&db, "T"), 4.0);
    assert_eq!(get(&db, "N"), 1.0, "T's FLNK ran inside the hold");

    let r = h.with_held("H", |held| held.db_put(&t_val, EpicsValue::Double(5.0)));
    assert_eq!(r, Some(Ok(())));
    assert_eq!(get(&db, "T"), 5.0);
    assert_eq!(get(&db, "N"), 1.0, "dbPut processes nothing");
}

#[epics_macros_rs::epics_test]
async fn put_field_processes_only_where_db_put_field_does() {
    let db = ioc().await;
    let h = db.async_handle();
    // DESC is not process-passive.
    let desc = h.name_to_dbaddr("T.DESC", None).unwrap();
    let r = h
        .with_held("H", |held| {
            held.db_put_field(&desc, EpicsValue::String("d".into()))
        })
        .map(done);
    assert_eq!(r, Some(Ok(())));
    assert_eq!(get(&db, "N"), 0.0);
    // A periodic record's VAL does not process it; its PROC does.
    let s_val = h.name_to_dbaddr("S.VAL", None).unwrap();
    let s_proc = h.name_to_dbaddr("S.PROC", None).unwrap();
    let before = get(&db, "M");
    h.with_held("H", |held| {
        held.db_put_field(&s_val, EpicsValue::Double(1.0))
    })
    .map(done)
    .unwrap()
    .unwrap();
    assert_eq!(get(&db, "M"), before, "S not processed");
    h.with_held("H", |held| held.db_put_field(&s_proc, EpicsValue::Long(1)))
        .map(done)
        .unwrap()
        .unwrap();
    assert_eq!(get(&db, "M"), before + 1.0, "PROC processes on any SCAN");
}

#[epics_macros_rs::epics_test]
async fn put_field_refuses_a_disp_record_and_db_put_does_not() {
    let db = ioc().await;
    let h = db.async_handle();
    let d_val = h.name_to_dbaddr("D.VAL", None).unwrap();
    let r = h
        .with_held("H", |held| {
            held.db_put_field(&d_val, EpicsValue::Double(3.0))
        })
        .map(done);
    assert!(matches!(r, Some(Err(_))), "{r:?}");
    assert_eq!(get(&db, "D"), 0.0);
    let r = h.with_held("H", |held| held.db_put(&d_val, EpicsValue::Double(3.0)));
    assert_eq!(r, Some(Ok(())));
    assert_eq!(get(&db, "D"), 3.0);
}

#[epics_macros_rs::epics_test]
async fn a_failed_put_answers_err() {
    let db = ioc().await;
    let h = db.async_handle();
    let t_val = h.name_to_dbaddr("T.VAL", None).unwrap();
    let r = h
        .with_held("H", |held| {
            held.db_put_field(&t_val, EpicsValue::String("not a number".into()))
        })
        .map(done);
    assert!(matches!(r, Some(Err(_))), "{r:?}");
    assert_eq!(get(&db, "N"), 0.0, "a failed put processes nothing");
}

#[epics_macros_rs::epics_test]
async fn an_active_target_takes_rpro_without_lcnt() {
    let db = ioc().await;
    let h = db.async_handle();
    let idle = async {
        while get(&db, "CO.PACT") != 0.0 {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
    };
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, idle)
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("CO", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert_eq!(get(&db, "CO.PACT"), 1.0, "CO waits out its ODLY");
    let a = h.name_to_dbaddr("CO.A", None).unwrap();
    h.with_held("H", |held| held.db_put_field(&a, EpicsValue::Double(2.0)))
        .map(done)
        .unwrap()
        .unwrap();
    assert_eq!(get(&db, "CO.RPRO"), 1.0);
    assert_eq!(get(&db, "CO.LCNT"), 0.0, "dbPutField calls no dbProcess");
}

#[epics_macros_rs::epics_test]
async fn the_held_record_s_own_dbcommon_field() {
    let db = ioc().await;
    let h = db.async_handle();
    let desc = h.name_to_dbaddr("H.DESC", None).unwrap();
    let r = h.with_held("H", |held| {
        let r = held.db_put(&desc, EpicsValue::String("own".into()));
        (r, held.with(|_, common| common.text.desc.to_string()))
    });
    assert_eq!(r, Some((Ok(()), "own".to_string())));
}
