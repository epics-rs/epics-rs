//! swait's INAN..INLN are `recDynLink` inputs (`swaitRecord.c:358`): INxV is
//! the channel's connection state, so a name with a link modifier names no
//! PV, a put posts PV BAD and then PV OK when the search connects, and a
//! blank name is cleared to empty (`special`, `:513-519`).
//!
//! oracleSwaitIoc (calc R3-7-5), PV:W `INAN "SQ:T PP"`, `INBN NOSUCH:PV`,
//! `INCN SQ:SRC.VAL`, `OUTN "SQ:T PP"`; PV:C `INCN SQ:SRC`, `CALC C`:
//!
//! ```text
//! pv pre PV BAD PV BAD PV OK PV BAD  C PV OK      W INAV INBV INCV OUTV, C INCV
//! pv post W 0 INVALID READ  C 7 NO_ALARM          after one process each
//! pv clear No PV                                  PV:C.INCN ""
//! pv blank No PV  []                              PV:C.INCN "  ", INCN after
//! pv set-now PV BAD                               PV:C.INCN SQ:T2, at once
//! pv set-later PV OK                              0.3 s later
//! ```
//!
//! The read at once is a race in C too: `special` posts PV BAD before
//! `recDynLinkAddInput` "because SearchCallback could happen immediatley"
//! (`:535-536`), on recDynInp above the CA server's priority. What C fixes is
//! the order of the posts, PV BAD then PV OK, and that is what is checked.
//!
//! ```text
//! pv bad PV BAD                                   PV:C.INCN NOSUCH:PV
//! ```

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SQ:T") { }
record(ao, "SQ:T2") { }
record(ao, "SQ:SRC") { field(VAL, "7") field(PINI, "YES") }
record(swait, "PV:W") { field(INAN, "SQ:T PP") field(INBN, "NOSUCH:PV") field(INCN, "SQ:SRC.VAL") field(OUTN, "SQ:T PP") field(CALC, "1") }
record(swait, "PV:C") { field(INCN, "SQ:SRC") field(CALC, "C") }
"#;

const PV_OK: f64 = 0.0;
const PV_NC: f64 = 1.0;
const NO_PV: f64 = 2.0;
const READ_ALARM: f64 = 1.0;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} is {other:?}"),
    }
}

async fn put(db: &PvDatabase, rec: &str, field: &str, v: &str) {
    db.put_record_field_from_ca(rec, field, EpicsValue::String(v.into()))
        .await
        .unwrap();
}

/// Process `rec` until `done`: an input's value lands on the link's first
/// monitor, after its status reads PV OK.
async fn process_until(db: &PvDatabase, rec: &str, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        put(db, rec, "PROC", "1").await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        if done(db) {
            return;
        }
    }
}

async fn settle(db: &PvDatabase, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if done(db) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

fn wait(ms: u64) -> impl std::future::Future<Output = ()> {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(ms))
}

#[epics_macros_rs::epics_test]
async fn an_input_name_is_a_channel_search() {
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
        num(db, "PV:W.INCV") == PV_OK && num(db, "PV:C.INCV") == PV_OK
    })
    .await;
    wait(300).await;
    let pre = [
        num(&db, "PV:W.INAV"),
        num(&db, "PV:W.INBV"),
        num(&db, "PV:W.INCV"),
        num(&db, "PV:W.OUTV"),
        num(&db, "PV:C.INCV"),
    ];
    put(&db, "PV:W", "PROC", "1").await;
    process_until(&db, "PV:C", |db| num(db, "PV:C.VAL") == 7.0).await;
    let post = [
        num(&db, "PV:W.VAL"),
        num(&db, "PV:W.SEVR"),
        num(&db, "PV:W.STAT"),
        num(&db, "PV:C.VAL"),
        num(&db, "PV:C.SEVR"),
    ];
    assert_eq!(
        (pre, post),
        (
            [PV_NC, PV_NC, PV_OK, PV_NC, PV_OK],
            [0.0, 3.0, READ_ALARM, 7.0, 0.0]
        ),
        "W INAV INBV INCV OUTV, C INCV; then W VAL SEVR STAT, C VAL SEVR"
    );

    put(&db, "PV:C", "INCN", "").await;
    wait(200).await;
    let clear = num(&db, "PV:C.INCV");
    put(&db, "PV:C", "INCN", "  ").await;
    wait(200).await;
    let blank = (num(&db, "PV:C.INCV"), text(&db, "PV:C.INCN"));
    let mut incv = DbSubscription::subscribe(&db, "PV:C.INCV").await.unwrap();
    put(&db, "PV:C", "INCN", "SQ:T2").await;
    let mut set = Vec::new();
    for _ in 0..2 {
        set.push(
            epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, incv.recv_f64())
                .await
                .ok()
                .flatten(),
        );
    }
    put(&db, "PV:C", "INCN", "NOSUCH:PV").await;
    wait(300).await;
    let bad = num(&db, "PV:C.INCV");
    assert_eq!(
        (clear, blank, set, bad),
        (
            NO_PV,
            (NO_PV, String::new()),
            vec![Some(PV_NC), Some(PV_OK)],
            PV_NC
        ),
        "PV:C.INCV after INCN \"\", \"  \" (and INCN), its posts on SQ:T2, after NOSUCH:PV"
    );
}
