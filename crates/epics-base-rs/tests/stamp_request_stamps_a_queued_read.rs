//! A `CycleHooks::STAMP_REQUEST` pass whose device queues its read stamps
//! before it waits, when it asked: C mca's `recGblGetTimeStamp` and RTIM
//! (`mcaRecord.c:762-764`) run ahead of the `return(0)` that waits for the
//! data callback on a CANBLOCK port (`:778`).

mod declared_rows;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{CycleHooks, DbLinkType, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, WallTime};

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

static ASK: AtomicBool = AtomicBool::new(false);
static HANDED: Mutex<Vec<WallTime>> = Mutex::new(Vec::new());

#[derive(Default)]
struct Queued {
    val: f64,
}

impl Record for Queued {
    fn record_type(&self) -> &'static str {
        "queued"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Double(self.val))
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        self.val = value.to_f64().unwrap_or(0.0);
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::ALL.union(CycleHooks::STAMP_REQUEST)
    }
    fn take_time_stamp_request(&mut self) -> bool {
        ASK.swap(false, Ordering::SeqCst)
    }
    fn time_stamped(&mut self, time: WallTime) {
        HANDED.lock().unwrap().push(time);
    }
}

/// Queues every read it is asked to begin.
struct Queues;

impl DeviceSupport for Queues {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        _done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        Ok(ReadStart::Pending)
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Queues"
    }
}

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("queued", || Box::new(Queued::default()))
        .register_dynamic_device_support(
            &[(
                "queued",
                declared_rows::rows(&["Queues"], DbLinkType::Constant),
            )],
            |ctx: &DeviceSupportContext| {
                (ctx.dtyp == "Queues").then(|| Box::new(Queues) as Box<dyn DeviceSupport>)
            },
        )
        .db_string(
            r#"
            record(queued, "P") { field(DTYP, "Queues") }
            record(queued, "Q") { field(DTYP, "Queues") field(TSEL, "SRC.TIME") }
            record(ao, "SRC") { field(VAL, "0") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn time(db: &PvDatabase, name: &str) -> WallTime {
    db.get_record(name).unwrap().read().common.time
}

fn handed() -> Vec<WallTime> {
    std::mem::take(&mut *HANDED.lock().unwrap())
}

fn pact(db: &PvDatabase, name: &str) -> bool {
    db.get_record(name).unwrap().read().is_processing()
}

#[epics_macros_rs::epics_test]
async fn a_queued_read_that_asked_is_stamped_before_it_waits() {
    let db = ioc().await;
    handed();
    let before = time(&db, "P");
    ASK.store(true, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    assert!(pact(&db, "P"), "P waits for its queued read");
    let stamped = time(&db, "P");
    assert_ne!(stamped, before, "TIME stamped ahead of the wait");
    assert_eq!(handed(), [stamped]);
}

#[epics_macros_rs::epics_test]
async fn a_queued_read_that_did_not_ask_is_not_stamped() {
    let db = ioc().await;
    handed();
    let before = time(&db, "P");
    ASK.store(false, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    assert!(pact(&db, "P"));
    assert_eq!(time(&db, "P"), before);
    assert_eq!(handed(), []);
}

#[epics_macros_rs::epics_test]
async fn a_queued_read_takes_its_time_tsel() {
    let db = ioc().await;
    db.process_record("SRC").await.unwrap();
    let src = time(&db, "SRC");
    std::thread::sleep(std::time::Duration::from_millis(2));
    handed();
    ASK.store(true, Ordering::SeqCst);
    db.process_record("Q").await.unwrap();
    assert!(pact(&db, "Q"));
    assert_eq!(time(&db, "Q"), src);
    assert_eq!(handed(), [src]);
}
