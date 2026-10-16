//! `DTYP "Async Soft Channel"` on an input ends its cycle as C's
//! `devAiSoftCallback` does: the notify's `doneCallback` calls
//! `callbackRequestProcessCallback(&pdevPvt->callback, prec->prio, prec)`
//! (`devAiSoftCallback.c:70-75`), so the completing pass runs on the band PRIO
//! names when the read lands. The re-entry used to run inside the task that
//! waited for the notify, on the band read at the first pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource};
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

type Pending = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

/// The notified target: its write waits for the test.
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

#[epics_macros_rs::epics_test]
async fn an_async_soft_input_completes_on_the_band_its_prio_names_now() {
    let pending: Pending = Arc::default();
    let passes = Arc::new(Mutex::new(Vec::new()));
    let (p, s) = (Arc::clone(&pending), Arc::clone(&passes));
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| match ctx.dtyp {
            "Submits" => Some(Box::new(Submits(Arc::clone(&p))) as Box<dyn DeviceSupport>),
            "Sink" => Some(Box::new(Sink(Arc::clone(&s))) as Box<dyn DeviceSupport>),
            _ => None,
        })
        .db_string(
            r#"
            record(ao, "TGT") { field(DTYP, "Submits") field(VAL, "7") }
            record(ai, "AI") {
                field(DTYP, "Async Soft Channel") field(INP, "TGT") field(FLNK, "SINK")
                field(PRIO, "LOW")
            }
            record(ao, "SINK") { field(DTYP, "Sink") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    // The first pass notifies TGT, whose write stays queued.
    db.process_record("AI").await.unwrap();
    assert!(db.get_record("AI").unwrap().read().is_processing());
    let (done, band) = pending.lock().unwrap().take().expect("TGT was processed");

    // PRIO moves while the read is in flight.
    db.get_record("AI").unwrap().write().set_prio(2);
    std::thread::spawn(move || done.complete(band, Ok(())).unwrap())
        .join()
        .unwrap();

    for _ in 0..500 {
        if !db.get_record("AI").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert!(!db.get_record("AI").unwrap().read().is_processing());
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(7.0));
    // A second completing pass would land within this window.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let passes = passes.lock().unwrap().clone();
    assert_eq!(passes.len(), 1, "one completing pass: {passes:?}");
    assert!(passes[0].starts_with("cbHigh"), "{passes:?}");
}
