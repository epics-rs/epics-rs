//! The link classifiers type a local target as C `dbNameToAddr` does, with no
//! lock: `sCalcoutRecord.c:521` re-classifies a re-pointed link from
//! `special()` under its own record's lock alone, and `tableRecord.c`
//! `checkLinks` reads no other record. A target another thread holds is typed
//! at once, and a holder that types a link whose target's holder waits for
//! its own set does not deadlock.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::{DbfCode, EpicsValue};

// menu(scalcoutINAV) (sCalcoutRecord.dbd).
const LOC: f64 = 2.0;

async fn ioc(db: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

async fn join_within<T>(thread: &std::thread::JoinHandle<T>, what: &str) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !thread.is_finished() {
        assert!(Instant::now() < deadline, "{what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
}

/// Holds `names` on a thread of its own until the returned sender is used.
fn hold(
    db: &Arc<PvDatabase>,
    names: &'static [&'static str],
) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    fn nest(
        h: &epics_base_rs::server::database::AsyncDbHandle,
        names: &[&str],
        held: &mpsc::Sender<()>,
        go: &mpsc::Receiver<()>,
    ) {
        match names.split_first() {
            Some((first, rest)) => h.with_record(first, |_| nest(h, rest, held, go)),
            None => {
                held.send(()).unwrap();
                go.recv().unwrap();
            }
        }
    }
    let h = db.async_handle();
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || nest(&h, names, &held_tx, &go_rx));
    held_rx.recv().unwrap();
    (go_tx, holder)
}

#[epics_macros_rs::epics_test]
async fn a_held_target_is_typed_without_waiting_for_the_hold() {
    let db = ioc("record(ai, \"B\") {}\n\
         record(waveform, \"W\") { field(FTVL, \"LONG\") field(NELM, \"8\") }\n")
    .await;
    let (go, holder) = hold(&db, &["B", "W"]);
    let typed = {
        let h = db.async_handle();
        std::thread::spawn(move || {
            [
                h.link_target_field_type("B"),
                h.link_target_field_type("W.VAL NPP"),
            ]
        })
    };
    join_within(&typed, "link_target_field_type waited for the hold").await;
    go.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(
        typed.join().unwrap(),
        [Some(DbfCode::Double), Some(DbfCode::Long)]
    );
}

#[epics_macros_rs::epics_test]
async fn a_repointed_link_is_classified_while_its_target_is_held() {
    let db = ioc("record(ai, \"T\") {}\nrecord(scalcout, \"S\") { field(CALC, \"A\") }\n").await;
    let (go, holder) = hold(&db, &["T"]);
    db.put_record_field_from_ca_no_notify("S", "INPA", EpicsValue::String("T CA".into()))
        .await
        .unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    let mut status = None;
    while Instant::now() < deadline {
        status = db.get_pv("S.INAV").ok().and_then(|v| v.to_f64());
        if status == Some(LOC) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    go.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(status, Some(LOC), "INAV not classified while T was held");
}

#[epics_macros_rs::epics_test]
async fn a_holder_typing_a_link_does_not_deadlock_with_the_targets_holder() {
    let db = ioc("record(ai, \"X\") {}\nrecord(ai, \"B\") {}\n").await;
    let h = db.async_handle();
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    // B's holder then waits for X's set.
    let b_holder = {
        let h = h.clone();
        std::thread::spawn(move || {
            h.with_record("B", |_| {
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                h.with_record("X", |_| {});
            })
        })
    };
    held_rx.recv().unwrap();
    // X's holder types a link to B.
    let x_holder = std::thread::spawn(move || {
        let mut typed = None;
        h.with_record("X", |_| {
            go_tx.send(()).unwrap();
            // B's holder is waiting on X by now.
            std::thread::sleep(Duration::from_millis(200));
            typed = h.link_target_field_type("B");
        });
        typed
    });
    join_within(&x_holder, "the holders deadlocked").await;
    join_within(&b_holder, "the holders deadlocked").await;
    b_holder.join().unwrap();
    assert_eq!(x_holder.join().unwrap(), Some(DbfCode::Double));
}
