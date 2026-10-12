//! `AsyncDbHandle::with_record` is C's `dbScanLock`, the record's own code,
//! `dbScanUnlock` from a thread that holds no record (asynRecord's
//! `exceptCallback`, asynRecord.c:911-916): the closure runs while the
//! record's lock set is held, and a post it marks goes out under that hold.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{CycleMarks, CyclePostMask, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

static POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 1] =
    epics_base_rs::server::record::declared_post_table(FIELDS, [("VAL", CyclePostMask::ValueLog)]);

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

#[derive(Default)]
struct Probe {
    val: f64,
    marked: bool,
}

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        "with_record_probe"
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
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(&POSTS, u128::from(std::mem::take(&mut self.marked)))
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Appends each VAL event it passes.
struct Seen(Arc<Mutex<Vec<f64>>>);

impl SubscriptionFilter for Seen {
    fn name(&self) -> &'static str {
        "seen"
    }
    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        if let Some(EpicsValue::Double(v)) = event.event.value().as_value() {
            self.0.lock().unwrap().push(*v);
        }
        Some(event)
    }
}

#[epics_macros_rs::epics_test]
async fn the_closure_runs_under_the_hold_and_its_marks_post() {
    let (db, _) = IocBuilder::new()
        .register_record_type("with_record_probe", || Box::new(Probe::default()))
        .db_string("record(with_record_probe, \"P\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("P").unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _reader = {
        let mut inst = cell.write();
        let reader = inst
            .add_subscriber("VAL", 0, DbFieldType::Double, EventMask::VALUE.bits())
            .unwrap();
        assert!(inst.attach_filter_to_last_subscriber("VAL", Arc::new(Seen(seen.clone()))));
        reader
    };

    let handle = db.async_handle();
    let locked = Arc::new(AtomicBool::new(false));
    let mut other = None;
    handle.with_record("P", |r| {
        let probe = r.as_any_mut().unwrap().downcast_mut::<Probe>().unwrap();
        probe.val = 7.0;
        probe.marked = true;
        let (db, flag) = (db.clone(), locked.clone());
        other = Some(std::thread::spawn(move || {
            let _gate = db.lock_record("P");
            flag.store(true, Ordering::Release);
        }));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !locked.load(Ordering::Acquire),
            "another thread took the record's lock set inside the closure"
        );
    });
    other.unwrap().join().unwrap();
    assert!(locked.load(Ordering::Acquire), "the hold was released");
    assert_eq!(*seen.lock().unwrap(), [7.0]);

    handle.with_record("NOSUCH", |_| panic!("no record, no closure"));
}
