//! A `CycleHooks::STAMP_REQUEST` type is stamped only on the passes whose
//! `process()` asked, as C mca calls `recGblGetTimeStamp` only at
//! `mcaRecord.c:762` and `:811`. A pass that does not ask keeps TIME and reads
//! no TSEL; a pass that asks and then waits with PACT is stamped before the
//! `return(0)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{CycleHooks, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, WallTime};

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// Whether the next `process()` asks for a stamp, and whether it then waits.
static ASK: AtomicBool = AtomicBool::new(false);
static WAIT: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct OwnStamp {
    asked: bool,
}

impl Record for OwnStamp {
    fn record_type(&self) -> &'static str {
        "own_stamp"
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
        (name == "VAL").then_some(EpicsValue::Double(0.0))
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
}

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("own_stamp", || Box::new(OwnStamp::default()))
        .db_string(
            r#"
            record(own_stamp, "P") {}
            record(own_stamp, "Q") { field(TSE, "-1") field(TSEL, "SRC") }
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

#[epics_macros_rs::epics_test]
async fn only_a_pass_that_asks_moves_time() {
    let db = ioc().await;
    let before = time(&db, "P");
    ASK.store(false, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    db.process_record("P").await.unwrap();
    assert_eq!(time(&db, "P"), before, "no pass asked");

    ASK.store(true, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    let stamped = time(&db, "P");
    assert_ne!(stamped, before, "the asking pass stamps");

    ASK.store(false, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(2));
    db.process_record("P").await.unwrap();
    assert_eq!(time(&db, "P"), stamped, "the request was consumed");
}

#[epics_macros_rs::epics_test]
async fn tsel_is_read_only_on_a_pass_that_asks() {
    let db = ioc().await;
    let tse = || db.get_record("Q").unwrap().read().common.tse();
    ASK.store(false, Ordering::SeqCst);
    db.process_record("Q").await.unwrap();
    db.process_record("Q").await.unwrap();
    assert_eq!(tse(), -1, "no TSEL read loaded SRC");

    ASK.store(true, Ordering::SeqCst);
    db.process_record("Q").await.unwrap();
    assert_eq!(tse(), 0, "the asking pass loads TSE from TSEL");
}

#[epics_macros_rs::epics_test]
async fn a_pass_that_asks_and_waits_is_stamped_before_it_returns() {
    let db = ioc().await;
    let before = time(&db, "P");
    ASK.store(true, Ordering::SeqCst);
    WAIT.store(true, Ordering::SeqCst);
    db.process_record("P").await.unwrap();
    assert!(db.get_record("P").unwrap().read().is_processing());
    assert_ne!(time(&db, "P"), before);
}
