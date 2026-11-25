//! C `scanOnce` queues every request (`dbScan.c:660-694`), so one record
//! requested faster than `onceTask` processes it fills `onceQ`, and every
//! other record's request is dropped until it drains. The port does not
//! queue a record that is already waiting: the process queued for it starts
//! after every request made meanwhile, and serves them all.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{background_scan_once, background_scan_once_stats, sleep};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

async fn ioc(db: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

const A_AND_B: &str = r#"record(calc, "A") { field(CALC, "VAL+1") }
record(calc, "B") { field(CALC, "VAL+1") }"#;

fn val(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

/// Keep the `scanOnce` thread busy until the returned sender is dropped.
fn hold_scan_once() -> Sender<()> {
    let (started_tx, started) = channel();
    let (release, released) = channel::<()>();
    background_scan_once(
        "HOLD",
        Box::new(move || {
            started_tx.send(()).unwrap();
            let _ = released.recv();
        }),
    )
    .unwrap();
    started
        .recv_timeout(fact_budget::FACT_BUDGET)
        .expect("the scanOnce thread never took the holder");
    release
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        sleep(Duration::from_millis(5)).await;
    }
}

/// Everything on the ring before this has run. The ring may still be full.
async fn ring_drained() {
    let ran = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        let flag = ran.clone();
        let sentinel = Box::new(move || flag.store(true, Ordering::Release));
        if background_scan_once("SENTINEL", sentinel).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the ring never took the sentinel"
        );
        sleep(Duration::from_millis(5)).await;
    }
    until("the scanOnce ring never drained", || {
        ran.load(Ordering::Acquire)
    })
    .await;
}

#[epics_macros_rs::epics_test]
async fn requests_made_while_the_record_waits_give_it_one_process() {
    let db = ioc(A_AND_B).await;
    let hold = hold_scan_once();
    db.scan_once(b"A");
    db.scan_once(b"A");
    drop(hold);
    ring_drained().await;
    assert_eq!(val(&db, "A"), 1.0);
}

#[epics_macros_rs::epics_test]
async fn one_records_repeated_requests_leave_room_for_another() {
    let db = ioc(A_AND_B).await;
    let hold = hold_scan_once();
    let size = background_scan_once_stats(false).unwrap().size;
    for _ in 0..size {
        db.scan_once(b"A");
    }
    db.scan_once(b"B");
    drop(hold);
    ring_drained().await;
    assert_eq!(val(&db, "B"), 1.0, "B's request was dropped");
}

static RUNS: AtomicU32 = AtomicU32::new(0);

/// A request made by the record's own process is not merged into it.
#[epics_macros_rs::epics_test]
async fn a_request_made_while_the_record_processes_gives_another_process() {
    let db = IocBuilder::new()
        .register_context_subroutine(
            "again",
            Box::new(|_rec, ctx| {
                if RUNS.fetch_add(1, Ordering::SeqCst) + 1 < 3 {
                    ctx.db().unwrap().scan_once(ctx.name().as_bytes());
                }
                Ok(0)
            }),
        )
        .db_string(
            r#"record(aSub, "SELF") { field(SNAM, "again") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.scan_once(b"SELF");
    until("the record stopped asking for itself", || {
        RUNS.load(Ordering::SeqCst) >= 3
    })
    .await;
    ring_drained().await;
    assert_eq!(RUNS.load(Ordering::SeqCst), 3);
}

/// A request the full ring refuses does not keep the record from being
/// queued again.
#[epics_macros_rs::epics_test]
async fn a_request_the_full_ring_refused_leaves_the_record_requestable() {
    let db = ioc(A_AND_B).await;
    let hold = hold_scan_once();
    let size = background_scan_once_stats(false).unwrap().size;
    for _ in 0..size {
        background_scan_once("FILL", Box::new(|| {})).unwrap();
    }
    db.scan_once(b"A");
    drop(hold);
    ring_drained().await;
    assert_eq!(val(&db, "A"), 0.0, "the full ring took A's request");
    db.scan_once(b"A");
    ring_drained().await;
    assert_eq!(val(&db, "A"), 1.0);
}
