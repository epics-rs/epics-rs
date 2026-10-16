//! Completions owed while a `DeviceDone` pass runs are drained by that run,
//! in order, even with `callbackParallelThreads` > 1: C's asyn callbacks
//! process the record one at a time on the port thread
//! (devMcaAsyn.c:357-382), never on two workers at once.

mod declared_rows;

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, Record};
use epics_base_rs::types::EpicsValue;

const PASSES: usize = 8;

type Ran = Arc<Mutex<Vec<ThreadId>>>;
type Queued = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;
type Started = Arc<Mutex<Option<mpsc::Sender<()>>>>;

/// Queues on the first pass; reads on every re-entered one, the first of
/// them slowly, after telling the test it is under way.
struct Queues {
    ran: Ran,
    queued: Queued,
    started: Started,
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
        *self.queued.lock().unwrap() = Some((done.device_done(), done.priority()));
        Ok(ReadStart::Pending)
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let mut ran = self.ran.lock().unwrap();
        ran.push(std::thread::current().id());
        record.set_val(EpicsValue::Double(ran.len() as f64))?;
        drop(ran);
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Queues"
    }
}

#[epics_macros_rs::epics_test]
async fn passes_owed_mid_run_are_drained_by_that_run() {
    epics_base_rs::runtime::background::callback_executor::set_parallel_threads(4, None);
    let ran: Ran = Arc::default();
    let queued: Queued = Arc::default();
    let (started_tx, started_rx) = mpsc::channel();
    let started: Started = Arc::new(Mutex::new(Some(started_tx)));
    let (r, q) = (Arc::clone(&ran), Arc::clone(&queued));
    let devices = [("ai", declared_rows::rows(&["Queues"], DbLinkType::Constant))];
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "Queues").then(|| {
                Box::new(Queues {
                    ran: Arc::clone(&r),
                    queued: Arc::clone(&q),
                    started: Arc::clone(&started),
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(
            r#"record(ai, "AI") { field(DTYP, "Queues") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    db.process_record("AI").await.unwrap();
    let (done, band) = queued.lock().unwrap().take().expect("a read was queued");
    epics_base_rs::runtime::task::spawn_blocking_background(band, move |_| {
        done.reprocess(band).unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first pass ran");
        for _ in 1..PASSES {
            done.reprocess(band).unwrap();
        }
    })
    .await
    .unwrap();

    for _ in 0..400 {
        if ran.lock().unwrap().len() >= PASSES {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    let ran = ran.lock().unwrap();
    assert_eq!(ran.len(), PASSES, "one pass per completion");
    assert!(
        ran.iter().all(|id| *id == ran[0]),
        "every pass owed mid-run ran on the run's worker: {ran:?}"
    );
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(PASSES as f64));
}
