//! R15-65 — dfanout's IVOA decision is made ONCE, before the push, and its
//! `IVOA=Set_output_to_IVOV` arm overwrites VAL.
//!
//! C `dfanoutRecord.c:127-146`:
//!
//! ```c
//! checkAlarms(prec);
//! if (prec->nsev < INVALID_ALARM)
//!     push_values(prec);
//! else switch (prec->ivoa) {
//!     case menuIvoaContinue_normally:    push_values(prec); break;
//!     case menuIvoaDon_t_drive_outputs:  break;
//!     case menuIvoaSet_output_to_IVOV:   prec->val = prec->ivov;
//!                                        push_values(prec); break;
//! }
//! monitor(prec);
//! ```
//!
//! Two facts follow, and the port broke the second:
//!
//! 1. `push_values` pushes `prec->val` (:309/321/331), and the IVOV arm
//!    ASSIGNS `prec->val` first — so the record's own VAL, and the VAL monitor
//!    `monitor()` posts, carry IVOV too.
//! 2. The severity tested is the one `checkAlarms` produced, read BEFORE any
//!    push. A `LINK_ALARM`/`INVALID` that a failed `dbPutLink` raises INSIDE
//!    `push_values` cannot feed back into the switch — C has already left it.
//!
//! The port re-derived the IVOA decision from `nsev` after the output stage had
//! begun, so a dfanout with no alarm of its own whose OUTn put simply FAILED
//! then had its VAL silently overwritten with IVOV. The decision now has a
//! single owner in `process_record_with_links_inner`, and every output path
//! (OUT, SIOL, the generic multi-output pairs, dfanout's push) consumes it.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

/// A dfanout at VAL=200 driving OUTA, with IVOV=9 and the given IVOA.
/// `invalid` adds HIHI=100/HHSV=INVALID, so the record's own checkAlarms puts
/// it at INVALID — the severity C's IVOA switch keys on.
async fn add_dfanout(db: &PvDatabase, ivoa: i16, invalid: bool, outa: &str) {
    let mut df = DfanoutRecord::new(200.0);
    df.put_field("IVOA", EpicsValue::Short(ivoa)).unwrap();
    df.put_field("IVOV", EpicsValue::Double(9.0)).unwrap();
    df.put_field("OUTA", EpicsValue::String(outa.into()))
        .unwrap();
    if invalid {
        df.put_field("HIHI", EpicsValue::Double(100.0)).unwrap();
        df.put_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
            .unwrap();
    }
    db.add_record("DF", Box::new(df)).await.unwrap();
    // VAL=200 stands for a `.db` `field(VAL,"200")`, and C's static write of
    // VAL also writes UDF=0 (`dbPutString`, dbStaticLib.c:2653-2660). dfanout's
    // `process()` never clears UDF, so without the seed the record would be
    // INVALID/UDF from `checkAlarms` on every cycle and the IVOA switch would
    // key on THAT — softIoc:
    // `record(dfanout,"DFBU"){field(SELM,"Specified") field(SELN,"99")}`
    // reports INVALID/UDF, while the same record with `field(VAL,"1")` reports
    // INVALID/SOFT.
    db.get_record("DF").unwrap().write().common.udf = 0;
}

/// Boundary 1 — INVALID cycle, IVOA=Set_output_to_IVOV: the targets get IVOV,
/// the record's own VAL becomes IVOV, and the VAL monitor fires with it.
#[tokio::test]
async fn r15_65_ivoa_ivov_overwrites_val_and_posts_it() {
    let db = PvDatabase::new();
    db.add_record("DF_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_dfanout(&db, 2, true, "DF_TGT").await;

    let mut val_rx = db
        .get_record("DF")
        .unwrap()
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .expect("a VAL subscription must be accepted");

    process(&db, "DF").await;

    assert_eq!(
        db.get_pv("DF").unwrap(),
        EpicsValue::Double(9.0),
        "IVOA=Set_output_to_IVOV assigns prec->val = prec->ivov \
         (dfanoutRecord.c:137) — VAL itself, not just the pushed copy"
    );
    assert_eq!(
        db.get_pv("DF_TGT").unwrap(),
        EpicsValue::Double(9.0),
        "push_values sends prec->val, which is now IVOV"
    );
    let posted = val_rx.try_recv().expect("VAL must post on the IVOV cycle");
    assert_eq!(
        posted.snapshot.value,
        EpicsValue::Double(9.0),
        "monitor() posts VAL after the IVOV assignment (dfanoutRecord.c:283-299)"
    );
}

/// Boundary 2 — the regression the shared IVOA owner closes. The dfanout has NO
/// alarm of its own; the only INVALID in the cycle is the one the FAILED OUTA
/// put raises from inside the push. C evaluated the IVOA switch before that put
/// and never returns to it, so VAL keeps its computed value.
#[tokio::test]
async fn r15_65_a_failed_push_does_not_retro_trigger_the_ivov_arm() {
    let db = PvDatabase::new();
    add_dfanout(&db, 2, false, "NO_SUCH_TARGET").await;

    process(&db, "DF").await;

    assert_eq!(
        db.get_record("DF").unwrap().read().common.sevr,
        AlarmSeverity::Invalid,
        "precondition: the failed OUTA put alarms the record (dbLink.c:444-446)"
    );
    assert_eq!(
        db.get_pv("DF").unwrap(),
        EpicsValue::Double(200.0),
        "the IVOA switch is decided on the checkAlarms severity BEFORE the push \
         (dfanoutRecord.c:128); an INVALID raised BY the push must not reach \
         back into it and overwrite VAL with IVOV"
    );
}

/// Boundary 3 — IVOA=Continue_normally on an INVALID cycle: VAL is untouched
/// and pushed as-is (dfanoutRecord.c:132-134).
#[tokio::test]
async fn r15_65_ivoa_continue_pushes_val_unchanged() {
    let db = PvDatabase::new();
    db.add_record("DF_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_dfanout(&db, 0, true, "DF_TGT").await;

    process(&db, "DF").await;

    assert_eq!(
        db.get_pv("DF").unwrap(),
        EpicsValue::Double(200.0),
        "Continue_normally leaves VAL alone"
    );
    assert_eq!(
        db.get_pv("DF_TGT").unwrap(),
        EpicsValue::Double(200.0),
        "…and pushes it"
    );
}

/// Boundary 4 — IVOA=Don't_drive_outputs on an INVALID cycle: no push at all
/// (dfanoutRecord.c:139), VAL untouched.
#[tokio::test]
async fn r15_65_ivoa_dont_drive_suppresses_the_push() {
    let db = PvDatabase::new();
    db.add_record("DF_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_dfanout(&db, 1, true, "DF_TGT").await;

    process(&db, "DF").await;

    assert_eq!(
        db.get_pv("DF").unwrap(),
        EpicsValue::Double(200.0),
        "Don't_drive leaves VAL alone"
    );
    assert_eq!(
        db.get_pv("DF_TGT").unwrap(),
        EpicsValue::Double(0.0),
        "…and drives nothing"
    );
}
