//! `last_posted` — the value a field last published — advances on every
//! value-class post whether or not the field has a subscriber.
//!
//! C `db_post_events` (`dbEvent.c:876-906`) is a no-op on an empty `mlis`, and
//! the record's own state (`motorRecord.cc:2603-2606` `if (pmr->dmov == TRUE)`,
//! `MARKED(M_DMOV)`) advances regardless, so a later subscriber sees the next
//! transition. The framework's change detector advanced `last_posted` on a
//! move-start notify with no subscriber but not on the completion walk (it
//! walked subscribed fields only) nor on an out-of-band post to a field with
//! no subscriber bucket, so the cache kept the move-start 0 while DMOV read 1
//! and the next subscriber's move-start 0 compared equal and was dropped —
//! ophyd `EpicsMotor` waits for a 1→0→1 DMOV and never finished its move.
//!
//! One case per boundary: no bucket, bucket emptied by `remove_subscriber`,
//! and an out-of-band post with no bucket; the control keeps a subscriber
//! throughout.

use std::collections::HashSet;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A two-pass move: a pass from DMOV=1 drops it and returns
/// `AsyncPendingNotify` (motor's move start), the next pass raises it and
/// completes (the completion walk).
struct Mover {
    dmov: i16,
}

static MOVER_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("DMOV", DbFieldType::Short, false),
];

impl Record for Mover {
    fn record_type(&self) -> &'static str {
        "lastpostedmover"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.dmov == 1 {
            self.dmov = 0;
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(vec![(
                    "DMOV".to_string(),
                    EpicsValue::Short(0),
                )]),
                actions: Vec::new(),
                device_did_compute: false,
                post_write_fields: Vec::new(),
            })
        } else {
            self.dmov = 1;
            Ok(ProcessOutcome::complete())
        }
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            "DMOV" => Some(EpicsValue::Short(self.dmov)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Double(_)) => Ok(()),
            ("DMOV", EpicsValue::Short(v)) => {
                self.dmov = v;
                Ok(())
            }
            (name, _) => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        MOVER_FIELDS
    }
}

async fn mover_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("M", Box::new(Mover { dmov: 1 }))
        .await
        .unwrap();
    db
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .unwrap();
}

/// Move start then move completion.
async fn move_once(db: &PvDatabase) {
    process(db, "M").await;
    process(db, "M").await;
}

fn subscribe(db: &PvDatabase, record: &str, field: &str, sid: u32) -> EventReader {
    db.get_record(record)
        .unwrap()
        .write()
        .add_subscriber(
            field,
            sid,
            DbFieldType::Short,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .unwrap()
}

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event.snapshot.value.clone());
    }
    out
}

const ZERO_THEN_ONE: [EpicsValue; 2] = [EpicsValue::Short(0), EpicsValue::Short(1)];

#[epics_macros_rs::epics_test]
async fn subscribed_move_posts_dmov_zero_then_one() {
    let db = mover_db().await;
    let mut rx = subscribe(&db, "M", "DMOV", 1);
    move_once(&db).await;
    assert_eq!(drain(&mut rx), ZERO_THEN_ONE);
}

/// Boundary: the field never had a subscriber bucket during the first move.
#[epics_macros_rs::epics_test]
async fn move_with_no_bucket_then_subscribe_sees_move_start() {
    let db = mover_db().await;
    move_once(&db).await;
    let mut rx = subscribe(&db, "M", "DMOV", 1);
    move_once(&db).await;
    assert_eq!(
        drain(&mut rx),
        ZERO_THEN_ONE,
        "an unwatched completion must advance DMOV's published value to 1, or the \
         next move-start 0 compares equal to the stale 0 and is dropped"
    );
}

/// Boundary: the bucket exists but is empty — a client monitored earlier and
/// went away — during the unwatched move.
#[epics_macros_rs::epics_test]
async fn move_with_emptied_bucket_then_subscribe_sees_move_start() {
    let db = mover_db().await;
    let mut first = subscribe(&db, "M", "DMOV", 1);
    move_once(&db).await;
    assert_eq!(drain(&mut first), ZERO_THEN_ONE);
    db.get_record("M").unwrap().write().remove_subscriber(1);
    move_once(&db).await;
    let mut rx = subscribe(&db, "M", "DMOV", 2);
    move_once(&db).await;
    assert_eq!(drain(&mut rx), ZERO_THEN_ONE);
}

/// Boundary: an out-of-band post (`post_fields`, the driver poster) to a field
/// with no subscriber bucket. The move-start notify published DMOV=0; the post
/// publishes DMOV=1; the next move start must post 0 to a subscriber that
/// joined in between.
#[epics_macros_rs::epics_test]
async fn post_with_no_bucket_then_subscribe_sees_move_start() {
    let db = mover_db().await;
    process(&db, "M").await;
    db.post_fields("M", vec![("DMOV".to_string(), EpicsValue::Short(1))])
        .unwrap();
    let mut rx = subscribe(&db, "M", "DMOV", 1);
    process(&db, "M").await;
    assert_eq!(
        drain(&mut rx),
        [EpicsValue::Short(0)],
        "a post to a field with no subscriber must advance its published value"
    );
}

/// The same defect on a stock record: aCalcout's ODLY cycle posts DLYA=1
/// through `AsyncPendingNotify` and its continuation clears DLYA through the
/// completion walk.
#[epics_macros_rs::epics_test]
async fn acalcout_unwatched_odly_then_subscribe_sees_dlya_rise() {
    let db = PvDatabase::new();
    let mut ac = AcalcoutRecord::default();
    ac.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    ac.special("CALC", true).unwrap();
    ac.put_field("OOPT", EpicsValue::Short(0)).unwrap();
    ac.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record("AC", Box::new(ac)).await.unwrap();

    let odly_cycle = |db: &PvDatabase| {
        let db = db.clone();
        async move {
            process(&db, "AC").await;
            db.process_record_continuation("AC", &mut HashSet::new())
                .await
                .unwrap();
        }
    };

    odly_cycle(&db).await;
    let mut rx = subscribe(&db, "AC", "DLYA", 1);
    odly_cycle(&db).await;
    assert_eq!(
        drain(&mut rx),
        [EpicsValue::UShort(1), EpicsValue::UShort(0)],
        "an unwatched continuation must advance DLYA's published value to 0"
    );
}
