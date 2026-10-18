//! An asynchronous device write ends its cycle as C devAsyn's does:
//! `processCallbackOutput` calls
//! `callbackRequestProcessCallback(&pPvt->processCallback, pr->prio, pr)` from
//! the port thread (devAsynFloat64.c:351), so the completing pass — and the
//! FLNK it fires — runs once, on the callback band the record's `PRIO`
//! selects. It used to run on a spawned task that waited for the write.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

type Pending = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

/// Submits every write and hands its completion to the test, which plays the
/// port thread.
struct Submits(Pending);

impl DeviceSupport for Submits {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        unreachable!("write_begin always submits")
    }
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        *self.0.lock().unwrap() = Some((done.device_done(), done.priority()));
        Ok(WriteStart::Pending)
    }
    fn dtyp(&self) -> &str {
        "Submits"
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
    for name in ["AO", "SINK"] {
        db.add_record(name, Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
    }
    {
        let rec = db.get_record("AO").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("SINK".into()))
            .unwrap();
    }
    db.ioc_init().await;
    let pending: Pending = Arc::default();
    let passes = Arc::new(Mutex::new(Vec::new()));
    {
        let rec = db.get_record("AO").unwrap();
        let mut inst = rec.write();
        inst.set_prio(prio);
        inst.common.udf = 0;
        inst.common.dtyp = "Submits".into();
        inst.device = Some(Box::new(Submits(Arc::clone(&pending))));
    }
    {
        let rec = db.get_record("SINK").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.common.dtyp = "Sink".into();
        inst.device = Some(Box::new(Sink(Arc::clone(&passes))));
    }

    db.put_record_field_from_ca_no_notify("AO", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(db.get_record("AO").unwrap().read().is_processing());
    assert!(
        passes.lock().unwrap().is_empty(),
        "no FLNK before the write lands"
    );

    let (done, band) = pending
        .lock()
        .unwrap()
        .take()
        .expect("the write was submitted");
    std::thread::Builder::new()
        .name("port".into())
        .spawn(move || done.complete(band).unwrap())
        .unwrap()
        .join()
        .unwrap();

    for _ in 0..500 {
        if !db.get_record("AO").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert!(!db.get_record("AO").unwrap().read().is_processing());
    // A second completing pass would land within this window.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    passes.lock().unwrap().clone()
}

#[epics_macros_rs::epics_test]
async fn a_high_prio_write_completes_once_on_cb_high() {
    let passes = completes_on(2).await;
    assert_eq!(passes.len(), 1, "one completing pass: {passes:?}");
    assert!(passes[0].starts_with("cbHigh"), "{passes:?}");
}

#[epics_macros_rs::epics_test]
async fn a_low_prio_write_completes_once_on_cb_low() {
    let passes = completes_on(0).await;
    assert_eq!(passes.len(), 1, "one completing pass: {passes:?}");
    assert!(passes[0].starts_with("cbLow"), "{passes:?}");
}

/// C `iocShutdown` stops the callback threads right after the scan facility
/// (iocInit.c:733-735): a write a closing port completes at exit ends no cycle.
#[epics_macros_rs::epics_test]
async fn a_completion_after_the_scan_stopped_processes_nothing() {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.ioc_init().await;
    let pending: Pending = Arc::default();
    {
        let rec = db.get_record("AO").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.common.dtyp = "Submits".into();
        inst.device = Some(Box::new(Submits(Arc::clone(&pending))));
    }
    db.put_record_field_from_ca_no_notify("AO", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    db.scan_control().stop();
    let (done, band) = pending.lock().unwrap().take().expect("submitted");
    std::thread::spawn(move || done.complete(band).unwrap())
        .join()
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    assert!(db.get_record("AO").unwrap().read().is_processing());
}
