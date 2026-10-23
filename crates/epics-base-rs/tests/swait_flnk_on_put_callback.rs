//! swait puts OUT with `recDynLinkPutCallback` and runs `recGblFwdLink` from
//! its `notifyCallback` (swaitRecord.c:378-386, 762-790): the cycle that puts
//! fires no forward link and completes no put-notify, the put's completion
//! does both, and a cycle whose put finds one in progress puts without a
//! callback and fires none.
//!
//! oracleSwaitIoc (calc R3-7-5), FN:T's FLNK a calcout with ODLY 1, so FN:W's
//! put completes 1 s after it processes; FN:N has no OUTN, FN:WD and FN:ND
//! are FN:W and FN:N with ODLY 0.5 and FLNK FN:CNT2:
//!
//! ```text
//! fn 0.3s CNT 0  DLY.PACT 1
//! fn 1.8s CNT 1
//! fn no-out CNT 2
//! fn caput-c secs 1 CNT 3
//! fn twice CNT+ 1                       FN:W processed twice at once
//! fn odly-out 0.8s CNT2 0  WD.PACT 0
//! fn odly-out 2.0s CNT2 1
//! fn odly-none 0.2s CNT2 1
//! fn odly-none 0.8s CNT2 2
//! fn odly-out caput-c tenths 15
//! ```
//!
//! Here FN:DLY writes FN:DONE at its delay end, ahead of the completion of
//! the put that processed it, so FN:DONE read when FN:CNT moves tells which
//! of the two fired the forward link.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: f64 = 0.0;

const DB: &str = r#"
record(ao, "FN:DONE") { }
record(ao, "FN:T") { field(FLNK, "FN:DLY") }
record(calcout, "FN:DLY") { field(CALC, "1") field(ODLY, "0.5") field(OUT, "FN:DONE PP") }
record(calc, "FN:CNT") { field(CALC, "A+1") field(INPA, "FN:CNT") }
record(swait, "FN:W") { field(CALC, "2") field(OUTN, "FN:T") field(OOPT, "Every Time") field(FLNK, "FN:CNT") }
record(swait, "FN:N") { field(CALC, "2") field(OOPT, "Every Time") field(FLNK, "FN:CNT") }
record(swait, "FN:WD") { field(CALC, "2") field(OUTN, "FN:T") field(ODLY, "0.2") field(OOPT, "Every Time") field(FLNK, "FN:CNT") }
record(swait, "FN:ND") { field(CALC, "2") field(ODLY, "0.2") field(OOPT, "Every Time") field(FLNK, "FN:CNT") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, rec: &str, field: &str, v: f64) -> ProcessCompletion {
    db.put_record_field_from_ca(rec, field, EpicsValue::Double(v))
        .await
        .unwrap()
}

async fn settle(db: &PvDatabase, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while !done(db) && std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// FN:DONE when `rec`'s put-notify on PROC completes.
async fn done_at_completion(db: &PvDatabase, rec: &str) -> f64 {
    if let Some(rx) = put(db, rec, "PROC", 1.0).await.into_handle() {
        let _ = rx.await;
    }
    num(db, "FN:DONE")
}

#[epics_macros_rs::epics_test]
async fn the_put_callback_fires_the_forward_link() {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await);
    settle(&db, |db| {
        num(db, "FN:W.OUTV") == PV_OK && num(db, "FN:WD.OUTV") == PV_OK
    })
    .await;

    put(&db, "FN:DONE", "VAL", 0.0).await;
    put(&db, "FN:W", "PROC", 1.0).await;
    let cnt_at_put = num(&db, "FN:CNT");
    settle(&db, |db| num(db, "FN:CNT") == 1.0).await;
    let done_at_flnk = num(&db, "FN:DONE");
    put(&db, "FN:N", "PROC", 1.0).await;
    let no_out = num(&db, "FN:CNT");
    put(&db, "FN:DONE", "VAL", 0.0).await;
    let notify_done = done_at_completion(&db, "FN:W").await;
    assert_eq!(
        (
            cnt_at_put,
            done_at_flnk,
            no_out,
            notify_done,
            num(&db, "FN:CNT")
        ),
        (0.0, 1.0, 2.0, 1.0, 3.0),
        "CNT after FN:W's cycle, DONE when CNT reached 1, CNT after FN:N's cycle; \
         DONE at FN:W's put-notify completion and CNT after it"
    );

    put(&db, "FN:DONE", "VAL", 0.0).await;
    put(&db, "FN:WD", "PROC", 1.0).await;
    settle(&db, |db| num(db, "FN:CNT") == 4.0).await;
    let odly_done = num(&db, "FN:DONE");
    put(&db, "FN:DONE", "VAL", 0.0).await;
    let odly_notify_done = done_at_completion(&db, "FN:WD").await;
    let odly_notify_cnt = num(&db, "FN:CNT");
    put(&db, "FN:ND", "PROC", 1.0).await;
    let nd_now = num(&db, "FN:CNT");
    settle(&db, |db| {
        num(db, "FN:ND.PACT") == 0.0 && num(db, "FN:CNT") == 6.0
    })
    .await;
    assert_eq!(
        (
            odly_done,
            odly_notify_done,
            odly_notify_cnt,
            nd_now,
            num(&db, "FN:CNT")
        ),
        (1.0, 1.0, 5.0, 5.0, 6.0),
        "DONE when FN:WD's FLNK fired, DONE at its put-notify completion and CNT \
         after it; CNT as FN:ND enters its delay and after it"
    );

    // Last: the plain put lands on FN:DLY in its delay and leaves it an RPRO
    // (dbDbLink.c:475-487).
    put(&db, "FN:DONE", "VAL", 0.0).await;
    put(&db, "FN:W", "PROC", 1.0).await;
    put(&db, "FN:W", "PROC", 1.0).await;
    let twice_now = num(&db, "FN:CNT");
    settle(&db, |db| num(db, "FN:CNT") == 7.0).await;
    let twice_done = num(&db, "FN:DONE");
    put(&db, "FN:N", "PROC", 1.0).await;
    assert_eq!(
        (twice_now, twice_done, num(&db, "FN:CNT")),
        (6.0, 1.0, 8.0),
        "FN:W processed twice: CNT after both cycles, DONE at CNT 7, CNT after FN:N"
    );
}
