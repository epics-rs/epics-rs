//! A hand-posted cycle owing more marks than one post table holds — sscan
//! posts 70 DnDA and 70 DnCA arrays and more at the end of a scan — drains
//! every take the record says remains, in the same cycle.

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

static NAMES: [&str; 4] = ["F0", "F1", "F2", "F3"];
static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("F0", DbFieldType::Double, false),
    FieldDesc::new("F1", DbFieldType::Double, false),
    FieldDesc::new("F2", DbFieldType::Double, false),
    FieldDesc::new("F3", DbFieldType::Double, false),
];
static FIRST: [(FieldIx, CyclePostMask); 2] =
    uniform_post_table(FIELDS, &["F0", "F1"], CyclePostMask::Value);
static SECOND: [(FieldIx, CyclePostMask); 2] =
    uniform_post_table(FIELDS, &["F2", "F3"], CyclePostMask::Value);

/// `process()` owes all four posts, two per table.
#[derive(Default)]
struct TwoTakeRecord {
    owed: u8,
}

impl Record for TwoTakeRecord {
    fn record_type(&self) -> &'static str {
        "two_take_test"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::HAND_POSTED
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.owed = 2;
        Ok(ProcessOutcome::complete())
    }
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        let table: &'static [(FieldIx, CyclePostMask)] = match self.owed {
            2 => &FIRST,
            1 => &SECOND,
            _ => return CycleMarks::NONE,
        };
        self.owed -= 1;
        CycleMarks::of(table, 0b11)
    }
    fn cycle_posts_remain(&self) -> bool {
        self.owed != 0
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

async fn ioc() -> (std::sync::Arc<PvDatabase>, Vec<EventReader>) {
    let (db, _) = IocBuilder::new()
        .register_record_type("two_take_test", || Box::new(TwoTakeRecord::default()))
        .db_string(r#"record(two_take_test, "T") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let readers = {
        let rec = db.get_record("T").unwrap();
        let mut inst = rec.write();
        NAMES
            .iter()
            .zip(1u32..)
            .map(|(name, id)| {
                inst.add_subscriber(name, id, DbFieldType::Double, EventMask::VALUE.bits())
                    .unwrap()
            })
            .collect()
    };
    (db, readers)
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

#[epics_macros_rs::epics_test]
async fn every_remaining_take_posts_in_the_cycle() {
    let (db, mut readers) = ioc().await;
    readers.iter_mut().for_each(|rx| {
        drain(rx);
    });
    db.process_record_with_links("T", &mut ProcStack::new())
        .await
        .unwrap();
    let posts: Vec<usize> = readers.iter_mut().map(drain).collect();
    assert_eq!(posts, vec![1; 4], "both takes post in the cycle");
}
