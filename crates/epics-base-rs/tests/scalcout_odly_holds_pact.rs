//! scalcout/calcout ODLY holds PACT across the delay — C `calcoutRecord.c:282`
//! and `sCalcoutRecord.c:404`: the ODLY branch sets `dlya=1`, schedules the
//! delayed callback, and `return 0`s with `pact` still TRUE (set at entry), so
//! the record stays ACTIVE during the whole delay and a concurrent `dbProcess`
//! bails; the delayed callback re-enters (`pact==TRUE`, `dlya` branch) and
//! clears pact.
//!
//! Before the fix the ODLY delaying cycle returned `AsyncPendingNotify`, which
//! does NOT set `processing`/PACT, so a foreign `dbProcess` arriving during the
//! delay re-entered `process()` while `dlya==1` and ran the continuation branch
//! — firing the deferred OUT write early. The framework now holds PACT on a
//! notify that schedules a `ReprocessAfter` (the continuation that releases it
//! at the `is_continuation` arm), so the foreign process bails at the PACT
//! entry guard, exactly as C holds pact across the delay. This is the timer-ODLY
//! generalization of the swait PACT fix; motor's notify carries no
//! `ReprocessAfter` and so is untouched.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

#[tokio::test]
async fn scalcout_odly_holds_pact_foreign_process_does_not_fire_early() {
    let db = PvDatabase::new();

    // OUT target seeded 0.0 — must not be driven to OVAL until the delay
    // genuinely completes (the continuation).
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // scalcout: CALC="42" → VAL=42=OVAL (DOPT=Use_CALC default); OOPT=Every →
    // output due; ODLY=100s (the real timer cannot fire within the test);
    // OUT→TGT.
    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("42".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.oopt = 0;
    sc.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    sc.put_field("OUT", EpicsValue::String("TGT".into()))
        .unwrap();
    db.add_record("SC", Box::new(sc)).await.unwrap();

    // Delaying cycle: ODLY>0 defers, sets DLYA=1, OUT not written.
    let mut v1 = HashSet::new();
    db.process_record_with_links("SC", &mut v1, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("SC").unwrap().read().record.get_field("DLYA"),
        Some(EpicsValue::Short(1)),
        "ODLY>0 cycle sets DLYA and defers"
    );
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "OUT deferred on the ODLY delaying cycle"
    );

    // Foreign dbProcess DURING the delay (is_continuation=false): must BAIL at
    // the PACT entry guard, NOT re-enter process() while dlya==1 and fire the
    // deferred OUT early.
    let mut v2 = HashSet::new();
    db.process_record_with_links("SC", &mut v2, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "PACT held: a foreign dbProcess during the ODLY delay must NOT fire the \
         deferred OUT early (C calcoutRecord.c:282 holds pact across the delay)"
    );

    // Continuation (bypasses the PACT guard): fires the deferred output once.
    let mut v3 = HashSet::new();
    db.process_record_continuation("SC", &mut v3, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(42.0),
        "continuation drives OUT to OVAL=42 after the ODLY delay"
    );
}
