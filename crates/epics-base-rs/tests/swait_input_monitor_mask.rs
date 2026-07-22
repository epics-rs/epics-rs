//! R9-72 — swait posts a changed input A..L with `monitor_mask | DBE_VALUE`.
//!
//! C `swaitRecord.c::monitor` (646-653):
//!
//! ```c
//! for (i=0, pnew=&pwait->a, pprev=&pwait->la; i<MAX_FIELDS; i++, pnew++, pprev++) {
//!     if (*pnew != *pprev) {
//!         db_post_events(pwait, pnew, monitor_mask|DBE_VALUE);
//! ```
//!
//! No forced `DBE_LOG`. `calcRecord.c:420` — the same loop, one module over —
//! writes `monitor_mask | DBE_VALUE | DBE_LOG`, and that forced-LOG shape was
//! the framework default the port applied to every record, swait included.
//!
//! The LOG bit rides along only when it is already in `monitor_mask` — i.e.
//! when VAL's own ADEL deadband crossed this cycle (`recGblResetAlarms` gives
//! the alarm bits, MDEL gives `DBE_VALUE`, ADEL gives `DBE_LOG`). So a
//! `DBE_LOG` subscriber (an archiver) on `swait.A` is sent a value on exactly
//! those cycles, not on every input change.
//!
//! The inputs are moved through their links, not by a direct put: a `dbPut` to
//! a field posts that field `DBE_VALUE | DBE_LOG` on its own, which is a
//! different post from `monitor()`'s.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const ALL: u16 = 0x07; // DBE_VALUE | DBE_LOG | DBE_ALARM

/// Subscribe to `REC.A` with every event class, so the post's own mask decides
/// what arrives.
async fn subscribe_a(db: &PvDatabase, rec: &str) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber("A", 1, DbFieldType::Double, ALL)
        .expect("A subscription must be accepted")
}

/// swait: CALC="A", input A driven from SRC (=7) through INAN, so VAL := A.
async fn swait_db(mdel: f64, adel: f64) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    w.put_field("INAN", EpicsValue::String("SRC".into()))
        .unwrap();
    w.put_field("MDEL", EpicsValue::Double(mdel)).unwrap();
    w.put_field("ADEL", EpicsValue::Double(adel)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// One priming cycle, then move SRC by `delta`. The first cycle of a fresh
/// record has no MLST/ALST history, so both deadbands trigger unconditionally
/// (C seeds them in `init_record`); the deadbands only decide anything from the
/// second cycle on. Drains the priming posts so the caller reads the post it
/// is actually testing.
async fn prime_then_move(db: &PvDatabase, rec: &str, rx: &mut EventReader, to: f64) {
    process(db, rec).await;
    while rx.try_recv().is_ok() {}
    db.put_pv("SRC", EpicsValue::Double(to)).await.unwrap();
    process(db, rec).await;
}

/// VAL's ADEL is wide enough that the archive deadband does not cross, so
/// `monitor_mask` carries no `DBE_LOG` and the changed input A posts
/// `DBE_VALUE` alone.
#[tokio::test]
async fn r9_72_changed_input_posts_dbe_value_without_dbe_log() {
    let db = swait_db(0.0, 1000.0).await;
    let mut rx = subscribe_a(&db, "W").await;

    // 7 -> 8: crosses MDEL (0) but not ADEL (1000).
    prime_then_move(&db, "W", &mut rx, 8.0).await;

    let event = rx.try_recv().expect("a changed input A must post");
    assert_eq!(
        event.mask,
        EventMask::VALUE,
        "C posts a changed swait input with `monitor_mask | DBE_VALUE`; VAL \
         moved 7 -> 8 with ADEL=1000, so monitor_mask holds DBE_VALUE (MDEL \
         crossed) and no DBE_LOG — got {:?}",
        event.mask
    );
}

/// ADEL=0: VAL's archive deadband crosses, so `monitor_mask` itself carries
/// `DBE_LOG` and C's `monitor_mask | DBE_VALUE` includes it. The LOG bit is not
/// forbidden — it is derived from VAL's mask.
#[tokio::test]
async fn r9_72_input_post_carries_log_when_vals_adel_crossed() {
    let db = swait_db(0.0, 0.0).await;
    let mut rx = subscribe_a(&db, "W").await;

    // 7 -> 8: crosses ADEL (0) too.
    prime_then_move(&db, "W", &mut rx, 8.0).await;

    let event = rx.try_recv().expect("a changed input A must post");
    assert_eq!(
        event.mask,
        EventMask::VALUE | EventMask::LOG,
        "ADEL=0 puts DBE_LOG into VAL's monitor_mask on any change; the input \
         post inherits it — got {:?}",
        event.mask
    );
}

/// The other side of the same rule: `calcRecord.c:420` DOES force
/// `DBE_VALUE | DBE_LOG` on a changed input, so calc's mask must not move.
#[tokio::test]
async fn r9_72_calc_input_post_still_forces_dbe_log() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();
    let mut c = CalcRecord::default();
    c.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    c.put_field("INPA", EpicsValue::String("SRC".into()))
        .unwrap();
    // The same wide ADEL as the swait DBE_VALUE-only case above: any LOG bit
    // here comes from calc's forced `| DBE_LOG`, not from a deadband crossing.
    c.put_field("MDEL", EpicsValue::Double(0.0)).unwrap();
    c.put_field("ADEL", EpicsValue::Double(1000.0)).unwrap();
    db.add_record("C", Box::new(c)).await.unwrap();
    let mut rx = subscribe_a(&db, "C").await;

    // Same 7 -> 8 move, same uncrossed ADEL as the swait case above.
    prime_then_move(&db, "C", &mut rx, 8.0).await;

    let event = rx.try_recv().expect("a changed input A must post");
    assert_eq!(
        event.mask,
        EventMask::VALUE | EventMask::LOG,
        "calcRecord.c:420 posts a changed input with `monitor_mask | \
         DBE_VALUE | DBE_LOG` — the forced LOG stays, ADEL crossing or not — \
         got {:?}",
        event.mask
    );
}
