//! C sseq `process` lists a link group only when `lnk.type != CONSTANT ||
//! dol.type != CONSTANT` (sseqRecord.c:355-356), so a group whose `DOLn` is a
//! number and whose `LNKn` is empty is no step at all: its `DLYn` is never
//! waited. An `ABORT` during a delay cancels the timer and makes its own
//! `callbackRequest` from `special` (sseqRecord.c:1199-1212), so the
//! sequence finishes once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::background::{CallbackBody, CallbackCell};
use epics_base_rs::runtime::task::{CallbackPriority, request_cell, spawn_background_after};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "A") {}
record(ao, "SRC") { field(VAL, "7") }
record(sseq, "S") {
    field(DLY1, "3600")
    field(DOL1, "5")
    field(DOL2, "2")
    field(LNK2, "A.VAL PP")
}
record(sseq, "L") { field(DOL1, "SRC") }
record(sseq, "Q") {
    field(DLY1, "0.1")
    field(DO1, "1")
    field(LNK1, "A.VAL PP")
}
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

fn read(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

#[epics_macros_rs::epics_test]
async fn a_group_with_a_number_dol_and_no_lnk_waits_no_delay() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=2 and BUSY=0", || {
        read(&db, "A") == 2.0 && read(&db, "S.BUSY") == 0.0
    })
    .await;
}

#[epics_macros_rs::epics_test]
async fn a_group_with_a_linked_dol_and_no_lnk_is_a_step() {
    let db = ioc().await;
    process(&db, "L").await;
    until("L.DO1=7 and BUSY=0", || {
        read(&db, "L.DO1") == 7.0 && read(&db, "L.BUSY") == 0.0
    })
    .await;
}

#[derive(Default)]
struct Hold {
    running: bool,
    released: bool,
}

type Shared = Arc<(Mutex<Hold>, Condvar)>;

struct Holder(Shared);

impl CallbackBody for Holder {
    fn run(&self) {
        let (hold, cv) = &*self.0;
        let mut hold = hold.lock().unwrap();
        hold.running = true;
        cv.notify_all();
        while !hold.released {
            hold = cv.wait(hold).unwrap();
        }
    }
}

/// The low band's one worker, busy until dropped.
struct BusyBand(Shared);

impl BusyBand {
    fn take() -> Self {
        let shared = Shared::default();
        request_cell(
            CallbackPriority::Low,
            &CallbackCell::new(Holder(shared.clone())),
        )
        .unwrap();
        let (hold, cv) = &*shared;
        let (held, wait) = cv
            .wait_timeout_while(hold.lock().unwrap(), fact_budget::FACT_BUDGET, |h| {
                !h.running
            })
            .unwrap();
        drop(held);
        assert!(!wait.timed_out(), "the low band never took the holder");
        BusyBand(shared)
    }
}

impl Drop for BusyBand {
    fn drop(&mut self) {
        let (hold, cv) = &*self.0;
        hold.lock().unwrap().released = true;
        cv.notify_all();
    }
}

/// A delay of at most `delay` armed before this call has expired and made its
/// request: the one timer fires in deadline order.
async fn expired(delay: Duration) {
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    drop(spawn_background_after(
        CallbackPriority::High,
        delay,
        async move { flag.store(true, Ordering::Release) },
    ));
    until("the delay expired", || fired.load(Ordering::Acquire)).await;
}

/// Step 1's delay expires while the band is busy, so its callback is already
/// requested when `ABORT` lands: the abort finishes the sequence and nothing
/// starts another one.
#[epics_macros_rs::epics_test]
async fn an_abort_after_the_delay_expired_finishes_the_sequence_once() {
    let db = ioc().await;
    process(&db, "Q").await;
    let band = BusyBand::take();
    expired(Duration::from_secs_f64(0.1)).await;
    // A plain put, as `caput` makes: a put-notify waits for the sequence.
    db.put_record_field_from_ca_no_notify("Q", "ABORT", EpicsValue::Short(1))
        .await
        .unwrap();
    drop(band);
    until("BUSY=0", || read(&db, "Q.BUSY") == 0.0).await;
    // absence-window: past step 1's delay again, a second sequence would have
    // written A.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        (read(&db, "A"), read(&db, "Q.BUSY")),
        (0.0, 0.0),
        "a sequence ran after the abort"
    );
}
