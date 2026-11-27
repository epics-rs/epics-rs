//! C `iocShutdown` stops the callback threads right after the scan facility
//! (iocInit.c:733-735). `callbackRequest` reads no state and `callbackTask`
//! empties its ring before it rereads `shutdown` (callback.c:210-235,
//! :341-376), so a callback requested before `callbackStop` still runs and
//! one requested after it never does; a delayed one is requested when its
//! timer fires (callback.c:404-408). So a seq group behind its `DLYn`
//! (seqRecord.c:214, `processCallback`) and a throttle's cooldown `valuePut`
//! (throttleRecord.c:595, `delayFuncCallback`) land by when they were
//! requested, not by when a busy band reaches them. An I/O Intr swait's
//! input change is a `callbackRequest` of `ioIntProcess` made on the
//! recDynLink thread (swaitRecord.c:846), which stores the input and
//! processes the record.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::background::{CallbackBody, CallbackCell};
use epics_base_rs::runtime::task::{CallbackPriority, request_cell, spawn_background_after};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{ProcessAction, ProcessActions, ProcessOutcome, Record};
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const DELAY: f64 = 0.3;

/// C throttle's cooldown reduced to its timer: each `process()` arms
/// `delayFuncCb` for `DLY`, and `FIRES` counts the `delayFuncCallback` runs.
#[derive(Default)]
struct Cooldown {
    dly: f64,
    fires: i32,
}

impl Record for Cooldown {
    fn record_type(&self) -> &'static str {
        "cooldown"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete_with(vec![
            ProcessAction::ValuePutAfter(Duration::from_secs_f64(self.dly)),
        ]))
    }
    fn delayed_value_put(&mut self) -> ProcessActions {
        self.fires += 1;
        ProcessActions::new()
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "DLY" => Some(EpicsValue::Double(self.dly)),
            "FIRES" => Some(EpicsValue::Long(self.fires)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "DLY" => self.dly = value.to_f64().unwrap_or(0.0),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
}

const DB: &str = r#"
record(ao, "A") {}
record(ao, "B") {}
record(seq, "S") {
    field(DO0, "1")
    field(LNK0, "A.VAL PP")
    field(DLY1, "0.3")
    field(DO1, "2")
    field(LNK1, "B.VAL PP")
}
record(cooldown, "T") { field(DLY, "0.3") }
record(cooldown, "U") { field(DLY, "0.05") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("cooldown", || Box::new(Cooldown::default()))
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

fn fires(db: &PvDatabase, rec: &str) -> i32 {
    match db.get_record(rec).unwrap().read().record.get_field("FIRES") {
        Some(EpicsValue::Long(n)) => n,
        other => panic!("{rec}.FIRES = {other:?}"),
    }
}

fn pact(db: &PvDatabase, rec: &str) -> bool {
    db.get_record(rec).unwrap().read().is_processing()
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

/// The low band's one worker, busy until dropped: what it is asked to run
/// meanwhile stays requested.
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

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = epics_base_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// Past the delay with margin: a callback still due would have run by now.
async fn past_the_delay() {
    // absence-window: a callback refused at its expiry leaves nothing to wait on.
    epics_base_rs::runtime::task::sleep(Duration::from_secs_f64(DELAY * 2.0)).await;
}

/// A delay of at most `DELAY` armed before this call has expired and made its
/// request: the one timer fires in deadline order.
async fn the_delay_expired() {
    let expired = Arc::new(AtomicBool::new(false));
    let flag = expired.clone();
    drop(spawn_background_after(
        CallbackPriority::High,
        Duration::from_secs_f64(DELAY),
        async move { flag.store(true, Ordering::Release) },
    ));
    until("the delay expired", || expired.load(Ordering::Acquire)).await;
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_behind_its_delay_runs_while_the_ioc_runs() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    until("B=2", || read(&db, "B") == 2.0).await;
}

/// C `iocPause` runs `scanPause` and leaves the callback threads up
/// (iocInit.c:281-297).
#[epics_macros_rs::epics_test]
async fn a_seq_group_behind_its_delay_runs_while_the_ioc_is_paused() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    db.scan_control().pause();
    until("B=2", || read(&db, "B") == 2.0).await;
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_behind_its_delay_never_runs_once_shutdown_began() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    db.scan_control().stop();
    past_the_delay().await;
    assert_eq!(read(&db, "B"), 0.0, "group 1 ran after scanStop");
}

#[epics_macros_rs::epics_test]
async fn a_throttle_cooldown_fires_while_the_ioc_runs() {
    let db = ioc().await;
    process(&db, "T").await;
    until("the cooldown", || fires(&db, "T") == 1).await;
}

#[epics_macros_rs::epics_test]
async fn a_throttle_cooldown_never_fires_once_shutdown_began() {
    let db = ioc().await;
    process(&db, "T").await;
    db.scan_control().stop();
    past_the_delay().await;
    assert_eq!(fires(&db, "T"), 0, "delayFuncCallback ran after scanStop");
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_requested_before_shutdown_runs_once_the_band_frees() {
    let db = ioc().await;
    let band = BusyBand::take();
    process(&db, "S").await;
    assert_eq!(read(&db, "A"), 0.0);
    db.scan_control().stop();
    drop(band);
    until("A=1", || read(&db, "A") == 1.0).await;
    // Group 1's delay expires after the stop: a request refused.
    past_the_delay().await;
    assert_eq!(read(&db, "B"), 0.0, "group 1 ran after scanStop");
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_whose_delay_expired_before_shutdown_runs_once_the_band_frees() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    let band = BusyBand::take();
    the_delay_expired().await;
    assert_eq!(read(&db, "B"), 0.0);
    db.scan_control().stop();
    drop(band);
    until("B=2", || read(&db, "B") == 2.0).await;
}

#[epics_macros_rs::epics_test]
async fn a_throttle_cooldown_that_expired_before_shutdown_fires_once_the_band_frees() {
    let db = ioc().await;
    let band = BusyBand::take();
    process(&db, "U").await;
    the_delay_expired().await;
    assert_eq!(fires(&db, "U"), 0);
    db.scan_control().stop();
    drop(band);
    until("the cooldown", || fires(&db, "U") == 1).await;
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_requested_after_shutdown_never_runs() {
    let db = ioc().await;
    db.scan_control().stop();
    process(&db, "S").await;
    assert!(pact(&db, "S"), "the seq went asynchronous");
    past_the_delay().await;
    assert_eq!(read(&db, "A"), 0.0, "group 0 ran after scanStop");
}

const SWAIT_DB: &str = r#"
record(ao, "SRC") {}
record(ao, "MARK") {}
record(swait, "W") { field(SCAN, "I/O Intr") field(INAN, "SRC") field(CALC, "A") }
record(swait, "X") { field(SCAN, "I/O Intr") field(INAN, "MARK") field(CALC, "A") field(PRIO, "HIGH") }
"#;

/// A running IOC whose `W` watches `SRC` on the low band and `X` watches
/// `MARK` on the high one, their inputs connected.
async fn swait_ioc() -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(SWAIT_DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    until("the inputs connected", || {
        read(&db, "W.INAV") == 0.0 && read(&db, "X.INAV") == 0.0
    })
    .await;
    db
}

async fn put(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_swait_input_change_after_shutdown_never_processes_it() {
    let db = swait_ioc().await;
    put(&db, "SRC", 3.0).await;
    until("W.A=3", || read(&db, "W.A") == 3.0).await;
    db.scan_control().stop();
    put(&db, "SRC", 5.0).await;
    past_the_delay().await;
    assert_eq!(
        (read(&db, "W.A"), read(&db, "W")),
        (3.0, 3.0),
        "ioIntProcess ran after scanStop"
    );
}

#[epics_macros_rs::epics_test]
async fn a_swait_input_change_before_shutdown_processes_it_once_the_band_frees() {
    let db = swait_ioc().await;
    let band = BusyBand::take();
    put(&db, "SRC", 5.0).await;
    put(&db, "MARK", 5.0).await;
    // One event queue delivers SRC's change before MARK's.
    until("X=5", || read(&db, "X") == 5.0).await;
    assert_eq!(read(&db, "W.A"), 0.0);
    db.scan_control().stop();
    drop(band);
    until("W=5", || read(&db, "W.A") == 5.0 && read(&db, "W") == 5.0).await;
}
