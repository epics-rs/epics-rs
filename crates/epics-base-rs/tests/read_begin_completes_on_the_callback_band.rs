//! An asynchronous device read ends its cycle as C devAsyn's does: the first
//! pass queues the read and returns with PACT set (devAsynInt32.c:836-841),
//! the port thread reads and calls
//! `callbackRequestProcessCallback(&pPvt->processCallback, pr->prio, pr)`
//! (devAsynInt32.c:500), and the re-entered pass runs `read_ai` again with
//! PACT set, which applies the value and fires FLNK once on the `PRIO` band.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

type Pending = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

/// Queues every idle read and hands its completion to the test, which plays
/// the port thread.
struct Queues {
    pending: Pending,
    begins: Arc<AtomicUsize>,
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
        self.begins.fetch_add(1, Ordering::SeqCst);
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

/// The FLNK target: records the thread each of its passes ran on.
struct Sink(Arc<Mutex<Vec<String>>>);

impl DeviceSupport for Sink {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        let name = std::thread::current().name().unwrap_or("").to_string();
        self.0.lock().unwrap().push(name);
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Sink"
    }
}

async fn completes_on(prio: i16) -> Vec<String> {
    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    db.add_record("AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("SINK", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.get_record("AI")
        .unwrap()
        .write()
        .put_common_field("FLNK", EpicsValue::String("SINK".into()))
        .unwrap();
    db.ioc_init().await;
    let pending: Pending = Arc::default();
    let begins = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let passes = Arc::new(Mutex::new(Vec::new()));
    {
        let rec = db.get_record("AI").unwrap();
        let mut inst = rec.write();
        inst.set_prio(prio);
        inst.common.dtyp = "Queues".into();
        inst.device = Some(Box::new(Queues {
            pending: Arc::clone(&pending),
            begins: Arc::clone(&begins),
            reads: Arc::clone(&reads),
        }));
    }
    {
        let rec = db.get_record("SINK").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.common.dtyp = "Sink".into();
        inst.device = Some(Box::new(Sink(Arc::clone(&passes))));
    }

    db.process_record("AI").await.unwrap();
    {
        let rec = db.get_record("AI").unwrap();
        let inst = rec.read();
        assert!(inst.is_processing(), "the queued read holds PACT");
        assert_eq!(inst.record.val(), Some(EpicsValue::Double(0.0)));
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no read before it lands");
    assert!(passes.lock().unwrap().is_empty(), "no FLNK before it lands");

    // A process request while PACT is set is refused, not queued again.
    db.process_record("AI").await.unwrap();
    assert_eq!(begins.load(Ordering::SeqCst), 1);

    let (done, band) = pending.lock().unwrap().take().expect("the read was queued");
    std::thread::Builder::new()
        .name("port".into())
        .spawn(move || done.reprocess(band).unwrap())
        .unwrap()
        .join()
        .unwrap();

    for _ in 0..500 {
        if !db.get_record("AI").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    {
        let rec = db.get_record("AI").unwrap();
        let inst = rec.read();
        assert!(!inst.is_processing());
        assert_eq!(inst.record.val(), Some(EpicsValue::Double(7.0)));
        assert_eq!(inst.common.udf, 0);
    }
    // A second completing pass would land within this window.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        begins.load(Ordering::SeqCst),
        1,
        "the continuation does not queue"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the continuation reads once"
    );
    passes.lock().unwrap().clone()
}

#[epics_macros_rs::epics_test]
async fn a_high_prio_read_completes_once_on_cb_high() {
    let passes = completes_on(2).await;
    assert_eq!(passes.len(), 1, "one completing pass: {passes:?}");
    assert!(passes[0].starts_with("cbHigh"), "{passes:?}");
}

#[epics_macros_rs::epics_test]
async fn a_low_prio_read_completes_once_on_cb_low() {
    let passes = completes_on(0).await;
    assert_eq!(passes.len(), 1, "one completing pass: {passes:?}");
    assert!(passes[0].starts_with("cbLow"), "{passes:?}");
}

/// `AI` on a device that queues every read, and the count of its reads.
async fn queueing_ai(db: &PvDatabase, pending: &Pending) -> Arc<AtomicUsize> {
    db.add_record("AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.ioc_init().await;
    let reads = Arc::new(AtomicUsize::new(0));
    let rec = db.get_record("AI").unwrap();
    let mut inst = rec.write();
    inst.common.dtyp = "Queues".into();
    inst.device = Some(Box::new(Queues {
        pending: Arc::clone(pending),
        begins: Arc::default(),
        reads: Arc::clone(&reads),
    }));
    reads
}

/// Run the queued read's completion from a port thread, then let any pass it
/// queued land.
async fn complete(db: &PvDatabase, pending: &Pending) {
    let (done, band) = pending.lock().unwrap().take().expect("the read was queued");
    std::thread::spawn(move || done.reprocess(band).unwrap())
        .join()
        .unwrap();
    for _ in 0..100 {
        if !db.get_record("AI").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
}

/// A cancel between the queue and the completion makes the completion re-enter
/// nothing, as it does for every other async re-entry of the record.
#[epics_macros_rs::epics_test]
async fn a_cancel_before_the_completion_stops_the_reentry() {
    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    let pending = Pending::default();
    let reads = queueing_ai(&db, &pending).await;
    db.process_record("AI").await.unwrap();
    db.cancel_async_reentry("AI");
    complete(&db, &pending).await;
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(db.get_record("AI").unwrap().read().is_processing());
}

/// The completion re-enters on the generation it was queued at and leaves it
/// there, so a token the record held before the read stays current.
#[epics_macros_rs::epics_test]
async fn a_completion_leaves_the_record_s_token_current() {
    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    let pending = Pending::default();
    let reads = queueing_ai(&db, &pending).await;
    let token = db.mint_async_token("AI").unwrap();
    db.process_record("AI").await.unwrap();
    complete(&db, &pending).await;
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(token.is_current());
}

/// C `iocShutdown` stops the callback threads right after the scan facility
/// (iocInit.c:733-735): a read a closing port completes at exit processes
/// nothing, so no READ/INVALID is posted.
#[epics_macros_rs::epics_test]
async fn a_completion_after_the_scan_stopped_processes_nothing() {
    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    let pending = Pending::default();
    let reads = queueing_ai(&db, &pending).await;
    db.process_record("AI").await.unwrap();
    db.scan_control().stop();
    complete(&db, &pending).await;
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(db.get_record("AI").unwrap().read().is_processing());
}
