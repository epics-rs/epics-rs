//! A bus entry's `dbPutField` into a link field ([`AsyncDbHandle::with_held`],
//! StreamEpics.cc's `%(t3.FLNK)` redirect). C's bus callback holds no lock,
//! so `dbPutFieldLink`'s `dbScanLockMany` takes the record and the new
//! target in id order (`dbAccess.c:1118-1123`); the port hands the relink
//! back to the entry, which makes it once its hold is down. The refusals C
//! makes before that lock answer the put in the hold; one under it is the
//! status of the relink, as C's caller has it from that `dbPutField`.

use std::collections::HashMap;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use epics_base_rs::error::DbResult;
use epics_base_rs::server::database::{AsyncDbHandle, FieldPut, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// AX holds T3 and BY holds T4, through their FLNKs.
const DB: &str = r#"
record(ao, "AX") { field(FLNK, "T3") }
record(ao, "T3") { }
record(ao, "BY") { field(FLNK, "T4") }
record(ao, "T4") { }
record(ao, "D") { field(DISP, "1") }
"#;

async fn ioc() -> Arc<PvDatabase> {
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

fn link(db: &PvDatabase, pv: &str) -> String {
    db.get_pv(pv).unwrap().text_g()
}

fn set(db: &PvDatabase, record: &str) -> u64 {
    db.lock_set_of(record).unwrap().id
}

fn text(t: &str) -> EpicsValue {
    EpicsValue::String(t.into())
}

/// Where an entry's `dbPutField` was answered.
#[derive(Debug, PartialEq)]
enum Answer {
    /// In its hold.
    Held(DbResult),
    /// By the relink, made after the hold.
    Unheld(DbResult),
}

fn answer(h: &AsyncDbHandle, put: Option<FieldPut>) -> Answer {
    match put.expect("the holder is there") {
        FieldPut::Done(r) => Answer::Held(r),
        FieldPut::Unheld(put) => Answer::Unheld(h.put_unheld(put)),
    }
}

/// An entry holding `holder` that waits at `barrier`, then puts `text` to
/// `pv`, on its own thread.
fn entry(
    h: &AsyncDbHandle,
    barrier: &Arc<Barrier>,
    holder: &'static str,
    pv: &'static str,
    text: &'static str,
    done: &mpsc::Sender<(&'static str, Answer)>,
) {
    let (h, barrier, done) = (h.clone(), barrier.clone(), done.clone());
    std::thread::spawn(move || {
        let addr = h.name_to_dbaddr(pv, None).unwrap();
        let r = h.with_held(holder, |held| {
            barrier.wait();
            held.db_put_field(&addr, EpicsValue::String(text.into()))
        });
        done.send((holder, answer(&h, r))).unwrap();
    });
}

fn finished(rx: &mpsc::Receiver<(&'static str, Answer)>, n: usize) -> Vec<(&'static str, Answer)> {
    let mut out: Vec<_> = (0..n)
        .map(|_| {
            rx.recv_timeout(Duration::from_secs(20))
                .expect("an entry is stuck in its link put")
        })
        .collect();
    out.sort_by_key(|(holder, _)| *holder);
    out
}

#[epics_macros_rs::epics_test]
async fn two_entries_put_each_others_link_field() {
    let db = ioc().await;
    assert_eq!(set(&db, "AX"), set(&db, "T3"));
    assert_eq!(set(&db, "BY"), set(&db, "T4"));
    assert_ne!(set(&db, "AX"), set(&db, "BY"));
    let h = db.async_handle();
    let barrier = Arc::new(Barrier::new(2));
    let (tx, rx) = mpsc::channel();
    entry(&h, &barrier, "AX", "T3.FLNK", "BY", &tx);
    entry(&h, &barrier, "BY", "T4.FLNK", "AX", &tx);
    assert_eq!(
        finished(&rx, 2),
        [
            ("AX", Answer::Unheld(Ok(()))),
            ("BY", Answer::Unheld(Ok(())))
        ]
    );
    assert_eq!(link(&db, "T3.FLNK"), "BY");
    assert_eq!(link(&db, "T4.FLNK"), "AX");
    assert_eq!(set(&db, "AX"), set(&db, "BY"));
}

/// The other thread holds the new target's set and waits for the entry's.
/// The new target's set sorts first, so C's entry waits for it holding
/// nothing; in the other order `dbScanLockMany` deadlocks in C too.
#[epics_macros_rs::epics_test]
async fn an_entry_against_a_holder_of_the_new_target() {
    let db = ioc().await;
    assert!(set(&db, "AX") < set(&db, "BY"));
    let h = db.async_handle();
    let barrier = Arc::new(Barrier::new(2));
    let (tx, rx) = mpsc::channel();
    {
        let (db, barrier) = (db.clone(), barrier.clone());
        std::thread::spawn(move || {
            let ax = db.lock_records(["AX"]);
            barrier.wait();
            drop(db.lock_records(["BY"]));
            drop(ax);
        });
    }
    entry(&h, &barrier, "BY", "T4.FLNK", "AX", &tx);
    assert_eq!(finished(&rx, 1), [("BY", Answer::Unheld(Ok(())))]);
    assert_eq!(link(&db, "T4.FLNK"), "AX");
}

#[epics_macros_rs::epics_test]
async fn the_relink_lands_when_the_hold_ends() {
    let db = ioc().await;
    let h = db.async_handle();
    let flnk = h.name_to_dbaddr("T3.FLNK", None).unwrap();
    let put = h.with_held("AX", |held| held.db_put_field(&flnk, text("BY")));
    assert_eq!(link(&db, "T3.FLNK"), "");
    assert_eq!(answer(&h, put), Answer::Unheld(Ok(())));
    assert_eq!(link(&db, "T3.FLNK"), "BY");
}

/// DISP, the request type and `dbParseLink` answer the put in the hold.
#[epics_macros_rs::epics_test]
async fn only_the_refusals_before_the_lock_answer() {
    let db = ioc().await;
    let h = db.async_handle();
    let addr = |pv| h.name_to_dbaddr(pv, None).unwrap();
    let put = |pv, value| {
        answer(
            &h,
            h.with_held("AX", |held| held.db_put_field(&addr(pv), value)),
        )
    };
    assert!(
        matches!(put("D.FLNK", text("BY")), Answer::Held(Err(_))),
        "DISP"
    );
    assert!(
        matches!(
            put("T3.FLNK", EpicsValue::Double(1.0)),
            Answer::Held(Err(_))
        ),
        "request type"
    );
    assert!(
        matches!(
            put("T3.FLNK", text(r#"{"nosuch":{}}"#)),
            Answer::Held(Err(_))
        ),
        "dbParseLink"
    );
    assert_eq!(link(&db, "T3.FLNK"), "");
    assert_eq!(link(&db, "D.FLNK"), "");
}

/// `dbCanSetLink` refuses under the lock (`dbAccess.c:1135-1139`): the
/// link stays as it was, and the refusal is the relink's status, which
/// StreamEpics.cc's `matchValue` reports (`:1469`).
#[epics_macros_rs::epics_test]
async fn a_refusal_under_the_lock_is_the_link_put_s_status() {
    let db = ioc().await;
    let h = db.async_handle();
    let out = h.name_to_dbaddr("T3.OUT", None).unwrap();
    let put = h.with_held("AX", |held| held.db_put_field(&out, text("@instio")));
    let r = answer(&h, put);
    assert!(matches!(r, Answer::Unheld(Err(_))), "{r:?}");
    assert_eq!(link(&db, "T3.OUT"), "");
}
