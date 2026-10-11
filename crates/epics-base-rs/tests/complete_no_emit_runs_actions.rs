//! `CompleteNoEmit` is C's `process()` returning 0 before `recGblGetTimeStamp`
//! with PACT cleared — `scalerRecord.c:470-545` on a non-idle cycle: the
//! `dbPutLink`s and `db_post_events` made inside `process()` happened, while
//! `monitor()` and `recGblFwdLink` did not, and the next `dbProcess` runs.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{
    CycleHooks, CycleMarks, CyclePostMask, FieldDesc, ProcessAction, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

/// Each `process()` bumps N, writes it through COUT, marks X and changes Y
/// without marking it. EMIT selects `Complete` over `CompleteNoEmit`.
#[derive(Default)]
struct NoEmitRecord {
    n: f64,
    y: f64,
    emit: bool,
    cout: String,
    marked: bool,
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("X", DbFieldType::Double, false),
    FieldDesc::new("Y", DbFieldType::Double, false),
    FieldDesc::new("EMIT", DbFieldType::Short, false),
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("COUT", DbFieldType::String, false)
    },
];

impl Record for NoEmitRecord {
    fn record_type(&self) -> &'static str {
        "noemit_test"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::PROCESS_POSTED
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.n += 1.0;
        self.y += 1.0;
        self.marked = true;
        let actions = vec![ProcessAction::WriteDbLink {
            link_field: "COUT",
            value: EpicsValue::Double(self.n),
        }];
        Ok(if self.emit {
            ProcessOutcome::complete_with(actions)
        } else {
            ProcessOutcome::complete_no_emit_with(actions)
        })
    }
    fn take_process_posted_fields(&mut self) -> CycleMarks {
        static X_POST: [(&str, CyclePostMask); 1] = [("X", CyclePostMask::Value)];
        CycleMarks::of(&X_POST, u64::from(std::mem::take(&mut self.marked)))
    }
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["X"]
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "X" => Some(EpicsValue::Double(0.0)),
            "Y" => Some(EpicsValue::Double(self.y)),
            "EMIT" => Some(EpicsValue::Short(self.emit.into())),
            "COUT" => Some(EpicsValue::String(self.cout.clone().into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "EMIT" => self.emit = value.to_f64().unwrap_or(0.0) != 0.0,
            "COUT" => self.cout = value.to_string(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

struct Ioc {
    db: Arc<PvDatabase>,
    x: EventReader,
    y: EventReader,
}

async fn ioc(emit: bool) -> Ioc {
    let (db, _) = IocBuilder::new()
        .register_record_type("noemit_test", || Box::new(NoEmitRecord::default()))
        .db_string(
            &format!(
                r#"
record(noemit_test, "N") {{ field(EMIT, "{}") field(COUT, "T.VAL") field(FLNK, "F") }}
record(ao, "T") {{}}
record(calc, "F") {{ field(CALC, "A+1") field(INPA, "F") }}
"#,
                u8::from(emit)
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let (x, y) = {
        let rec = db.get_record("N").unwrap();
        let mut inst = rec.write();
        let mask = EventMask::VALUE.bits();
        (
            inst.add_subscriber("X", 1, DbFieldType::Double, mask)
                .unwrap(),
            inst.add_subscriber("Y", 2, DbFieldType::Double, mask)
                .unwrap(),
        )
    };
    Ioc { db, x, y }
}

fn count(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// Process N once; the X and Y events that cycle posted.
async fn cycle(ioc: &mut Ioc) -> (usize, usize) {
    count(&mut ioc.x);
    count(&mut ioc.y);
    ioc.db
        .process_record_with_links("N", &mut ProcStack::new())
        .await
        .unwrap();
    (count(&mut ioc.x), count(&mut ioc.y))
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_no_emit_cycle_runs_its_actions_posts_its_marks_and_stops_there() {
    let mut ioc = ioc(false).await;
    assert_eq!(cycle(&mut ioc).await, (1, 0), "X marked, Y not posted");
    assert_eq!(num(&ioc.db, "T.VAL"), 1.0, "COUT written");
    assert_eq!(num(&ioc.db, "F.VAL"), 0.0, "FLNK not followed");
    assert_eq!(num(&ioc.db, "N.PACT"), 0.0);

    // PACT clear, so the next dbProcess runs rather than being refused.
    assert_eq!(cycle(&mut ioc).await, (1, 0));
    assert_eq!(num(&ioc.db, "T.VAL"), 2.0);
    assert_eq!(num(&ioc.db, "N.LCNT"), 0.0);
    assert_eq!(num(&ioc.db, "F.VAL"), 0.0);
}

/// The same record completing: what the no-emit cycle skips is the epilogue.
#[epics_macros_rs::epics_test]
async fn a_complete_cycle_runs_the_epilogue_the_no_emit_cycle_skips() {
    let mut ioc = ioc(true).await;
    assert_eq!(cycle(&mut ioc).await, (1, 1));
    assert_eq!(num(&ioc.db, "T.VAL"), 1.0);
    assert_eq!(num(&ioc.db, "F.VAL"), 1.0);
    assert_eq!(num(&ioc.db, "N.PACT"), 0.0);
}

/// STAT, SEVR and TIME: what `recGblGetTimeStamp` and `monitor()`'s
/// `recGblResetAlarms` would move.
fn stamp_and_alarm(db: &PvDatabase) -> (u16, u16, std::time::SystemTime) {
    let rec = db.get_record("N").unwrap();
    let g = rec.read();
    (g.common.stat, g.common.sevr as u16, g.common.time)
}

/// `scalerRecord.c:470`: a non-idle cycle runs no stamp and no alarm reset,
/// so the init UDF/INVALID survives it on either dispatch path; the
/// completing cycle is what commits and stamps.
#[epics_macros_rs::epics_test]
async fn a_no_emit_cycle_leaves_the_stamp_and_the_alarm() {
    let mut ioc = ioc(false).await;
    let init = stamp_and_alarm(&ioc.db);
    assert_eq!((init.0, init.1), (17, 3), "init UDF/INVALID");
    cycle(&mut ioc).await;
    assert_eq!(stamp_and_alarm(&ioc.db), init, "engine path");
    {
        let rec = ioc.db.get_record("N").unwrap();
        rec.write().process_local().unwrap();
    }
    assert_eq!(stamp_and_alarm(&ioc.db), init, "process_local");
    assert_eq!(count(&mut ioc.x), 1, "process_local posts the mark too");

    ioc.db
        .put_record_field_from_ca("N", "EMIT", EpicsValue::Short(1))
        .await
        .unwrap();
    cycle(&mut ioc).await;
    let done = stamp_and_alarm(&ioc.db);
    assert_ne!(done.2, init.2, "the completing cycle stamps");
}
