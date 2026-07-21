//! R12-62 — the count-completion cycle posts each changed `Sn` TWICE.
//!
//! C `scalerRecord.c::process()`:
//!
//!   * the done-interrupt arm sets `ss = SCALER_STATE_IDLE` (`:367-368`);
//!   * `updateCounts()` (`:453` → `:578-584`) posts every `Sn` whose count
//!     moved with `DBE_VALUE`;
//!   * `ss == IDLE` then admits the `monitor()` call at `:510-513`, and
//!     `monitor()` (`:757-773`) posts EVERY active `Sn` with a literal
//!     `DBE_LOG` — unconditionally, change or no change.
//!
//! So on the ONE cycle that carries the final counts, a changed `Sn` produces
//! two `db_post_events`: `DBE_VALUE`, then `DBE_LOG`. The port modelled the
//! `DBE_LOG` sweep as the `else` arm of the change check, so a changed `Sn`
//! could only ever emit the `DBE_VALUE` half — a `DBE_LOG`-only archiver never
//! received the final counts, which is the only `Sn` sample it wants.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use scaler_rs::records::scaler::ScalerRecord;

async fn scaler_db() -> PvDatabase {
    let db = PvDatabase::new();
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 4;
    rec.init_record(1).unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();
    db
}

async fn watch(db: &PvDatabase, field: &str, sid: u32, mask: EventMask) -> EventReader {
    let rec = db.get_record("SCAL").unwrap();
    let mut inst = rec.write();
    inst.add_subscriber(field, sid, DbFieldType::Long, mask.bits())
        .expect("subscription must be accepted")
}

async fn process(db: &PvDatabase) {
    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("SCAL", &mut visited, 0)
        .await
        .unwrap();
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// Hardware reported new counts and acquisition complete — the dset `read()`
/// filling `s[]` plus the dset `done()` return (`scalerRecord.c:366`).
async fn hardware_finishes_with(db: &PvDatabase, counts: [u32; 4]) {
    let rec = db.get_record("SCAL").unwrap();
    let mut inst = rec.write();
    let scal = inst
        .record
        .as_any_mut()
        .unwrap()
        .downcast_mut::<ScalerRecord>()
        .unwrap();
    scal.s[..4].copy_from_slice(&counts);
    scal.set_done();
}

#[tokio::test]
async fn r12_62_completion_cycle_posts_sn_with_both_value_and_log() {
    let db = scaler_db().await;
    // Two separate subscribers on S1: one archiver (DBE_LOG only), one
    // value client (DBE_VALUE only). C's completion cycle serves both.
    let mut s1_log = watch(&db, "S1", 1, EventMask::LOG).await;
    let mut s1_value = watch(&db, "S1", 2, EventMask::VALUE).await;

    // CNT is pp(TRUE): the put starts the count (ss = COUNTING). No
    // monitor() runs on a counting cycle, so no idle sweep here.
    db.put_record_field_from_ca("SCAL", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    drain(&mut s1_log);
    drain(&mut s1_value);

    // The completion cycle: counts moved AND ss lands on IDLE.
    hardware_finishes_with(&db, [4242, 0, 0, 0]).await;
    process(&db).await;

    assert_eq!(
        drain(&mut s1_value),
        1,
        "updateCounts() posts the changed S1 with DBE_VALUE (scalerRecord.c:582)"
    );
    assert_eq!(
        drain(&mut s1_log),
        1,
        "monitor()'s sweep posts the SAME S1 with DBE_LOG on that same cycle \
         (scalerRecord.c:771) — the archiver must receive the final counts"
    );
}

/// The sweep half alone, unchanged: an idle cycle with no count movement still
/// posts every `Sn` with `DBE_LOG` and nothing with `DBE_VALUE`.
#[tokio::test]
async fn r12_62_idle_cycle_still_posts_log_only() {
    let db = scaler_db().await;
    let mut s1_log = watch(&db, "S1", 1, EventMask::LOG).await;
    let mut s1_value = watch(&db, "S1", 2, EventMask::VALUE).await;

    process(&db).await;

    assert_eq!(
        drain(&mut s1_log),
        1,
        "an idle process runs monitor() → one DBE_LOG per active channel"
    );
    assert_eq!(
        drain(&mut s1_value),
        0,
        "S1 did not move — updateCounts() posts no DBE_VALUE"
    );
}
