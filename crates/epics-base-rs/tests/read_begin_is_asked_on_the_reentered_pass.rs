//! `read_begin` is asked on the pass a completion re-entered, with PACT set, so
//! a device may queue a second round trip from there: C mca's status pass
//! sends `mcaData`, sets `pact` again and waits for a second callback
//! (devMcaAsyn.c:284-289, mcaRecord.c:770-775). The re-entered pass used to
//! skip `read_begin`, so its `read()` could only block under the hold.

mod declared_rows;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, Record};
use epics_base_rs::types::EpicsValue;

type Pending = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

/// Queues a read on each of its first two `read_begin`s; the third reads.
struct TwoTrips {
    pending: Pending,
    begins: Arc<Mutex<Vec<bool>>>,
    reads: Arc<Mutex<usize>>,
}

impl DeviceSupport for TwoTrips {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        let mut begins = self.begins.lock().unwrap();
        begins.push(done.pact());
        if begins.len() > 2 {
            return Ok(ReadStart::Synchronous);
        }
        *self.pending.lock().unwrap() = Some((done.device_done(), done.priority()));
        Ok(ReadStart::Pending)
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        *self.reads.lock().unwrap() += 1;
        record.set_val(EpicsValue::Double(7.0))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "TwoTrips"
    }
}

struct Sink(Arc<Mutex<usize>>);

impl DeviceSupport for Sink {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Sink"
    }
}

async fn complete(db: &PvDatabase, pending: &Pending) {
    let (done, band) = pending.lock().unwrap().take().expect("a read was queued");
    std::thread::spawn(move || done.reprocess(band).unwrap())
        .join()
        .unwrap();
    // The re-entered pass either queues again or ends the cycle.
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if pending.lock().unwrap().is_some() || !db.get_record("AI").unwrap().read().is_processing()
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("the completion never re-entered AI");
}

#[epics_macros_rs::epics_test]
async fn a_reentered_pass_may_queue_a_second_read() {
    let pending: Pending = Arc::default();
    let begins = Arc::new(Mutex::new(Vec::new()));
    let reads = Arc::new(Mutex::new(0));
    let flnks = Arc::new(Mutex::new(0));
    let (p, b, r, f) = (
        Arc::clone(&pending),
        Arc::clone(&begins),
        Arc::clone(&reads),
        Arc::clone(&flnks),
    );
    let devices = [
        (
            "ai",
            declared_rows::rows(&["TwoTrips"], DbLinkType::Constant),
        ),
        ("ao", declared_rows::rows(&["Sink"], DbLinkType::Constant)),
    ];
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            match ctx.dtyp {
                "TwoTrips" => Some(Box::new(TwoTrips {
                    pending: Arc::clone(&p),
                    begins: Arc::clone(&b),
                    reads: Arc::clone(&r),
                }) as Box<dyn DeviceSupport>),
                "Sink" => Some(Box::new(Sink(Arc::clone(&f))) as Box<dyn DeviceSupport>),
                _ => None,
            }
        })
        .db_string(
            r#"
            record(ai, "AI") { field(DTYP, "TwoTrips") field(FLNK, "SINK") }
            record(ao, "SINK") { field(DTYP, "Sink") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    db.process_record("AI").await.unwrap();
    assert!(db.get_record("AI").unwrap().read().is_processing());

    // The first completion's pass queues the second read and holds PACT.
    complete(&db, &pending).await;
    assert!(db.get_record("AI").unwrap().read().is_processing());
    assert_eq!(*reads.lock().unwrap(), 0);
    assert_eq!(*flnks.lock().unwrap(), 0);

    complete(&db, &pending).await;
    assert!(!db.get_record("AI").unwrap().read().is_processing());
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(7.0));
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    assert_eq!(*begins.lock().unwrap(), [false, true, true]);
    assert_eq!(*reads.lock().unwrap(), 1);
    assert_eq!(*flnks.lock().unwrap(), 1, "one cycle, one FLNK");
}
