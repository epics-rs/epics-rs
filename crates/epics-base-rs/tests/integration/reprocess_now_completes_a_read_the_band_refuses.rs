//! C devMcaAsyn's port-thread callback ends a queued read with
//! `dbScanLock; process; dbScanUnlock` on that thread (devMcaAsyn.c:359-361,
//! :381-383), never through `callbackRequest`. A completion that IS queued
//! and finds the band full is lost (`S_db_bufFull`, callback.c:368-373), and
//! the record stays PACT.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::background::CallbackError;
use epics_base_rs::runtime::background::callback_executor::{set_parallel_threads, set_queue_size};
use epics_base_rs::runtime::task::{
    CallbackPriority, background_callback_stats, spawn_blocking_background,
};
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

type Pending = Arc<Mutex<Option<DeviceDone>>>;

/// Queues every idle read; the completing pass reads 7 and names its thread.
struct Queues {
    pending: Pending,
    reads: Arc<Mutex<Vec<String>>>,
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
        *self.pending.lock().unwrap() = Some(done.device_done());
        Ok(ReadStart::Pending)
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let thread = std::thread::current().name().unwrap_or("").to_string();
        self.reads.lock().unwrap().push(thread);
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

/// Holds the low band's worker until dropped.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn wait(&self) {
        let (open, wake) = &*self.0;
        let mut open = open.lock().unwrap();
        while !*open {
            open = wake.wait(open).unwrap();
        }
    }
}

struct FullBand(Gate);

impl Drop for FullBand {
    fn drop(&mut self) {
        let (open, wake) = &*(self.0).0;
        *open.lock().unwrap() = true;
        wake.notify_all();
    }
}

fn low_overflows() -> u64 {
    background_callback_stats(false).unwrap()[CallbackPriority::Low.index()].num_overflow
}

/// Hold the low band's one worker, then take its one ring slot with ad-hoc
/// requests until one is refused.
fn fill_low_band() -> FullBand {
    let gate = Gate::default();
    let (running_tx, running_rx) = std::sync::mpsc::channel();
    let holder = gate.clone();
    drop(spawn_blocking_background(
        CallbackPriority::Low,
        move |_| {
            running_tx.send(()).unwrap();
            holder.wait();
        },
    ));
    running_rx.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
    let refused = low_overflows() + 1;
    for _ in 0..64 {
        let filler = gate.clone();
        drop(spawn_blocking_background(
            CallbackPriority::Low,
            move |_| filler.wait(),
        ));
        if low_overflows() == refused {
            return FullBand(gate);
        }
    }
    panic!("the low band never filled");
}

/// `AI` at PRIO LOW with its read queued, the band full behind it.
async fn queued_read() -> (PvDatabase, DeviceDone, Arc<Mutex<Vec<String>>>, FullBand) {
    // `callbackSetQueueSize` and `callbackParallelThreads` before `callbackInit`.
    set_queue_size(1);
    set_parallel_threads(1, Some(CallbackPriority::Low));
    let db = PvDatabase::new();
    epics_base_rs::runtime::task::background_init();
    db.add_record("AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.ioc_init().await;
    let pending = Pending::default();
    let reads = Arc::new(Mutex::new(Vec::new()));
    {
        let rec = db.get_record("AI").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "Queues".into();
        inst.device = Some(Box::new(Queues {
            pending: Arc::clone(&pending),
            reads: Arc::clone(&reads),
        }));
    }
    db.process_record("AI").await.unwrap();
    assert!(db.get_record("AI").unwrap().read().is_processing());
    let done = pending.lock().unwrap().take().expect("the read was queued");
    assert_eq!(done.priority(), CallbackPriority::Low);
    let full = fill_low_band();
    (db, done, reads, full)
}

fn val(db: &PvDatabase) -> Option<EpicsValue> {
    db.get_record("AI").unwrap().read().record.val()
}

#[epics_macros_rs::epics_test]
async fn a_read_completion_the_full_band_refuses_leaves_the_record_active() {
    let (db, done, reads, full) = queued_read().await;
    assert_eq!(
        done.reprocess(CallbackPriority::Low),
        Err(CallbackError::QueueFull)
    );
    drop(full);
    // A pass still owed would run on the freed band within this window.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    assert!(db.get_record("AI").unwrap().read().is_processing());
    assert_eq!(val(&db), Some(EpicsValue::Double(0.0)));
    assert!(reads.lock().unwrap().is_empty());
}

#[epics_macros_rs::epics_test]
async fn a_read_completed_now_runs_on_the_port_thread_past_a_full_band() {
    let (db, done, reads, _full) = queued_read().await;
    std::thread::Builder::new()
        .name("port".into())
        .spawn(move || done.reprocess_now())
        .unwrap()
        .join()
        .unwrap();
    let rec = db.get_record("AI").unwrap();
    let inst = rec.read();
    assert!(!inst.is_processing(), "the pass ended the cycle");
    assert_eq!(inst.record.val(), Some(EpicsValue::Double(7.0)));
    assert_eq!(inst.common.udf, 0);
    assert_eq!(*reads.lock().unwrap(), ["port"]);
}
