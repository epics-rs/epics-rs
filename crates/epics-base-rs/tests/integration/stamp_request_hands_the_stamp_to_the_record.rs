//! A `CycleHooks::STAMP_REQUEST` pass that asked is handed the TIME its stamp
//! point stored, TSEL applied, before the pass posts: C mca sets RTIM and STIM
//! from the fresh `pmca->time` right after `recGblGetTimeStamp`
//! (mcaRecord.c:762-764, :811-814) and `monitor()` posts them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{CycleHooks, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, WallTime};

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

static ASK: AtomicBool = AtomicBool::new(false);
static WAIT: AtomicBool = AtomicBool::new(false);
/// Every TIME `time_stamped` was handed, in order.
static HANDED: Mutex<Vec<WallTime>> = Mutex::new(Vec::new());

fn secs(t: WallTime) -> f64 {
    t.unix_secs() as f64 + f64::from(t.subsec_nanos()) * 1e-9
}

/// VAL is the seconds of the last stamp it was handed, as mca's RTIM.
#[derive(Default)]
struct Handed {
    asked: bool,
    val: f64,
}

impl Record for Handed {
    fn record_type(&self) -> &'static str {
        "handed"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.asked = ASK.load(Ordering::SeqCst);
        Ok(if WAIT.load(Ordering::SeqCst) {
            ProcessOutcome::async_pending()
        } else {
            ProcessOutcome::complete()
        })
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
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::ALL.union(CycleHooks::STAMP_REQUEST)
    }
    fn take_time_stamp_request(&mut self) -> bool {
        std::mem::take(&mut self.asked)
    }
    fn time_stamped(&mut self, time: WallTime) {
        HANDED.lock().unwrap().push(time);
        self.val = secs(time);
    }
}

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("handed", || Box::new(Handed::default()))
        .db_string(
            r#"
            record(handed, "P") {}
            record(handed, "Q") { field(TSEL, "SRC.TIME") }
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

#[epics_macros_rs::epics_test]
async fn the_asking_pass_is_handed_its_stamp_before_it_posts() {
    let db = ioc().await;
    let mut val_rx = db
        .get_record("P")
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    while val_rx.try_recv().is_ok() {}
    handed();

    ASK.store(false, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    assert_eq!(handed(), [], "a pass that did not ask is handed nothing");
    while val_rx.try_recv().is_ok() {}

    ASK.store(true, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    let stamped = time(&db, "P");
    assert_eq!(handed(), [stamped]);
    let mut posted = Vec::new();
    while let Ok(e) = val_rx.try_recv() {
        posted.push(e.value().as_value().cloned().unwrap());
    }
    assert_eq!(posted, [EpicsValue::Double(secs(stamped))]);
}

#[epics_macros_rs::epics_test]
async fn a_time_tsel_is_applied_before_the_record_is_handed_it() {
    let db = ioc().await;
    db.process_record("SRC").await.unwrap();
    let src = time(&db, "SRC");
    std::thread::sleep(std::time::Duration::from_millis(2));
    handed();
    ASK.store(true, Ordering::SeqCst);
    db.process_record("Q").await.unwrap();
    assert_eq!(time(&db, "Q"), src);
    assert_eq!(handed(), [src]);
}

#[epics_macros_rs::epics_test]
async fn a_pass_that_asks_and_waits_is_handed_its_stamp() {
    let db = ioc().await;
    handed();
    ASK.store(true, Ordering::SeqCst);
    WAIT.store(true, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    assert!(db.get_record("P").unwrap().read().is_processing());
    assert_eq!(handed(), [time(&db, "P")]);
}
