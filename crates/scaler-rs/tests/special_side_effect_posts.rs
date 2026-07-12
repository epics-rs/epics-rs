//! R10-62 — the monitor posts C's `special()` makes inline.
//!
//! None of TP / PR1..PR64 / G1..G64 / RATE is `pp(TRUE)` in `scalerRecord.dbd`,
//! so a put to any of them does NOT process the record. The `db_post_events`
//! calls inside C's `special()` are therefore the ONLY source of those monitor
//! events, and every one of them uses a literal `DBE_VALUE`:
//!
//! ```c
//! case scalerRecordTP:                        /* :670-677 */
//!     pscal->pr1 = (epicsUInt32)(pscal->tp * pscal->freq);
//!     db_post_events(pscal,&(pscal->pr1),DBE_VALUE);
//!     pscal->d1 = pscal->g1 = 1;
//!     db_post_events(pscal,&(pscal->d1),DBE_VALUE);
//!     db_post_events(pscal,&(pscal->g1),DBE_VALUE);
//!     break;
//! case scalerRecordRATE:                      /* :690-693 */
//!     pscal->rate = MIN(60.,MAX(0.,pscal->rate));
//!     db_post_events(pscal,&(pscal->tp),DBE_VALUE);
//!     break;
//! ```
//!
//! The RATE case posts **TP** — a field the write never touched. It is a
//! copy-paste of the TP case's post, and it is what a `caput scaler.RATE`
//! observably does on a C IOC, so it is the contract (R10-62).
//!
//! The port's `special()` performed each case's field mutations but declared no
//! `monitor_side_effect_fields`, so NONE of these posts fired: a `camonitor
//! scaler.PR1` saw nothing when TP was written, and a `camonitor scaler.TP` saw
//! nothing when RATE was written.

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
    rec.init_record(16).unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();
    db
}

/// Subscribe to one field with an explicit event mask.
async fn watch(
    db: &PvDatabase,
    field: &str,
    sid: u32,
    dbf: DbFieldType,
    mask: EventMask,
) -> EventReader {
    let rec = db.get_record("SCAL").await.unwrap();
    let mut inst = rec.write().await;
    inst.add_subscriber(field, sid, dbf, mask.bits())
        .expect("subscription must be accepted")
}

async fn caput(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca("SCAL", field, value)
        .await
        .unwrap();
}

/// The cited case: writing RATE posts TP, which the write did not change.
#[tokio::test]
async fn r10_62_a_rate_write_posts_tp() {
    let db = scaler_db().await;
    let mut tp_rx = watch(&db, "TP", 1, DbFieldType::Double, EventMask::VALUE).await;

    caput(&db, "RATE", EpicsValue::Float(5.0)).await;

    let event = tp_rx
        .try_recv()
        .expect("scalerRecord.c:692 posts TP on every RATE write");
    assert_eq!(
        event.snapshot.value.to_f64(),
        Some(1.0),
        "the event carries TP's UNCHANGED value — C posts the field it never wrote"
    );
}

/// C's post is a literal `DBE_VALUE`, so a `DBE_LOG`-only (archiver) subscriber
/// gets nothing from a RATE write.
#[tokio::test]
async fn r10_62_a_rate_write_posts_tp_with_dbe_value_only() {
    let db = scaler_db().await;
    let mut tp_log_rx = watch(&db, "TP", 2, DbFieldType::Double, EventMask::LOG).await;

    caput(&db, "RATE", EpicsValue::Float(5.0)).await;

    assert!(
        tp_log_rx.try_recv().is_err(),
        "db_post_events(&pscal->tp, DBE_VALUE) carries no LOG bit (:692)"
    );
}

/// Family: the TP case posts PR1, D1 and G1 (C:673-676), unconditionally.
#[tokio::test]
async fn r10_62_a_tp_write_posts_pr1_d1_and_g1() {
    let db = scaler_db().await;
    let mut pr1_rx = watch(&db, "PR1", 3, DbFieldType::ULong, EventMask::VALUE).await;
    let mut d1_rx = watch(&db, "D1", 4, DbFieldType::Short, EventMask::VALUE).await;
    let mut g1_rx = watch(&db, "G1", 5, DbFieldType::Short, EventMask::VALUE).await;

    caput(&db, "TP", EpicsValue::Double(2.0)).await;

    let pr1 = pr1_rx.try_recv().expect("C:673 posts PR1");
    assert_eq!(
        pr1.snapshot.value.to_f64(),
        Some(2.0e7),
        "PR1 = TP * FREQ (2 s at 10 MHz)"
    );
    assert!(d1_rx.try_recv().is_ok(), "C:675 posts D1");
    assert!(g1_rx.try_recv().is_ok(), "C:676 posts G1");
}

/// Family: a PRn write that forces the channel on posts that channel's Dn/Gn
/// (C:702-706). Uses channel 5 to prove the post is per-channel, not PR1-only.
#[tokio::test]
async fn r10_62_a_preset_write_posts_that_channels_gate_and_direction() {
    let db = scaler_db().await;
    let mut d5_rx = watch(&db, "D5", 6, DbFieldType::Short, EventMask::VALUE).await;
    let mut g5_rx = watch(&db, "G5", 7, DbFieldType::Short, EventMask::VALUE).await;

    caput(&db, "PR5", EpicsValue::Long(1000)).await;

    assert!(d5_rx.try_recv().is_ok(), "C:704 posts D5");
    let g5 = g5_rx.try_recv().expect("C:705 posts G5");
    assert_eq!(
        g5.snapshot.value.to_f64(),
        Some(1.0),
        "the gate C just forced on"
    );
}

/// Family: a Gn write that defaults an unset preset posts PRn (C:715-718).
#[tokio::test]
async fn r10_62_a_gate_write_posts_the_preset_it_defaults() {
    let db = scaler_db().await;
    let mut pr3_rx = watch(&db, "PR3", 8, DbFieldType::ULong, EventMask::VALUE).await;

    caput(&db, "G3", EpicsValue::Short(1)).await;

    let pr3 = pr3_rx.try_recv().expect("C:717 posts the defaulted PR3");
    assert_eq!(
        pr3.snapshot.value.to_f64(),
        Some(1000.0),
        "C:716 gives an unset preset the 1000-count default"
    );
}

/// ...but only when it actually defaulted it: C's guard is
/// `if (pgate[i] && (ppreset[i] == 0))`. A gate write over an existing preset
/// posts nothing.
#[tokio::test]
async fn r10_62_a_gate_write_over_an_existing_preset_posts_nothing() {
    let db = scaler_db().await;

    // Give PR3 a value first (this put's own posts are consumed below).
    caput(&db, "PR3", EpicsValue::Long(500)).await;
    let mut pr3_rx = watch(&db, "PR3", 9, DbFieldType::ULong, EventMask::VALUE).await;

    caput(&db, "G3", EpicsValue::Short(1)).await;

    assert!(
        pr3_rx.try_recv().is_err(),
        "ppreset[i] != 0, so C's guard skips both the default and the post"
    );
}

/// The list is per-put, not sticky: a put whose C case posts nothing must not
/// re-emit the previous put's posts.
#[tokio::test]
async fn r10_62_the_post_list_does_not_leak_into_the_next_put() {
    let db = scaler_db().await;

    caput(&db, "TP", EpicsValue::Double(2.0)).await; // posts PR1, D1, G1
    let mut pr1_rx = watch(&db, "PR1", 10, DbFieldType::ULong, EventMask::VALUE).await;

    // TP1 has no special() case in C at all — it posts nothing.
    caput(&db, "TP1", EpicsValue::Double(3.0)).await;

    assert!(
        pr1_rx.try_recv().is_err(),
        "the TP write's post list must not survive into the TP1 write"
    );
}
