//! R10-63 — a scaler process cycle posts only the fields C's
//! `process()`/`monitor()` post.
//!
//! C's scaler makes exactly these `db_post_events` calls from a process cycle:
//! `CNT` (scalerRecord.c:372), `PR1` (:425), `TP` (:427), `FREQ` (:430,:530),
//! `VAL` (:478), `S1..Snch` (:582 and the idle `monitor()` sweep at :771) and
//! `T` (:588). Everything else it writes stays silent — most visibly the
//! gate→direction copy at `:413-414` (`pdir[i] = pgate[i]`), which C posts
//! nothing for; `Dn` is posted only from `special()`.
//!
//! The framework's generic rule ("post every subscribed field that changed
//! since its last post") invented `Dn` events on top of C: `Dn` is changed by
//! the count-start copy and never posted by C's process.
//! `Record::process_posted_fields` closes that by declaring C's set — a field
//! outside it is never posted by a process cycle.
//!
//! (`Gn`/`PRn` were once double-posted here too — posted by their own put, then
//! change-detected a second time on the next cycle. That was R11-C10, a
//! FRAMEWORK defect: the put-time post did not advance `last_posted`. It is
//! fixed at the framework — every value-class post now advances it — so this
//! hook no longer carries that load.)

// RTEMS-EXEC-MODEL-ALLOW(5): checked, not waived — all 5 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --all-features`, 112/112). scaler-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use scaler_rs::records::scaler::ScalerRecord;

/// A 16-channel scaler at 10 MHz with TP = 1 s.
async fn scaler_db() -> PvDatabase {
    let db = PvDatabase::new();
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 16;
    rec.init_record(1).unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();
    db
}

async fn watch(
    db: &PvDatabase,
    field: &str,
    sid: u32,
    dbf: DbFieldType,
    mask: EventMask,
) -> EventReader {
    let rec = db.get_record("SCAL").unwrap();
    let mut inst = rec.write();
    inst.add_subscriber(field, sid, dbf, mask.bits())
        .expect("subscription must be accepted")
}

async fn caput(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca("SCAL", field, value)
        .await
        .unwrap();
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

async fn field(db: &PvDatabase, f: &str) -> EpicsValue {
    let rec = db.get_record("SCAL").unwrap();
    let g = rec.read();
    g.record.get_field(f).unwrap()
}

/// The cited site: starting a count copies G2 into D2 (`scalerRecord.c:413-414`)
/// — the field MOVES, and C posts nothing for it.
#[tokio::test]
async fn r10_63_count_start_gate_to_direction_copy_posts_no_dn() {
    let db = scaler_db().await;
    let mut d2 = watch(&db, "D2", 1, DbFieldType::Short, EventMask::VALUE).await;

    caput(&db, "G2", EpicsValue::Short(1)).await;
    assert_eq!(drain(&mut d2), 0, "a G2 write does not touch D2");

    // CNT is pp(TRUE): the put processes the record, which runs the copy.
    caput(&db, "CNT", EpicsValue::Short(1)).await;

    assert_eq!(
        field(&db, "D2").await,
        EpicsValue::Short(1),
        "the copy ran — D2 took G2's value"
    );
    assert_eq!(
        drain(&mut d2),
        0,
        "C's :413-414 copy carries no db_post_events — the port must post no Dn"
    );
}

/// The other half: a put to Gn posts it ONCE (C `dbPut`), and the process cycle
/// that follows must not post it again — C's `process()` posts no `Gn` at all.
#[tokio::test]
async fn r10_63_a_gate_put_is_posted_once_not_again_on_the_next_process() {
    let db = scaler_db().await;
    let mut g2 = watch(&db, "G2", 2, DbFieldType::Short, EventMask::VALUE).await;

    caput(&db, "G2", EpicsValue::Short(1)).await;
    assert_eq!(drain(&mut g2), 1, "dbPut posts the field it wrote");

    caput(&db, "CNT", EpicsValue::Short(1)).await;
    assert_eq!(
        drain(&mut g2),
        0,
        "C's process() posts no Gn — the value was already published by the put"
    );
}

/// The internal state fields C never posts anywhere: SS / US / PCNT
/// (`scalerRecord.dbd:45-59`, all SPC_NOMOD; no `db_post_events` names them).
#[tokio::test]
async fn r10_63_internal_state_fields_are_not_posted() {
    let db = scaler_db().await;
    let mut ss = watch(&db, "SS", 3, DbFieldType::Short, EventMask::VALUE).await;
    let mut us = watch(&db, "US", 4, DbFieldType::Short, EventMask::VALUE).await;
    let mut pcnt = watch(&db, "PCNT", 5, DbFieldType::Short, EventMask::VALUE).await;

    caput(&db, "CNT", EpicsValue::Short(1)).await;

    assert_eq!(
        field(&db, "SS").await,
        EpicsValue::Short(2),
        "the record did start counting (SS moved)"
    );
    assert_eq!(drain(&mut ss), 0, "C posts no SS");
    assert_eq!(drain(&mut us), 0, "C posts no US");
    assert_eq!(drain(&mut pcnt), 0, "C posts no PCNT");
}

/// Negative control 1 — the fields C DOES post from a process cycle still post.
/// `Sn` is in the declared set, so the idle `monitor()` DBE_LOG sweep
/// (`scalerRecord.c:770-772`) survives the gate.
#[tokio::test]
async fn r10_63_the_idle_sn_log_sweep_still_posts() {
    let db = scaler_db().await;
    let mut s1 = watch(&db, "S1", 6, DbFieldType::Short, EventMask::LOG).await;

    process(&db).await;

    assert_eq!(
        drain(&mut s1),
        1,
        "S1 is in C's process post set — the idle DBE_LOG sweep must still fire"
    );
}

/// Negative control 2 — the `special()` side-effect posts are a different path
/// and must be untouched: a TP write still posts PR1/D1/G1 (C:673-676), even
/// though `Dn`/`Gn` are outside the process post set.
#[tokio::test]
async fn r10_63_special_side_effect_posts_still_fire() {
    let db = scaler_db().await;
    let mut d1 = watch(&db, "D1", 7, DbFieldType::Short, EventMask::VALUE).await;
    let mut g1 = watch(&db, "G1", 8, DbFieldType::Short, EventMask::VALUE).await;
    let mut pr1 = watch(&db, "PR1", 9, DbFieldType::ULong, EventMask::VALUE).await;

    caput(&db, "TP", EpicsValue::Double(2.0)).await;

    assert_eq!(drain(&mut d1), 1, "C:675 posts D1 from special()");
    assert_eq!(drain(&mut g1), 1, "C:676 posts G1 from special()");
    assert_eq!(drain(&mut pr1), 1, "C:673 posts PR1 from special()");
}
