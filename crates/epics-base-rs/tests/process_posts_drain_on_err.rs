//! A `PROCESS_POSTED` record's marks are drained in the cycle that set them,
//! whether `process()` returned `Ok` or `Err`: C's `db_post_events` runs inside
//! `process()`, before it returns its status, so a failing cycle has already
//! posted and leaves nothing for the next one.
//!
//! The context push and the drain share one hook test, so each of the four
//! hook combinations gets exactly its own: a `CONTEXT`-only type (transform's
//! shape) is pushed and never drained, a type with neither is left alone.

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, ProcessContext, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// `process()` marks X when MARK is set, then fails when FAIL is set. PUSH and
/// DRAIN count the framework's `set_process_context` and
/// `take_process_posted_fields` calls.
struct MarkRecord {
    hooks: CycleHooks,
    mark: bool,
    fail: bool,
    marked: bool,
    pushes: i32,
    drains: i32,
}

impl MarkRecord {
    fn new(hooks: CycleHooks) -> Self {
        Self {
            hooks,
            mark: false,
            fail: false,
            marked: false,
            pushes: 0,
            drains: 0,
        }
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("X", DbFieldType::Double, false),
    FieldDesc::new("MARK", DbFieldType::Short, false),
    FieldDesc::new("FAIL", DbFieldType::Short, false),
    FieldDesc::new("PUSH", DbFieldType::Long, true),
    FieldDesc::new("DRAIN", DbFieldType::Long, true),
];

impl Record for MarkRecord {
    fn record_type(&self) -> &'static str {
        "mark_test"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        self.hooks
    }
    fn set_process_context(&mut self, _: &ProcessContext) {
        self.pushes += 1;
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Accumulates, so a mark left undrained would post next cycle.
        self.marked |= self.mark;
        if self.fail {
            return Err(CaError::InvalidValue("FAIL".into()));
        }
        Ok(ProcessOutcome::complete())
    }
    fn take_process_posted_fields(&mut self) -> CycleMarks {
        static X_POST: [(&str, CyclePostMask); 1] = [("X", CyclePostMask::Value)];
        self.drains += 1;
        CycleMarks::of(&X_POST, u64::from(std::mem::take(&mut self.marked)))
    }
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["X"]
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "X" => Some(EpicsValue::Double(0.0)),
            "MARK" => Some(EpicsValue::Short(self.mark.into())),
            "FAIL" => Some(EpicsValue::Short(self.fail.into())),
            "PUSH" => Some(EpicsValue::Long(self.pushes)),
            "DRAIN" => Some(EpicsValue::Long(self.drains)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let on = value.to_f64().unwrap_or(0.0) != 0.0;
        match name {
            "MARK" => self.mark = on,
            "FAIL" => self.fail = on,
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc() -> (std::sync::Arc<PvDatabase>, EventReader) {
    ioc_with(CycleHooks::PROCESS_POSTED).await
}

async fn ioc_with(hooks: CycleHooks) -> (std::sync::Arc<PvDatabase>, EventReader) {
    let (db, _) = IocBuilder::new()
        .register_record_type("mark_test", move || Box::new(MarkRecord::new(hooks)))
        .db_string(r#"record(mark_test, "M") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let rx = db
        .get_record("M")
        .unwrap()
        .write()
        .add_subscriber("X", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    (db, rx)
}

/// Process M once with MARK and FAIL as given; the X events that cycle posted.
async fn cycle(db: &PvDatabase, rx: &mut EventReader, mark: bool, fail: bool) -> usize {
    db.put_record_field_from_ca_no_notify("M", "MARK", EpicsValue::Short(mark.into()))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("M", "FAIL", EpicsValue::Short(fail.into()))
        .await
        .unwrap();
    while rx.try_recv().is_ok() {}
    let _ = db
        .process_record_with_links("M", &mut ProcStack::new())
        .await;
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

#[epics_macros_rs::epics_test]
async fn a_failed_process_posts_its_marks_and_carries_none() {
    let (db, mut rx) = ioc().await;
    assert_eq!(
        cycle(&db, &mut rx, true, true).await,
        1,
        "Err cycle posts X"
    );
    assert_eq!(
        cycle(&db, &mut rx, false, false).await,
        0,
        "nothing carried"
    );
}

#[epics_macros_rs::epics_test]
async fn a_completed_process_posts_its_marks_and_carries_none() {
    let (db, mut rx) = ioc().await;
    assert_eq!(
        cycle(&db, &mut rx, true, false).await,
        1,
        "Ok cycle posts X"
    );
    assert_eq!(
        cycle(&db, &mut rx, false, false).await,
        0,
        "nothing carried"
    );
}

#[epics_macros_rs::epics_test]
async fn an_unmarked_failed_process_posts_nothing() {
    let (db, mut rx) = ioc().await;
    assert_eq!(cycle(&db, &mut rx, false, true).await, 0);
}

fn count(db: &PvDatabase, field: &str) -> i32 {
    match db.get_record("M").unwrap().read().record.get_field(field) {
        Some(EpicsValue::Long(n)) => n,
        v => panic!("{field} {v:?}"),
    }
}

/// Two marked cycles, one failing: (X posts, pushes, drains). A `CONTEXT`
/// type is pushed twice a cycle, by the pre-input stage and again by
/// `process_and_drain` after the input links folded their alarms in.
async fn two_marked_cycles(hooks: CycleHooks) -> (usize, i32, i32) {
    let (db, mut rx) = ioc_with(hooks).await;
    let posts = cycle(&db, &mut rx, true, false).await + cycle(&db, &mut rx, true, true).await;
    (posts, count(&db, "PUSH"), count(&db, "DRAIN"))
}

#[epics_macros_rs::epics_test]
async fn each_hook_combination_gets_its_push_and_its_drain_only() {
    use CycleHooks as H;
    assert_eq!(two_marked_cycles(H::PROCESS_POSTED).await, (2, 0, 2));
    assert_eq!(
        two_marked_cycles(H::CONTEXT).await,
        (0, 4, 0),
        "transform shape: pushed, a stray mark never drained or posted"
    );
    assert_eq!(
        two_marked_cycles(H::CONTEXT.union(H::PROCESS_POSTED)).await,
        (2, 4, 2)
    );
    assert_eq!(two_marked_cycles(H::NONE).await, (0, 0, 0));
}
