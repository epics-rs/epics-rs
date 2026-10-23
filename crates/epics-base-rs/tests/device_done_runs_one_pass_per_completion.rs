//! Two `DeviceDone` completions queued back-to-back run two passes, in order:
//! C's asyn callback processes the record once per callback
//! (devMcaAsyn.c:357-382). Both are requested from the record's own band
//! while it is busy, so the second lands while the first is still queued.

mod declared_rows;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

type Log = Arc<Mutex<Vec<String>>>;
type Queued = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

/// Queues on a pass it did not re-enter; reads on every re-entered one.
struct Queues {
    log: Log,
    queued: Queued,
    reads: f64,
}

impl DeviceSupport for Queues {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.log
            .lock()
            .unwrap()
            .push(format!("begin pact={}", done.pact()));
        if done.pact() {
            return Ok(ReadStart::Synchronous);
        }
        *self.queued.lock().unwrap() = Some((done.device_done(), done.priority()));
        Ok(ReadStart::Pending)
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads += 1.0;
        self.log
            .lock()
            .unwrap()
            .push(format!("read {}", self.reads));
        record.set_val(EpicsValue::Double(self.reads))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Queues"
    }
}

struct Sink(Log);

impl DeviceSupport for Sink {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.0.lock().unwrap().push("flnk".into());
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Sink"
    }
}

#[epics_macros_rs::epics_test]
async fn two_completions_queued_back_to_back_run_two_passes() {
    let log: Log = Arc::default();
    let queued: Queued = Arc::default();
    let (l, q) = (Arc::clone(&log), Arc::clone(&queued));
    let devices = [
        ("ai", declared_rows::rows(&["Queues"], DbLinkType::Constant)),
        ("ao", declared_rows::rows(&["Sink"], DbLinkType::Constant)),
    ];
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            match ctx.dtyp {
                "Queues" => Some(Box::new(Queues {
                    log: Arc::clone(&l),
                    queued: Arc::clone(&q),
                    reads: 0.0,
                }) as Box<dyn DeviceSupport>),
                "Sink" => Some(Box::new(Sink(Arc::clone(&l))) as Box<dyn DeviceSupport>),
                _ => None,
            }
        })
        .db_string(
            r#"
            record(ai, "AI") { field(DTYP, "Queues") field(FLNK, "SINK") }
            record(ao, "SINK") { field(DTYP, "Sink") }
            "#,
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
        done.reprocess(band).unwrap();
    })
    .await
    .unwrap();

    let want = [
        "begin pact=false",
        "begin pact=true",
        "read 1",
        "flnk",
        "begin pact=true",
        "read 2",
        "flnk",
    ];
    for _ in 0..400 {
        if log.lock().unwrap().len() >= want.len() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    assert_eq!(*log.lock().unwrap(), want);
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(2.0));
}
