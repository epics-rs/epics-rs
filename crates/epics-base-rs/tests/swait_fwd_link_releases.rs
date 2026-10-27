//! What swait's `recGblFwdLink` releases besides its forward link — the
//! source's PUTF and a pending RPRO (recGbl.c:295-302) — stays with the
//! record until OUT's put callback makes the call (swaitRecord.c:378-386).
//!
//! oracleSwaitIoc (calc R3-7-5) on these records:
//!
//! ```text
//! dbpf PB:T.PROC 1; dbpf PB:S.PROC 1; 2.5 s     PB:CNT 2
//! dbpf PB:T.PROC 1; dbpf PB:N.PROC 1; 2.5 s     PB:CNT 4
//! dbpf RP:S.PROC 1; 0.1 s; dbpf RP:S.PROC 1     RP:S.RPRO 1
//!   at 1.0 s  RP:CNT 0  RP:TC 0
//!   at 2.0 s  RP:CNT 1  RP:TC 1
//!   at 4.0 s  RP:CNT 2  RP:TC 2
//! ```

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: f64 = 0.0;

const PB_DB: &str = r#"
record(calc, "PB:CNT") { field(CALC, "A+1") field(INPA, "PB:CNT") }
record(calcout, "PB:T") { field(CALC, "1") field(ODLY, "0.5") field(OOPT, "Every Time") field(FLNK, "PB:CNT") }
record(ao, "PB:Z") { }
record(swait, "PB:S") { field(CALC, "1") field(OUTN, "PB:Z") field(OOPT, "Every Time") field(FLNK, "PB:T") }
record(swait, "PB:N") { field(CALC, "1") field(OOPT, "Every Time") field(FLNK, "PB:T") }
"#;

const RP_DB: &str = r#"
record(calc, "RP:CNT") { field(CALC, "A+1") field(INPA, "RP:CNT") }
record(calc, "RP:TC") { field(CALC, "A+1") field(INPA, "RP:TC") }
record(calcout, "RP:T") { field(CALC, "1") field(ODLY, "1.0") field(OOPT, "Every Time") field(FLNK, "RP:TC") }
record(swait, "RP:S") { field(CALC, "1") field(OUTN, "RP:T.PROC") field(ODLY, "0.5") field(OOPT, "Every Time") field(FLNK, "RP:CNT") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// `dbpf`: `dbPutField`, no put-notify.
async fn put(db: &PvDatabase, rec: &str, field: &str, v: f64) {
    db.put_record_field_from_ca_no_notify(rec, field, EpicsValue::Double(v))
        .await
        .unwrap();
}

/// Neither in a cycle nor owed one.
fn idle(db: &PvDatabase, rec: &str) -> bool {
    num(db, &format!("{rec}.PACT")) == 0.0 && num(db, &format!("{rec}.RPRO")) == 0.0
}

async fn settle(db: &PvDatabase, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done(db) && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

async fn sleep_until(t0: Instant, secs: f64) {
    let at = t0 + Duration::from_secs_f64(secs);
    if let Some(left) = at.checked_duration_since(Instant::now()) {
        epics_base_rs::runtime::task::sleep(left).await;
    }
}

async fn ioc(db: &str, swait: &str) -> std::sync::Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await);
    let outv = format!("{swait}.OUTV");
    settle(&db, |db| num(db, &outv) == PV_OK).await;
    db
}

#[epics_macros_rs::epics_test]
async fn the_put_callback_forwards_the_putf() {
    let db = ioc(PB_DB, "PB:S").await;
    // PB:S's FLNK lands on PB:T in its delay, from the put callback, with the
    // put's PUTF: PB:T takes an RPRO and runs again (dbDbLink.c:475-487).
    // Each PB:T cycle counts once, from its ODLY callback's FLNK.
    let counted = |n: f64| settle(&db, move |db| num(db, "PB:CNT") >= n && idle(db, "PB:T"));
    put(&db, "PB:T", "PROC", 1.0).await;
    put(&db, "PB:S", "PROC", 1.0).await;
    counted(2.0).await;
    let out = num(&db, "PB:CNT");
    put(&db, "PB:T", "PROC", 1.0).await;
    put(&db, "PB:N", "PROC", 1.0).await;
    counted(4.0).await;
    assert_eq!(
        (out, num(&db, "PB:CNT")),
        (2.0, 4.0),
        "PB:CNT after PB:S's cycle, then after PB:N's"
    );
}

#[epics_macros_rs::epics_test]
async fn the_put_callback_restarts_the_rpro() {
    let db = ioc(RP_DB, "RP:S").await;
    // The put in RP:S's delay leaves an RPRO, which waits for the callback of
    // the delay-end put on RP:T: the second cycle's put finds none in
    // progress and calls back too.
    let t0 = Instant::now();
    put(&db, "RP:S", "PROC", 1.0).await;
    sleep_until(t0, 0.1).await;
    put(&db, "RP:S", "PROC", 1.0).await;
    let rpro = num(&db, "RP:S.RPRO");
    // RP:S counts from RP:T's put callback, after RP:T's own count; the
    // rerun's next count is an RP:T delay away.
    settle(&db, |db| num(db, "RP:CNT") >= 1.0).await;
    let first = (num(&db, "RP:CNT"), num(&db, "RP:TC"));
    settle(&db, |db| {
        num(db, "RP:CNT") >= 2.0 && idle(db, "RP:S") && idle(db, "RP:T")
    })
    .await;
    assert_eq!(
        (rpro, first, (num(&db, "RP:CNT"), num(&db, "RP:TC"))),
        (1.0, (1.0, 1.0), (2.0, 2.0)),
        "RP:S.RPRO after the second put; RP:CNT and RP:TC at RP:S's first count and at the end"
    );
}
