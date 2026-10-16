//! C `db_post_events` copies the field into the event at the call: two posts
//! of one field in a cycle deliver both values, in order, whatever the field
//! holds when the drain runs.

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, FieldDesc, FieldIx, ProcessOutcome, Record, declared_field_ix,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

static FIELDS: &[FieldDesc] = &[FieldDesc::new("F0", DbFieldType::Double, false)];
const F0: FieldIx = declared_field_ix(FIELDS, "F0");

/// `process()` posts F0 at 1 then at 2 and leaves it at 3.
#[derive(Default)]
struct StoredPostRecord {
    f0: f64,
    owed: Vec<f64>,
}

impl Record for StoredPostRecord {
    fn record_type(&self) -> &'static str {
        "stored_post_test"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::HAND_POSTED
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        for v in [1.0, 2.0] {
            self.f0 = v;
            self.owed.push(v);
        }
        self.f0 = 3.0;
        Ok(ProcessOutcome::complete())
    }
    fn take_cycle_posted_value(&mut self) -> Option<(FieldIx, EventMask, EpicsValue)> {
        (!self.owed.is_empty()).then(|| {
            (
                F0,
                EventMask::VALUE,
                EpicsValue::Double(self.owed.remove(0)),
            )
        })
    }
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["F0"]
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "F0").then_some(EpicsValue::Double(self.f0))
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("stored_post_test", || Box::new(StoredPostRecord::default()))
        .db_string(r#"record(stored_post_test, "T") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

#[epics_macros_rs::epics_test]
async fn two_stored_posts_of_one_field_deliver_both_values_in_order() {
    let db = ioc().await;
    let rec = db.get_record("T").unwrap();
    let mut rx = rec
        .write()
        .add_subscriber("F0", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    while rx.try_recv().is_ok() {}
    db.process_record_with_links("T", &mut ProcStack::new())
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Ok(e) = rx.try_recv() {
        seen.push(e.delivered_value(&rec.read(), "F0").unwrap());
    }
    assert_eq!(
        seen,
        vec![EpicsValue::Double(1.0), EpicsValue::Double(2.0)],
        "each post carries the value it was made with, not the 3 left behind"
    );
}
