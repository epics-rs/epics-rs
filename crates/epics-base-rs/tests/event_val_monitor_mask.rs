//! R9-77 — the event record posts VAL with `monitor_mask | DBE_VALUE`, never
//! with `DBE_LOG`.
//!
//! C `eventRecord.c::monitor` (157-165) is the record's entire posting:
//!
//! ```c
//! monitor_mask = recGblResetAlarms(prec);
//! db_post_events(prec,&prec->val,monitor_mask|DBE_VALUE);
//! ```
//!
//! `monitor_mask` is `recGblResetAlarms`'s return — the alarm bits alone. event
//! has no MDEL/ADEL, and the post carries no `if (monitor_mask)` guard, so VAL
//! posts on EVERY process with `DBE_VALUE` (+ `DBE_ALARM` when the alarm moved)
//! and on no cycle with `DBE_LOG`. It is the only record in base whose VAL post
//! is written this way; every other one is `if (monitor_mask) db_post_events(
//! &prec->val, monitor_mask)`.
//!
//! The port gave VAL the framework default `DBE_VALUE | DBE_LOG`, so a
//! `DBE_LOG`-only archiver was sent the event name on every process.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::event::EventRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const ALL: u16 = 0x07; // DBE_VALUE | DBE_LOG | DBE_ALARM

async fn subscribe_val(db: &PvDatabase, rec: &str, dbf: DbFieldType) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber("VAL", 1, dbf, ALL)
        .expect("VAL subscription must be accepted")
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// The post carries `DBE_VALUE` and nothing else: no alarm moved, and event has
/// no ADEL to put `DBE_LOG` into the mask.
#[tokio::test]
async fn r9_77_event_val_posts_dbe_value_without_dbe_log() {
    let db = PvDatabase::new();
    db.add_record("EV", Box::new(EventRecord::new("shutter")))
        .await
        .unwrap();
    let mut rx = subscribe_val(&db, "EV", DbFieldType::String).await;

    // The first process carries the born-UDF -> NO_ALARM transition
    // (dbCommon.dbd STAT `initial("UDF")`), so `recGblResetAlarms` puts
    // DBE_ALARM in `monitor_mask` there — in C too. The steady-state cycle,
    // where no alarm moves, is the one this test is about.
    process(&db, "EV").await;
    let first = rx.try_recv().expect("event VAL posts on every process");
    assert_eq!(first.mask, EventMask::VALUE | EventMask::ALARM);

    process(&db, "EV").await;

    let event = rx.try_recv().expect("event VAL posts on every process");
    assert_eq!(
        event.mask,
        EventMask::VALUE,
        "C posts event VAL with `monitor_mask | DBE_VALUE` and monitor_mask is \
         empty here (no alarm change, no MDEL/ADEL) — got {:?}",
        event.mask
    );
}

/// C posts VAL unguarded on every `monitor()` call, so a second cycle with an
/// unchanged event name posts again — and again without `DBE_LOG`.
#[tokio::test]
async fn r9_77_event_val_reposts_each_cycle_still_without_log() {
    let db = PvDatabase::new();
    db.add_record("EV", Box::new(EventRecord::new("shutter")))
        .await
        .unwrap();
    let mut rx = subscribe_val(&db, "EV", DbFieldType::String).await;

    process(&db, "EV").await;
    rx.try_recv().expect("first cycle posts");

    process(&db, "EV").await;

    let event = rx
        .try_recv()
        .expect("C's VAL post has no `if (monitor_mask)` guard — it fires again");
    assert_eq!(
        event.mask,
        EventMask::VALUE,
        "the unchanged re-post carries DBE_VALUE alone — got {:?}",
        event.mask
    );
}

/// A `DBE_LOG`-only subscriber — an archiver — receives event VAL on no cycle
/// at all. This is the observable the forced LOG bit broke.
#[tokio::test]
async fn r9_77_dbe_log_only_subscriber_receives_nothing() {
    let db = PvDatabase::new();
    db.add_record("EV", Box::new(EventRecord::new("shutter")))
        .await
        .unwrap();
    let mut rx = {
        let inst = db.get_record("EV").unwrap();
        let mut g = inst.write();
        g.add_subscriber("VAL", 1, DbFieldType::String, 0x02) // DBE_LOG only
            .expect("VAL subscription must be accepted")
    };

    process(&db, "EV").await;
    db.put_pv("EV", EpicsValue::String("beamdump".into()))
        .await
        .unwrap();
    process(&db, "EV").await;

    // The `dbPut` itself posts DBE_VALUE|DBE_LOG (a put's own post, not
    // `monitor()`'s) — drain that one, it is not the post under test.
    let from_put = rx.try_recv();
    if let Ok(ev) = &from_put {
        assert!(
            ev.mask.contains(EventMask::LOG),
            "only a put post may reach a LOG-only subscriber here — got {:?}",
            ev.mask
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "monitor()'s VAL post never carries DBE_LOG, so no processing cycle \
         reaches a DBE_LOG-only subscriber"
    );
}

/// The rule is per-record, not a framework-wide change: calc's VAL still takes
/// `DBE_LOG` from its own ADEL deadband (`calcRecord.c:411`, the standard
/// `if (monitor_mask) db_post_events(&prec->val, monitor_mask)`).
#[tokio::test]
async fn r9_77_calc_val_still_logs_on_adel_crossing() {
    let db = PvDatabase::new();
    // `CalcRecord::new` compiles RPCL at construction; a bare `put_field`
    // stores only the string (C's dbPut/special split — `special("CALC")`
    // owns the compile, and no init pass runs on this direct-add path).
    let mut c = CalcRecord::new("VAL+1");
    c.put_field("ADEL", EpicsValue::Double(0.0)).unwrap();
    c.put_field("MDEL", EpicsValue::Double(0.0)).unwrap();
    db.add_record("C", Box::new(c)).await.unwrap();
    let mut rx = subscribe_val(&db, "C", DbFieldType::Double).await;

    process(&db, "C").await;
    while rx.try_recv().is_ok() {}
    process(&db, "C").await;

    let event = rx.try_recv().expect("calc VAL moved, so it posts");
    assert!(
        event.mask.contains(EventMask::LOG),
        "ADEL=0 puts DBE_LOG into calc's monitor_mask — got {:?}",
        event.mask
    );
}
