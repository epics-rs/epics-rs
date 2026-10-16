//! A hand-posted `process()` posts what it marked in that cycle whichever way
//! the cycle leaves — C's sscan posts by hand inside `process()` before it
//! returns, early or with an error (`sscanRecord.c:932-1257`, :1040-1050).

use std::collections::HashMap;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, FieldIx, ProcessAction, ProcessOutcome,
    Record, ReturnPact, uniform_post_table,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

static NAMES: [&str; 1] = ["F0"];
static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("F0", DbFieldType::Double, false),
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("COUT", DbFieldType::String, false)
    },
];
static TABLE: [(FieldIx, CyclePostMask); 1] =
    uniform_post_table(FIELDS, &["F0"], CyclePostMask::Value);

/// How `process()` leaves, one per exit of the cycle.
#[derive(Clone, Copy)]
enum Exit {
    Complete,
    NoEmit,
    AsyncPending,
    Err,
    FailedWriteDbLinkOrReturn,
}

impl Exit {
    const ALL: [Exit; 5] = [
        Exit::Complete,
        Exit::NoEmit,
        Exit::AsyncPending,
        Exit::Err,
        Exit::FailedWriteDbLinkOrReturn,
    ];

    fn record_type(self) -> &'static str {
        match self {
            Exit::Complete => "hp_complete",
            Exit::NoEmit => "hp_no_emit",
            Exit::AsyncPending => "hp_pending",
            Exit::Err => "hp_err",
            Exit::FailedWriteDbLinkOrReturn => "hp_link",
        }
    }
}

struct Probe {
    exit: Exit,
    marked: bool,
    cout: String,
}

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        self.exit.record_type()
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::HAND_POSTED
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.marked = true;
        Ok(match self.exit {
            Exit::Complete => ProcessOutcome::complete(),
            Exit::NoEmit => ProcessOutcome::complete_no_emit(),
            Exit::AsyncPending => ProcessOutcome::async_pending(),
            Exit::Err => return Err(CaError::FieldNotFound("probe".into())),
            Exit::FailedWriteDbLinkOrReturn => {
                ProcessOutcome::complete_with(vec![ProcessAction::WriteDbLinkOrReturn {
                    link_field: "COUT",
                    value: EpicsValue::Double(1.0),
                    pact: ReturnPact::Held,
                }])
            }
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
        match name {
            "F0" => Some(EpicsValue::Double(0.0)),
            "COUT" => Some(EpicsValue::String(self.cout.clone().into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "COUT" => self.cout = value.to_string(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc(exit: Exit) -> (std::sync::Arc<PvDatabase>, EventReader) {
    let mut builder = IocBuilder::new();
    for e in Exit::ALL {
        builder = builder.register_record_type(e.record_type(), move || {
            Box::new(Probe {
                exit: e,
                marked: false,
                cout: String::new(),
            })
        });
    }
    let ty = exit.record_type();
    let (db, _) = builder
        .db_string(
            &format!(r#"record({ty}, "R") {{ field(COUT, "NOSUCH.VAL") }}"#),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let rx = db
        .get_record("R")
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

async fn assert_posts_once(exit: Exit) {
    let (db, mut rx) = ioc(exit).await;
    drain(&mut rx);
    let r = db
        .process_record_with_links("R", &mut ProcStack::new())
        .await;
    let pact = db.get_pv("R.PACT").unwrap().to_f64().unwrap();
    assert_eq!(drain(&mut rx), 1, "process returned {r:?}, PACT {pact}");
}

#[epics_macros_rs::epics_test]
async fn a_complete_cycle_posts_its_marks_once() {
    assert_posts_once(Exit::Complete).await;
}

#[epics_macros_rs::epics_test]
async fn a_no_emit_return_posts_its_marks() {
    assert_posts_once(Exit::NoEmit).await;
}

#[epics_macros_rs::epics_test]
async fn an_async_pending_return_posts_its_marks() {
    assert_posts_once(Exit::AsyncPending).await;
}

#[epics_macros_rs::epics_test]
async fn an_err_return_posts_its_marks() {
    assert_posts_once(Exit::Err).await;
}

#[epics_macros_rs::epics_test]
async fn a_failed_write_db_link_or_return_posts_its_marks() {
    assert_posts_once(Exit::FailedWriteDbLinkOrReturn).await;
}
