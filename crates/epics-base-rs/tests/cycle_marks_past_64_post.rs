//! A take of process marks addresses more than 64 table entries: C's scaler
//! posts PR1, TP, FREQ, T and S1..S64 from one count-start cycle
//! (`scalerRecord.c:425-430`, `:582`, `:588`), 68 posts in one take.

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const N: usize = 69;

static NAMES: [&str; N] = [
    "F00", "F01", "F02", "F03", "F04", "F05", "F06", "F07", "F08", "F09", "F10", "F11", "F12",
    "F13", "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24", "F25",
    "F26", "F27", "F28", "F29", "F30", "F31", "F32", "F33", "F34", "F35", "F36", "F37", "F38",
    "F39", "F40", "F41", "F42", "F43", "F44", "F45", "F46", "F47", "F48", "F49", "F50", "F51",
    "F52", "F53", "F54", "F55", "F56", "F57", "F58", "F59", "F60", "F61", "F62", "F63", "F64",
    "F65", "F66", "F67", "F68",
];

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("F00", DbFieldType::Double, false),
    FieldDesc::new("F01", DbFieldType::Double, false),
    FieldDesc::new("F02", DbFieldType::Double, false),
    FieldDesc::new("F03", DbFieldType::Double, false),
    FieldDesc::new("F04", DbFieldType::Double, false),
    FieldDesc::new("F05", DbFieldType::Double, false),
    FieldDesc::new("F06", DbFieldType::Double, false),
    FieldDesc::new("F07", DbFieldType::Double, false),
    FieldDesc::new("F08", DbFieldType::Double, false),
    FieldDesc::new("F09", DbFieldType::Double, false),
    FieldDesc::new("F10", DbFieldType::Double, false),
    FieldDesc::new("F11", DbFieldType::Double, false),
    FieldDesc::new("F12", DbFieldType::Double, false),
    FieldDesc::new("F13", DbFieldType::Double, false),
    FieldDesc::new("F14", DbFieldType::Double, false),
    FieldDesc::new("F15", DbFieldType::Double, false),
    FieldDesc::new("F16", DbFieldType::Double, false),
    FieldDesc::new("F17", DbFieldType::Double, false),
    FieldDesc::new("F18", DbFieldType::Double, false),
    FieldDesc::new("F19", DbFieldType::Double, false),
    FieldDesc::new("F20", DbFieldType::Double, false),
    FieldDesc::new("F21", DbFieldType::Double, false),
    FieldDesc::new("F22", DbFieldType::Double, false),
    FieldDesc::new("F23", DbFieldType::Double, false),
    FieldDesc::new("F24", DbFieldType::Double, false),
    FieldDesc::new("F25", DbFieldType::Double, false),
    FieldDesc::new("F26", DbFieldType::Double, false),
    FieldDesc::new("F27", DbFieldType::Double, false),
    FieldDesc::new("F28", DbFieldType::Double, false),
    FieldDesc::new("F29", DbFieldType::Double, false),
    FieldDesc::new("F30", DbFieldType::Double, false),
    FieldDesc::new("F31", DbFieldType::Double, false),
    FieldDesc::new("F32", DbFieldType::Double, false),
    FieldDesc::new("F33", DbFieldType::Double, false),
    FieldDesc::new("F34", DbFieldType::Double, false),
    FieldDesc::new("F35", DbFieldType::Double, false),
    FieldDesc::new("F36", DbFieldType::Double, false),
    FieldDesc::new("F37", DbFieldType::Double, false),
    FieldDesc::new("F38", DbFieldType::Double, false),
    FieldDesc::new("F39", DbFieldType::Double, false),
    FieldDesc::new("F40", DbFieldType::Double, false),
    FieldDesc::new("F41", DbFieldType::Double, false),
    FieldDesc::new("F42", DbFieldType::Double, false),
    FieldDesc::new("F43", DbFieldType::Double, false),
    FieldDesc::new("F44", DbFieldType::Double, false),
    FieldDesc::new("F45", DbFieldType::Double, false),
    FieldDesc::new("F46", DbFieldType::Double, false),
    FieldDesc::new("F47", DbFieldType::Double, false),
    FieldDesc::new("F48", DbFieldType::Double, false),
    FieldDesc::new("F49", DbFieldType::Double, false),
    FieldDesc::new("F50", DbFieldType::Double, false),
    FieldDesc::new("F51", DbFieldType::Double, false),
    FieldDesc::new("F52", DbFieldType::Double, false),
    FieldDesc::new("F53", DbFieldType::Double, false),
    FieldDesc::new("F54", DbFieldType::Double, false),
    FieldDesc::new("F55", DbFieldType::Double, false),
    FieldDesc::new("F56", DbFieldType::Double, false),
    FieldDesc::new("F57", DbFieldType::Double, false),
    FieldDesc::new("F58", DbFieldType::Double, false),
    FieldDesc::new("F59", DbFieldType::Double, false),
    FieldDesc::new("F60", DbFieldType::Double, false),
    FieldDesc::new("F61", DbFieldType::Double, false),
    FieldDesc::new("F62", DbFieldType::Double, false),
    FieldDesc::new("F63", DbFieldType::Double, false),
    FieldDesc::new("F64", DbFieldType::Double, false),
    FieldDesc::new("F65", DbFieldType::Double, false),
    FieldDesc::new("F66", DbFieldType::Double, false),
    FieldDesc::new("F67", DbFieldType::Double, false),
    FieldDesc::new("F68", DbFieldType::Double, false),
];

/// Each `process()` marks every one of its N fields.
#[derive(Default)]
struct WideMarkRecord {
    marked: bool,
}

impl Record for WideMarkRecord {
    fn record_type(&self) -> &'static str {
        "wide_mark_test"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::PROCESS_POSTED
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.marked = true;
        Ok(ProcessOutcome::complete_no_emit())
    }
    fn take_process_posted_fields(&mut self) -> CycleMarks {
        static POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); N] =
            epics_base_rs::server::record::uniform_post_table(FIELDS, &NAMES, CyclePostMask::Value);
        let all = (1u128 << N) - 1;
        CycleMarks::of(
            &POSTS,
            if std::mem::take(&mut self.marked) {
                all
            } else {
                0
            },
        )
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
        .register_record_type("wide_mark_test", || Box::new(WideMarkRecord::default()))
        .db_string(r#"record(wide_mark_test, "W") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let readers = {
        let rec = db.get_record("W").unwrap();
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
async fn a_cycle_marking_69_fields_posts_69() {
    let (db, mut readers) = ioc().await;
    readers.iter_mut().for_each(|rx| {
        drain(rx);
    });
    db.process_record_with_links("W", &mut ProcStack::new())
        .await
        .unwrap();
    let posts: Vec<usize> = readers.iter_mut().map(drain).collect();
    assert_eq!(
        posts,
        vec![1; N],
        "one post per marked field, past bit 64 too"
    );
}
