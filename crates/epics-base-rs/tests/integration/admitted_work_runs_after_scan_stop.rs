//! C judges each request against the scan state once, when it is made, and
//! runs what it admitted. `callbackRequest` reads no state and
//! `callbackTask` empties its ring before it rereads `shutdown`
//! (callback.c:210-235, :341-376), whose `ProcessCallback` body has no gate
//! (:379-387); `callbackStop` follows `scanStop` (iocInit.c:733-735), so a
//! request after it never runs. `postEvent` checks `scanCtl` when it posts
//! and `eventCallback` scans with no check (dbScan.c:459-465, :538-546).
//! `scanStop` drains the `scanOnce` ring before `onceTask` exits, and a
//! `scanOnce` after it is never run (dbScan.c:158-181, :700-724).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::background::{CallbackBody, CallbackCell};
use epics_base_rs::runtime::task::{CallbackPriority, background_scan_once, request_cell};
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, ProcStack, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "C") { field(CALC, "VAL+1") }
record(calc, "T") { field(SCAN, "Event") field(EVNT, "7") field(CALC, "VAL+1") }
record(event, "EV") { field(VAL, "7") }
record(ai, "AI") {}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.scan_control().run();
    db
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

#[derive(Default)]
struct Hold {
    running: bool,
    released: bool,
}

type Shared = Arc<(Mutex<Hold>, Condvar)>;

fn hold(shared: &Shared) {
    let (hold, cv) = &**shared;
    let mut hold = hold.lock().unwrap();
    hold.running = true;
    cv.notify_all();
    while !hold.released {
        hold = cv.wait(hold).unwrap();
    }
}

struct Holder(Shared);

impl CallbackBody for Holder {
    fn run(&self) {
        hold(&self.0);
    }
}

/// A worker busy until dropped: what it is asked to run meanwhile stays
/// requested.
struct Busy(Shared);

impl Busy {
    /// The band's one worker.
    fn band(band: CallbackPriority) -> Self {
        let shared = Shared::default();
        request_cell(band, &CallbackCell::new(Holder(shared.clone()))).unwrap();
        Self::taken(shared)
    }

    /// The `scanOnce` thread.
    fn scan_once() -> Self {
        let shared = Shared::default();
        let held = shared.clone();
        background_scan_once("HOLD", Box::new(move || hold(&held))).unwrap();
        Self::taken(shared)
    }

    fn taken(shared: Shared) -> Self {
        let (hold, cv) = &*shared;
        let (held, wait) = cv
            .wait_timeout_while(hold.lock().unwrap(), fact_budget::FACT_BUDGET, |h| {
                !h.running
            })
            .unwrap();
        drop(held);
        assert!(!wait.timed_out(), "the worker never took the holder");
        Busy(shared)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let (hold, cv) = &*self.0;
        hold.lock().unwrap().released = true;
        cv.notify_all();
    }
}

struct Flag(Arc<AtomicBool>);

impl CallbackBody for Flag {
    fn run(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Everything requested on `band` before this has run.
async fn band_drained(band: CallbackPriority) {
    let ran = Arc::new(AtomicBool::new(false));
    request_cell(band, &CallbackCell::new(Flag(ran.clone()))).unwrap();
    until("the band drained", || ran.load(Ordering::Acquire)).await;
}

/// Everything queued on the `scanOnce` ring before this has run.
async fn scan_once_drained() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    background_scan_once(
        "SENTINEL",
        Box::new(move || flag.store(true, Ordering::Release)),
    )
    .unwrap();
    until("the scanOnce ring drained", || ran.load(Ordering::Acquire)).await;
}

type Pending = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

/// Queues each read and hands its completion to the test, the port thread.
struct Queues {
    pending: Pending,
    reads: Arc<AtomicUsize>,
}

impl DeviceSupport for Queues {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        if done.pact() {
            return Ok(ReadStart::Synchronous);
        }
        *self.pending.lock().unwrap() = Some((done.device_done(), done.priority()));
        Ok(ReadStart::Pending)
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        record.set_val(EpicsValue::Double(7.0))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Queues"
    }
}

/// `AI` with its read queued: its completion, and the count of reads.
async fn queued_read(db: &PvDatabase) -> (DeviceDone, CallbackPriority, Arc<AtomicUsize>) {
    let pending = Pending::default();
    let reads = Arc::new(AtomicUsize::new(0));
    {
        let rec = db.get_record("AI").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "Queues".into();
        inst.device = Some(Box::new(Queues {
            pending: pending.clone(),
            reads: reads.clone(),
        }));
    }
    process(db, "AI").await;
    let (done, band) = pending.lock().unwrap().take().expect("the read queued");
    (done, band, reads)
}

fn pact(db: &PvDatabase, rec: &str) -> bool {
    db.get_record(rec).unwrap().read().is_processing()
}

#[epics_macros_rs::epics_test]
async fn a_read_completion_requested_before_the_scan_stopped_ends_its_cycle() {
    let db = ioc().await;
    let (done, band, reads) = queued_read(&db).await;
    let busy = Busy::band(band);
    done.reprocess(band).unwrap();
    db.scan_control().stop();
    drop(busy);
    until("the read completed", || {
        reads.load(Ordering::SeqCst) == 1 && !pact(&db, "AI")
    })
    .await;
    assert_eq!(read(&db, "AI"), 7.0);
}

#[epics_macros_rs::epics_test]
async fn a_read_completion_requested_after_the_scan_stopped_processes_nothing() {
    let db = ioc().await;
    let (done, band, reads) = queued_read(&db).await;
    db.scan_control().stop();
    done.reprocess(band).unwrap();
    band_drained(band).await;
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(pact(&db, "AI"));
}

#[epics_macros_rs::epics_test]
async fn a_process_callback_requested_before_the_scan_stopped_processes() {
    let db = ioc().await;
    let cb = db.async_handle().process_callback("C");
    let busy = Busy::band(CallbackPriority::Low);
    cb.request(CallbackPriority::Low).unwrap();
    db.scan_control().stop();
    drop(busy);
    until("C processed", || read(&db, "C") == 1.0).await;
}

#[epics_macros_rs::epics_test]
async fn a_process_callback_requested_after_the_scan_stopped_processes_nothing() {
    let db = ioc().await;
    let cb = db.async_handle().process_callback("C");
    db.scan_control().stop();
    cb.request(CallbackPriority::Low).unwrap();
    band_drained(CallbackPriority::Low).await;
    assert_eq!(read(&db, "C"), 0.0);
}

/// The event record's post rides the middle band.
#[epics_macros_rs::epics_test]
async fn an_event_posted_before_the_scan_stopped_scans_its_records() {
    let db = ioc().await;
    let busy = Busy::band(CallbackPriority::Medium);
    process(&db, "EV").await;
    db.scan_control().stop();
    drop(busy);
    until("T processed", || read(&db, "T") == 1.0).await;
}

/// C `iocPause` stops `postEvent` (dbScan.c:538-539): a post made then is
/// not run by a later `iocRun`.
#[epics_macros_rs::epics_test]
async fn an_event_posted_while_paused_scans_nothing_once_resumed() {
    let db = ioc().await;
    let busy = Busy::band(CallbackPriority::Medium);
    db.scan_control().pause();
    process(&db, "EV").await;
    db.scan_control().run();
    drop(busy);
    band_drained(CallbackPriority::Medium).await;
    assert_eq!(read(&db, "T"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn an_event_posted_after_the_scan_stopped_scans_nothing() {
    let db = ioc().await;
    db.scan_control().stop();
    process(&db, "EV").await;
    band_drained(CallbackPriority::Medium).await;
    assert_eq!(read(&db, "T"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn a_scan_once_queued_before_the_scan_stopped_processes() {
    let db = ioc().await;
    let busy = Busy::scan_once();
    db.scan_once(b"C");
    db.scan_control().stop();
    drop(busy);
    until("C processed", || read(&db, "C") == 1.0).await;
}

#[epics_macros_rs::epics_test]
async fn a_scan_once_requested_after_the_scan_stopped_processes_nothing() {
    let db = ioc().await;
    db.scan_control().stop();
    db.scan_once(b"C");
    scan_once_drained().await;
    assert_eq!(read(&db, "C"), 0.0);
}
