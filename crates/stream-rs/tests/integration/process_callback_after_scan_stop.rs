//! A protocol's finish requests the record's `processCallback`
//! (StreamEpics.cc:1038-1043), judged once, when requested: one made before
//! `scanStop` ends the record's cycle after it, and one made after it never
//! runs, as C's after `callbackStop` (callback.c:210-235, iocInit.c:733-735).

use crate::redirect;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::background::{CallbackBody, CallbackCell};
use epics_base_rs::runtime::task::{CallbackPriority, request_cell, sleep, test_block_on};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use redirect::{db_path, get, ioc};
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// The stream waiting for its reply.
static WAITING: Mutex<Option<Weak<dyn Client>>> = Mutex::new(None);

/// Bus `Q`: takes the lock and the write at once and leaves the read for
/// [`answer`].
struct Queued(Weak<dyn Client>);

impl StreamBusInterface for Queued {
    fn lock_request(&self, _: u64) -> bool {
        if let Some(c) = self.0.upgrade() {
            c.lock_callback(StreamIoStatus::StreamIoSuccess);
        }
        true
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, _: &[u8], _: u64) -> bool {
        if let Some(c) = self.0.upgrade() {
            c.write_callback(StreamIoStatus::StreamIoSuccess);
        }
        true
    }
    fn read_request(&self, _: u64, _: u64, _: isize, _: bool) -> bool {
        *WAITING.lock().unwrap() = Some(self.0.clone());
        true
    }
}

fn queued(
    client: Weak<dyn Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "Q").then(|| Box::new(Queued(client)) as Box<dyn StreamBusInterface>)
}

/// Reply "5" to the waiting read from a thread of its own, as a port
/// thread does, and wait for that call to return: the finish it makes is
/// requested by then.
async fn answer() {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    let client = loop {
        if let Some(c) = WAITING.lock().unwrap().take().and_then(|c| c.upgrade()) {
            break c;
        }
        assert!(Instant::now() < deadline, "no read was requested");
        sleep(Duration::from_millis(5)).await;
    };
    let returned = Arc::new(AtomicBool::new(false));
    let done = returned.clone();
    std::thread::spawn(move || {
        client.read_callback(StreamIoStatus::StreamIoEnd, ReadInput::Bytes(b"5\n"));
        done.store(true, Ordering::Release);
    });
    while !returned.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the reply never returned");
        sleep(Duration::from_millis(5)).await;
    }
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
struct BusyLow(Shared);

impl BusyLow {
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
        BusyLow(shared)
    }
}

impl Drop for BusyLow {
    fn drop(&mut self) {
        let (hold, cv) = &*self.0;
        hold.lock().unwrap().released = true;
        cv.notify_all();
    }
}

/// An IOC with `r`, a low-priority ai read over bus `Q`. Its reply sets VAL
/// when it is read, as C's `readData` does (StreamEpics.cc:1489,
/// devaiStream.c:62); the cycle ends with PACT 0.
async fn queued_ai(test: &str) -> Arc<PvDatabase> {
    bus::register_stream_bus_interface("Queued", queued);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("q.proto");
    std::fs::write(&file, "Terminator = LF;\nq { out \"SQ\"; in \"%f\"; }\n").unwrap();
    ioc(&format!(
        r#"record(ai, "r") {{ field(DTYP, "stream") field(INP, "@{} q Q") }}"#,
        db_path(&file)
    ))
    .await
}

async fn put_proc(db: &PvDatabase) {
    db.put_record_field_from_ca_no_notify("r", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
}

fn f(db: &PvDatabase, pv: &str) -> f64 {
    get(db, pv).to_f64().unwrap()
}

/// Everything requested on the low band before this has run.
async fn low_band_drained() {
    let shared = Shared::default();
    shared.0.lock().unwrap().released = true;
    request_cell(
        CallbackPriority::Low,
        &CallbackCell::new(Holder(shared.clone())),
    )
    .unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !shared.0.lock().unwrap().running {
        assert!(Instant::now() < deadline, "the low band never drained");
        sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn a_finish_requested_before_the_scan_stopped_ends_the_cycle() {
    test_block_on(async {
        let db = queued_ai("a_finish_requested_before_the_scan_stopped").await;
        let busy = BusyLow::take();
        put_proc(&db).await;
        answer().await;
        db.scan_control().stop();
        drop(busy);
        let deadline = Instant::now() + fact_budget::FACT_BUDGET;
        while f(&db, "r.PACT") != 0.0 {
            assert!(Instant::now() < deadline, "the cycle never ended");
            sleep(Duration::from_millis(5)).await;
        }
    });
}

#[test]
fn a_finish_requested_after_the_scan_stopped_runs_nothing() {
    test_block_on(async {
        let db = queued_ai("a_finish_requested_after_the_scan_stopped").await;
        put_proc(&db).await;
        db.scan_control().stop();
        answer().await;
        low_band_drained().await;
        assert_eq!(f(&db, "r.PACT"), 1.0, "the cycle ended");
    });
}
