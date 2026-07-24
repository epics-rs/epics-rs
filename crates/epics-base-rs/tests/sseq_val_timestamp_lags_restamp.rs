//! sseq restamps `TIME` AFTER the VAL monitor post, not before it.
//!
//! C `sseqRecord.c::asyncFinish` ends every process cycle with
//!
//! ```c
//! if (MonitorMask) db_post_events(pR, &pR->val, MonitorMask); // :474
//! ...
//! recGblFwdLink(pR);                                          // :499
//! recGblGetTimeStamp(pR);                                     // :501
//! ...
//! db_post_events(pR, &pR->busy, MonitorMask);                 // :505
//! ```
//!
//! so the VAL event is posted while the record still holds its PREVIOUS
//! timestamp, and `recGblGetTimeStamp` restamps only afterwards — for the
//! BUSY post and the next cycle. Unlike every standard record
//! (`aoRecord.c:190` stamps ahead of the value post) and unlike the base
//! `seq` (`seqRecord.c:224` restamps BEFORE the `:229` VAL post), sseq's VAL
//! monitor timestamp therefore lags exactly one completion behind. The port
//! previously restamped ahead of the VAL post (the framework's uniform
//! pre-output `apply_timestamp`), so the first VAL event carried wall-clock
//! "now" instead of the pre-update time the differential oracle observed from
//! C's QSRV2 monitor.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue, WallTime};

fn full() -> u16 {
    (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits()
}

/// A bare sseq with no links: a put to `VAL` starts and completes a sequence
/// with no active step synchronously, so the `asyncFinish` VAL post lands
/// before the put returns.
async fn bare_sseq() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SQV", Box::new(SseqRecord::new()))
        .await
        .unwrap();
    db
}

/// The first VAL monitor event carries the record's PRE-update timestamp, and
/// the second cycle's VAL carries the timestamp the FIRST cycle restamped to —
/// the one-completion lag C's `asyncFinish` produces (VAL at `:474` before
/// `recGblGetTimeStamp` at `:501`).
#[epics_macros_rs::epics_test]
async fn sseq_val_timestamp_is_pre_restamp_and_lags_one_cycle() {
    let db = bare_sseq().await;
    let inst = db.get_record("SQV").unwrap();

    // Inject a distinguishable sentinel so the VAL monitor timestamp is
    // comparable bit-for-bit and cannot be confused with wall-clock "now": a
    // fixed instant well in the past (1970-01-12), which the deferred restamp
    // must move forward.
    let sentinel = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    inst.write().common.time = sentinel;

    let mut val_rx = inst
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Long, full())
        .unwrap();

    // Cycle 1.
    db.put_record_field_from_ca("SQV", "VAL", EpicsValue::Long(1))
        .await
        .unwrap();
    let ev1 = val_rx.try_recv().expect("cycle 1 posts a VAL event");
    let t_after_1 = inst.read().common.time;

    // C posts VAL (sseqRecord.c:474) BEFORE recGblGetTimeStamp (:501), so the
    // first VAL carries the pre-update timestamp — the sentinel, untouched.
    assert_eq!(
        ev1.snapshot.timestamp,
        WallTime::from(sentinel),
        "the first VAL event must carry the record's pre-update timestamp \
         (asyncFinish posts VAL at :474 before the :501 restamp), not \
         wall-clock now"
    );
    // The deferred restamp (C recGblGetTimeStamp :501) then advanced TIME off
    // the sentinel, for the BUSY post and the next cycle.
    assert!(
        WallTime::from(t_after_1) > WallTime::from(sentinel),
        "the completion must restamp TIME forward off the sentinel after the \
         VAL post (asyncFinish :501)"
    );

    // Cycle 2: VAL carries the timestamp cycle 1 restamped to — the
    // one-completion lag, proving VAL reads the record's carried-over TIME and
    // is NOT stamped fresh at its own post.
    db.put_record_field_from_ca("SQV", "VAL", EpicsValue::Long(2))
        .await
        .unwrap();
    let ev2 = val_rx.try_recv().expect("cycle 2 posts a VAL event");
    assert_eq!(
        ev2.snapshot.timestamp,
        WallTime::from(t_after_1),
        "the second VAL event must carry the timestamp the FIRST cycle \
         restamped to (asyncFinish's one-completion lag), not a fresh stamp"
    );
    assert_eq!(
        ev2.snapshot.value,
        EpicsValue::Long(2),
        "cycle 2's VAL value is the newly put 2"
    );
}
