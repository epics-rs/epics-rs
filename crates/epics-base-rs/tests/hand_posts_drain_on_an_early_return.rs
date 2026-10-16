//! A hand-posted `process()` that returns before `monitor()` — sscan's
//! early returns post the fields they changed by hand and skip
//! `recGblResetAlarms` and FLNK (`sscanRecord.c:932-1257`) — still posts
//! what it marked in that cycle, whether PACT stays clear or held.

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, FieldIx, ProcessOutcome, Record,
    uniform_post_table,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

static NAMES: [&str; 1] = ["F0"];
static FIELDS: &[FieldDesc] = &[FieldDesc::new("F0", DbFieldType::Double, false)];
static TABLE: [(FieldIx, CyclePostMask); 1] =
    uniform_post_table(FIELDS, &["F0"], CyclePostMask::Value);

struct EarlyReturnRecord {
    pending: bool,
    marked: bool,
}

impl Record for EarlyReturnRecord {
    fn record_type(&self) -> &'static str {
        if self.pending {
            "early_pending_test"
        } else {
            "early_no_emit_test"
        }
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::HAND_POSTED
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.marked = true;
        Ok(if self.pending {
            ProcessOutcome::async_pending()
        } else {
            ProcessOutcome::complete_no_emit()
        })
    }
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        if !std::mem::take(&mut self.marked) {
            return CycleMarks::NONE;
        }
        CycleMarks::of(&TABLE, 0b1)
    }
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &NAMES
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        NAMES.contains(&name).then_some(EpicsValue::Double(0.0))
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc(pending: bool) -> (std::sync::Arc<PvDatabase>, EventReader) {
    let (db, _) = IocBuilder::new()
        .register_record_type("early_no_emit_test", || {
            Box::new(EarlyReturnRecord {
                pending: false,
                marked: false,
            })
        })
        .register_record_type("early_pending_test", || {
            Box::new(EarlyReturnRecord {
                pending: true,
                marked: false,
            })
        })
        .db_string(
            r#"record(early_no_emit_test, "N") {}
record(early_pending_test, "P") {}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let rx = db
        .get_record(if pending { "P" } else { "N" })
        .unwrap()
        .write()
        .add_subscriber("F0", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    (db, rx)
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

async fn posts_in_cycle(pending: bool) -> usize {
    let (db, mut rx) = ioc(pending).await;
    drain(&mut rx);
    db.process_record_with_links(if pending { "P" } else { "N" }, &mut ProcStack::new())
        .await
        .unwrap();
    drain(&mut rx)
}

#[epics_macros_rs::epics_test]
async fn no_emit_return_posts_its_marks() {
    assert_eq!(posts_in_cycle(false).await, 1);
}

#[epics_macros_rs::epics_test]
async fn async_pending_return_posts_its_marks() {
    assert_eq!(posts_in_cycle(true).await, 1);
}
