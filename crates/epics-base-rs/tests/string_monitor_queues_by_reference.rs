//! A `DBF_STRING` monitor queues BY REFERENCE in every stock C build, so an
//! undrained subscription holds one entry, not one per post.
//!
//! `db_add_event` decides it once per subscription:
//!
//! ```c
//! if (dbChannelElements(chan) == 1 &&
//!     dbChannelSpecial(chan) != SPC_DBADDR &&
//!     dbChannelFieldSize(chan) <= sizeof(union native_value)) {
//!     pevent->useValque = TRUE;
//! }
//! ```
//! (`dbEvent.c:493-500`). `union native_value` (`db_field_log.h:41-56`) lists
//! its `char dbf_string[MAX_STRING_SIZE]` member behind
//! `#ifdef DB_EVENT_LOG_STRINGS` — a macro epics-base names only in that
//! header's own comment and `#ifdef` and defines nowhere — so the union is its
//! `epicsFloat64` member, 8 bytes. A `DBF_STRING` field is 40. `40 <= 8` is
//! FALSE, `useValque` is FALSE, and `db_queue_event_log`'s early-drop
//! (`dbEvent.c:794-800`) then keeps ONE entry for that monitor however many
//! posts arrive.
//!
//! The port had `NATIVE_VALUE_BYTES = 40` and compared it against the CONTENT
//! length of the string rather than the declared width of the field, so every
//! string shorter than 41 bytes queued by value: three posts to an undrained
//! monitor became three camonitor lines where C prints one.
//!
//! The boundaries are the two sides of that predicate — a string field and a
//! numeric field of the same record, subscribed the same way, posted the same
//! number of times — plus the one thing the port must get right that C gets
//! for free: C's surviving by-reference log reads the record's LIVE field at
//! delivery, so the single entry the port keeps must carry the NEWEST value,
//! never the oldest.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// The three values every test posts, in order. The last is what a C client
/// would see on the single delivery.
const STRINGS: [&str; 3] = ["first", "second", "third"];

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn subscribe(db: &PvDatabase, rec: &str, field: &str, sid: u32, ty: DbFieldType) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber(field, sid, ty, EventMask::VALUE.bits())
        .expect("the subscription must be accepted")
}

/// Everything queued for this reader, drained. The reader is never touched
/// between the posts, which is the undrained C monitor these tests model.
fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.snapshot.value.clone());
    }
    out
}

/// The defect, at the level an operator sees it: three string posts, one
/// delivery — and the delivery carries the LAST value, which is what C's
/// surviving reference reads out of the record at delivery time.
#[epics_macros_rs::epics_test]
async fn three_posts_to_a_string_monitor_deliver_one_latest_event() {
    let db = PvDatabase::new();
    db.add_record("SI", Box::new(StringinRecord::new("")))
        .await
        .unwrap();
    let mut rx = subscribe(&db, "SI", "VAL", 1, DbFieldType::String);

    for s in STRINGS {
        db.put_pv("SI", EpicsValue::String((*s).into()))
            .await
            .unwrap();
        process(&db, "SI").await;
    }

    assert_eq!(
        drain(&mut rx),
        vec![EpicsValue::String(STRINGS[2].into())],
        "a DBF_STRING field is 40 bytes wide and the union is 8, so C's \
         useValque is FALSE and the monitor holds ONE entry — carrying the \
         newest value, because C's reference reads the live field at delivery"
    );
    assert!(
        rx.queue().latest_only(1),
        "the subscription must be latched by-reference, not merely coalesced \
         by chance"
    );
    assert_eq!(
        rx.queue().ncollapse(1),
        2,
        "two of the three posts were absorbed by the early-drop"
    );
}

/// Content length must not decide it. C compares `dbChannelFieldSize`, the
/// DECLARED width of the field, so a one-character string is refused exactly
/// as a forty-character one is — the port's old test asserted the opposite.
#[epics_macros_rs::epics_test]
async fn a_one_character_string_is_still_by_reference() {
    let db = PvDatabase::new();
    db.add_record("SI:SHORT", Box::new(StringinRecord::new("")))
        .await
        .unwrap();
    let mut rx = subscribe(&db, "SI:SHORT", "VAL", 1, DbFieldType::String);

    for s in ["a", "b", "c"] {
        db.put_pv("SI:SHORT", EpicsValue::String(s.into()))
            .await
            .unwrap();
        process(&db, "SI:SHORT").await;
    }

    assert_eq!(
        drain(&mut rx),
        vec![EpicsValue::String("c".into())],
        "40 <= 8 is false whatever the string holds"
    );
}

/// The other side of the same predicate, and the control that keeps the two
/// tests above from passing on a queue that collapses everything: a numeric
/// scalar field is 8 bytes or fewer, so `useValque` is TRUE and every post is
/// delivered separately.
#[epics_macros_rs::epics_test]
async fn a_numeric_scalar_monitor_still_queues_every_post() {
    let db = PvDatabase::new();
    db.add_record("AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let mut rx = subscribe(&db, "AI", "VAL", 1, DbFieldType::Double);

    for v in [1.0, 2.0, 3.0] {
        db.put_pv("AI", EpicsValue::Double(v)).await.unwrap();
        process(&db, "AI").await;
    }

    assert_eq!(
        drain(&mut rx),
        vec![
            EpicsValue::Double(1.0),
            EpicsValue::Double(2.0),
            EpicsValue::Double(3.0),
        ],
        "a DBF_DOUBLE field fits union native_value, so C queues each post"
    );
    assert!(
        !rx.queue().latest_only(1),
        "and the subscription is never latched into keep-only-the-latest"
    );
}
