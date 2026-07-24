//! R11-64 (widened) — transform posts A..P with a literal `DBE_VALUE|DBE_LOG`.
//!
//! The same C shape as `epidRecord.c:376`, and the only other one in the ported
//! record set: `transformRecord.c:793-794` computes the cycle's alarm mask and
//! then throws it away.
//!
//! ```c
//! monitor_mask = recGblResetAlarms(ptran);
//! monitor_mask = DBE_VALUE|DBE_LOG;          /* :794 — assignment, not |= */
//! for (i = 0, pnew = &ptran->a, pprev = &ptran->la; i < MAX_FIELDS; i++, ...) {
//!     if ((*pnew != *pprev) || (prpvt->firstCalcPosted == 0)) {
//!         db_post_events(ptran, pnew, monitor_mask);
//!         *pprev = *pnew;
//!     }
//! }
//! ```
//!
//! So NO transform field ever carries `DBE_ALARM` — a `DBE_ALARM`-only client on
//! `.A` is notified on no cycle at all, not even the one the record went into
//! alarm on. The port posted A..P through the generic change loop, which forces
//! `alarm_bits | DBE_VALUE | DBE_LOG`.
//!
//! The alarm here arrives the way it does in a real transform: an `MS` input
//! link whose source record is in alarm (`transformRecord.c:554` reads `nsev`
//! folded in by `dbGetLink`), so the same cycle both moves A and raises the
//! severity.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// Drive the source and process it, so its severity is settled before the
/// transform fetches it.
async fn set(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
    process(db, rec).await;
}

/// `SRC` alarms MAJOR above 5; `T.INPA` reads it with the `MS` modifier, so the
/// severity folds into the transform's `nsev` on the cycle that fetches it —
/// the same cycle that moves A.
async fn transform_with_an_ms_input() -> PvDatabase {
    let db = PvDatabase::new();

    db.add_record("SRC", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    {
        // The alarm limits live in the INSTANCE's `analog_alarm` config.
        let rec = db.get_record("SRC").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("HIHI", EpicsValue::Double(5.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
    }

    let mut t = TransformRecord::default();
    t.put_field("INPA", EpicsValue::String("SRC MS".into()))
        .unwrap();
    db.add_record("T", Box::new(t)).await.unwrap();
    db
}

/// The finding: on the alarm-transition cycle, A posts with a literal
/// `DBE_VALUE|DBE_LOG` — no alarm bit — while STAT (posted by
/// `recGblResetAlarms` itself, not by `monitor()`) does carry `DBE_ALARM`.
#[epics_macros_rs::epics_test]
async fn r11_64_transform_channels_post_without_the_cycles_alarm_bits() {
    let db = transform_with_an_ms_input().await;
    let inst = db.get_record("T").unwrap();

    let full = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    let (mut a_rx, mut stat_rx) = {
        let mut g = inst.write();
        let a = g
            .add_subscriber("A", 1, DbFieldType::Double, full)
            .expect("an A subscription must be accepted");
        let stat = g
            .add_subscriber("STAT", 2, DbFieldType::Short, EventMask::ALARM.bits())
            .expect("a STAT subscription must be accepted");
        (a, stat)
    };

    // Baseline cycle: SRC=1, no alarm. A takes the input value and posts.
    process(&db, "T").await;
    while a_rx.try_recv().is_ok() {}
    while stat_rx.try_recv().is_ok() {}

    // SRC crosses HIHI. The next transform cycle fetches it: A moves 1 -> 10 AND
    // the MS link raises MAJOR — one cycle, both facts.
    set(&db, "SRC", 10.0).await;
    process(&db, "T").await;

    {
        let g = inst.read();
        assert_eq!(
            g.common.sevr,
            AlarmSeverity::Major,
            "the MS input link folds SRC's MAJOR into the transform"
        );
    }
    assert!(
        stat_rx.try_recv().is_ok(),
        "the severity moved, so recGblResetAlarms posted STAT with DBE_ALARM — \
         the cycle's alarm bits are live"
    );

    let e = a_rx.try_recv().expect("A moved 1 -> 10, so C posts it");
    assert_eq!(e.snapshot.value, EpicsValue::Double(10.0));
    assert_eq!(
        e.mask,
        EventMask::VALUE | EventMask::LOG,
        "transformRecord.c:794 overwrites the alarm mask with a literal \
         DBE_VALUE|DBE_LOG before the A..P post loop, so A carries no alarm bit"
    );
}

/// The subscriber-side statement of the same fact, across both alarm
/// transitions.
#[epics_macros_rs::epics_test]
async fn r11_64_an_alarm_only_subscriber_on_a_transform_channel_never_fires() {
    let db = transform_with_an_ms_input().await;
    let inst = db.get_record("T").unwrap();

    let mut a_rx = inst
        .write()
        .add_subscriber("A", 1, DbFieldType::Double, EventMask::ALARM.bits())
        .expect("an A subscription must be accepted");

    process(&db, "T").await; // SRC=1: NO_ALARM

    set(&db, "SRC", 10.0).await;
    process(&db, "T").await; // NO_ALARM -> MAJOR, A: 1 -> 10

    set(&db, "SRC", 2.0).await;
    process(&db, "T").await; // MAJOR -> NO_ALARM, A: 10 -> 2

    {
        let g = inst.read();
        assert_eq!(g.common.sevr, AlarmSeverity::NoAlarm);
        assert_eq!(
            g.record.get_field("A").unwrap(),
            EpicsValue::Double(2.0),
            "A really was changing under the subscriber on both transitions"
        );
    }
    assert!(
        a_rx.try_recv().is_err(),
        "no transform post carries DBE_ALARM (transformRecord.c:794), so a \
         DBE_ALARM-only subscriber receives nothing on either transition"
    );
}
