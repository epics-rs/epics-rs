//! A cycle nobody watches still owes the record's own post state.
//!
//! `collect_subscriber_posts` returns early on an unwatched cycle, but C's
//! `monitor()` runs whether or not anyone is subscribed: it clears
//! `pcalc->newm` every cycle, and a published value (`last_posted`) advances
//! with no subscriber. One case per `MonitorHot::WALK_OWED` bit that keeps an
//! unwatched cycle going.

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{CycleHooks, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn subscribe(db: &PvDatabase, rec: &str, field: &str, sid: u32) -> EventReader {
    db.get_record(rec)
        .unwrap()
        .write()
        .add_subscriber(
            field,
            sid,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .unwrap()
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// CYCLE_POSTED: the unwatched cycle whose link changed AA sets NEWM, and
/// `monitor()` clears it then (aCalcoutRecord.c:1036). Left set, the next
/// cycle — same link value, now watched — would post AA from the stale bit.
#[epics_macros_rs::epics_test]
async fn an_unwatched_cycle_clears_newm() {
    let db = PvDatabase::new();
    let mut w = WaveformRecord::new(4, DbFieldType::Double);
    w.put_field(
        "VAL",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0].into()),
    )
    .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    a.put_field("CALC", EpicsValue::String("SUM(AA)".into()))
        .unwrap();
    a.special("CALC", true, &Default::default()).unwrap();
    a.put_field("INAA", EpicsValue::String("W".into())).unwrap();
    db.add_record("A", Box::new(a)).await.unwrap();

    process(&db, "A").await;
    let mut rx = subscribe(&db, "A", "AA", 1);
    process(&db, "A").await;
    assert_eq!(
        drain(&mut rx),
        0,
        "the link delivered what AA already held; NEWM must have been cleared \
         by the unwatched cycle that set it"
    );
}

/// A record whose X no declared set or old copy covers: the walk alone
/// decides it, against the value last published. No cycle hook, so only
/// PUBLISHED keeps an unwatched cycle walking.
struct Knob {
    x: f64,
}

static KNOB_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("X", DbFieldType::Double, false),
];

impl Record for Knob {
    fn record_type(&self) -> &'static str {
        "unwatchedknob"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            "X" => Some(EpicsValue::Double(self.x)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Double(_)) => Ok(()),
            ("X", EpicsValue::Double(v)) => {
                self.x = v;
                Ok(())
            }
            (name, _) => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        KNOB_FIELDS
    }
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::NONE
    }
}

/// PUBLISHED: a field published once and now unwatched is still walked, so
/// its published value follows the record. X 7 is published, goes 9 with
/// nobody watching, then back to 7: the 7 differs from the 9 last published
/// and must post.
#[epics_macros_rs::epics_test]
async fn an_unwatched_cycle_advances_a_published_field() {
    let db = PvDatabase::new();
    db.add_record("K", Box::new(Knob { x: 7.0 })).await.unwrap();
    let inst = db.get_record("K").unwrap();
    let set_x = |v: f64| {
        inst.write()
            .record
            .put_field("X", EpicsValue::Double(v))
            .unwrap()
    };

    let first = subscribe(&db, "K", "X", 1);
    inst.write().remove_subscriber(first.id());
    set_x(9.0);
    process(&db, "K").await;
    set_x(7.0);
    let mut rx = subscribe(&db, "K", "X", 2);
    process(&db, "K").await;
    let mut last = None;
    while let Ok(e) = rx.try_recv() {
        last = e.value().as_value().cloned();
    }
    assert_eq!(
        last,
        Some(EpicsValue::Double(7.0)),
        "the unwatched cycle must advance X's published value to 9, or the \
         return to 7 compares equal to the stale 7 and is dropped"
    );
}
