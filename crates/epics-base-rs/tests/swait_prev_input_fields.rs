//! R9-75 — swait publishes LA..LL, the previous value of each input A..L.
//!
//! C `swaitRecord.dbd:298-331` declares twelve `DBF_DOUBLE` "Last Val of Input
//! x" fields, and C `swaitRecord.c::monitor` (646-653) is their writer:
//!
//! ```c
//! for (i=0, pnew=&pwait->a, pprev=&pwait->la; i<MAX_FIELDS; i++, pnew++, pprev++) {
//!     if (*pnew != *pprev) {
//!         db_post_events(pwait, pnew, monitor_mask|DBE_VALUE);
//!         *pprev = *pnew;
//!         db_post_events(pwait, pprev, monitor_mask|DBE_VALUE);
//!     }
//! }
//! ```
//!
//! So on a cycle where input A changed, C posts A *and* the freshly advanced
//! LA, both with `monitor_mask | DBE_VALUE` — the mask R9-72 gave A. The port
//! had no LA..LL fields at all.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const ALL: u16 = 0x07; // DBE_VALUE | DBE_LOG | DBE_ALARM

/// swait: CALC="A", input A driven from SRC through INAN. ADEL is wide so VAL's
/// archive deadband never crosses and `monitor_mask` is DBE_VALUE alone.
async fn swait_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    w.put_field("INAN", EpicsValue::String("SRC".into()))
        .unwrap();
    w.put_field("MDEL", EpicsValue::Double(0.0)).unwrap();
    w.put_field("ADEL", EpicsValue::Double(1000.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    db
}

async fn process(db: &PvDatabase) {
    let mut visited = HashSet::new();
    db.process_record_with_links("W", &mut visited, 0)
        .await
        .unwrap();
}

async fn subscribe(db: &PvDatabase, field: &str) -> EventReader {
    let inst = db.get_record("W").unwrap();
    let mut g = inst.write();
    g.add_subscriber(field, 1, DbFieldType::Double, ALL)
        .expect("subscription must be accepted")
}

/// LA tracks A: after a cycle that fetched A=7 through INAN, LA reads 7.
#[epics_macros_rs::epics_test]
async fn r9_75_la_holds_previous_input_value() {
    let db = swait_db().await;
    process(&db).await;

    let inst = db.get_record("W").unwrap();
    let g = inst.read();
    assert_eq!(
        g.record.get_field("A"),
        Some(EpicsValue::Double(7.0)),
        "INAN fetched SRC into A"
    );
    assert_eq!(
        g.record.get_field("LA"),
        Some(EpicsValue::Double(7.0)),
        "C `monitor()` advances *pprev = *pnew for the input it just posted"
    );
}

/// The advanced LA is posted with the same mask as A: `monitor_mask |
/// DBE_VALUE`, with no forced DBE_LOG (ADEL=1000 is not crossed by 7 -> 8).
#[epics_macros_rs::epics_test]
async fn r9_75_la_posts_with_the_inputs_monitor_mask() {
    let db = swait_db().await;
    let mut rx_a = subscribe(&db, "A").await;
    let mut rx_la = subscribe(&db, "LA").await;

    // Priming cycle: no MLST/ALST history yet, so drain what it posts.
    process(&db).await;
    while rx_a.try_recv().is_ok() {}
    while rx_la.try_recv().is_ok() {}

    db.put_pv("SRC", EpicsValue::Double(8.0)).await.unwrap();
    process(&db).await;

    let ev_a = rx_a.try_recv().expect("changed input A must post");
    let ev_la = rx_la.try_recv().expect("the advanced LA must post too");
    assert_eq!(
        ev_la.snapshot.value,
        EpicsValue::Double(8.0),
        "C posts LA *after* `*pprev = *pnew`, so the posted value is the new one"
    );
    assert_eq!(
        ev_la.mask,
        EventMask::VALUE,
        "LA rides the input's `monitor_mask | DBE_VALUE` — got {:?}",
        ev_la.mask
    );
    assert_eq!(
        ev_la.mask, ev_a.mask,
        "C posts the input and its previous-value field with the identical mask"
    );
}

/// An unchanged input posts nothing — C's `if (*pnew != *pprev)` guard covers
/// both `db_post_events` calls, so LA is silent on a no-change cycle.
#[epics_macros_rs::epics_test]
async fn r9_75_la_is_silent_when_the_input_did_not_change() {
    let db = swait_db().await;
    let mut rx_la = subscribe(&db, "LA").await;

    process(&db).await;
    while rx_la.try_recv().is_ok() {}

    // SRC unchanged: A stays 7, so LA stays 7.
    process(&db).await;
    assert!(
        rx_la.try_recv().is_err(),
        "input A did not change, so C posts neither A nor LA"
    );
}
