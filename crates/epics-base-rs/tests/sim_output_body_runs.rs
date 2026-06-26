//! SIMM (simulation mode) on an OUTPUT record must still run the record's
//! process() BODY — it replaces only the device write, not the body.
//!
//! C handles simulation inside `writeValue()`, which runs at the END of
//! `process()`: the body first computes the output (OROC rate-limiting in
//! `aoRecord.c::convert`, the bo HIGH momentary-reset state machine in
//! `boRecord.c::process`), and only the final `dbPutLink(&prec->siol, ...,
//! &prec->oval)` is substituted for the real device write. The pre-fix port
//! short-circuited a simulated record before its body, so output records lost
//! every body side effect in SIMM mode: a `bo` with `HIGH > 0` never armed its
//! momentary reset, and an `ao` with `OROC` wrote the un-rate-limited VAL
//! instead of OVAL.
//!
//! These tests drive an OUTPUT record in SIMM=YES synchronous mode (default
//! `SDLY = -1.0`) and assert the body ran by observing a body-only effect in
//! the SIOL target:
//!   * `ao` OROC: the SIOL target receives the OROC-limited OVAL, not VAL.
//!   * `bo` HIGH: the momentary reset arms and (on the timer reprocess) drives
//!     the simulated output back to 0.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::EpicsValue;

/// `ao` with `OROC` (output rate-of-change limit) in SIMM=YES mode: the body's
/// `convert()` rate-limits OVAL toward VAL by at most OROC per cycle, and the
/// simulated write sends OVAL to SIOL. Starting OVAL=0, VAL=10, OROC=1 → the
/// body computes OVAL=1, so the SIOL target receives 1 — NOT the un-limited
/// VAL=10. The pre-fix short-circuit skipped `convert()`, leaving OVAL=0 and
/// writing VAL=10 straight through, so this assertion pins the body running.
#[tokio::test]
async fn sim_output_runs_ao_oroc_body_writes_limited_oval_to_siol() {
    let db = PvDatabase::new();
    // SIML reads 1 -> SIMM=YES; SIOL target starts at a sentinel.
    db.add_record("AOROC_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("AOROC_TGT", Box::new(AoRecord::new(-99.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(10.0); // VAL=10
    ao.siml = "AOROC_SW".to_string();
    ao.siol = "AOROC_TGT".to_string();
    ao.oroc = 1.0; // rate limit: at most 1.0 EGU per cycle
    // oval starts 0.0; sdly left at the default -1.0 (synchronous sim write).
    db.add_record("AOROC", Box::new(ao)).await.unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("AOROC", &mut v1, 0)
        .await
        .unwrap();

    // Body ran: OROC limited OVAL from 0 toward 10 by 1 -> OVAL == 1.
    let oval = db.get_pv("AOROC.OVAL").await.unwrap();
    assert!(
        matches!(oval, EpicsValue::Double(v) if (v - 1.0).abs() < 1e-10),
        "ao body ran in SIMM mode: OROC limited OVAL to 1, got {oval:?}"
    );

    // The simulated write sent the OROC-limited OVAL (1), not the raw VAL (10),
    // to the SIOL target. Pre-fix wrote VAL=10 (body skipped).
    let tgt = db.get_pv("AOROC_TGT").await.unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v - 1.0).abs() < 1e-10),
        "SIOL target received the OROC-limited OVAL=1, not the un-limited VAL=10, got {tgt:?}"
    );
}

/// `bo` with `HIGH > 0` in SIMM=YES mode: the body's HIGH state machine arms a
/// one-shot momentary reset (C `boRecord.c::process` schedules a reprocess that
/// drives VAL back to 0). The first cycle writes VAL=1 to SIOL and arms the
/// reset; the reset reprocess (driven here directly, the timer being unfireable)
/// runs the body again, sees the pending reset, sets VAL=0, and writes 0 to
/// SIOL. The pre-fix short-circuit never ran the body, so the reset never armed
/// and the simulated output stayed latched at 1.
#[tokio::test]
async fn sim_output_runs_bo_high_momentary_reset_in_sim_mode() {
    let db = PvDatabase::new();
    db.add_record("BOHI_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    // Target starts at a sentinel distinct from both 0 and 1.
    db.add_record("BOHI_TGT", Box::new(AoRecord::new(-99.0)))
        .await
        .unwrap();

    let mut bo = BoRecord::new(1); // VAL=1
    bo.siml = "BOHI_SW".to_string();
    bo.siol = "BOHI_TGT".to_string();
    bo.high = 100.0; // momentary: hold high then reset (timer unfireable here)
    // sdly left at the default -1.0 (synchronous sim write).
    db.add_record("BOHI", Box::new(bo)).await.unwrap();

    // Fresh cycle: body runs, writes VAL=1 to SIOL, and arms the HIGH reset.
    let mut v1 = HashSet::new();
    db.process_record_with_links("BOHI", &mut v1, 0)
        .await
        .unwrap();

    let tgt = db.get_pv("BOHI_TGT").await.unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v - 1.0).abs() < 1e-10),
        "SIOL target received the simulated bo output VAL=1, got {tgt:?}"
    );

    // HIGH reset reprocess (the one-shot the body armed): the body runs again,
    // drives VAL back to 0, and the simulated write sends 0 to SIOL. Pre-fix
    // the body never ran, so the reset never armed and the target stayed at 1.
    let mut v2 = HashSet::new();
    db.process_record_continuation("BOHI", &mut v2, 0)
        .await
        .unwrap();

    let val = db.get_pv("BOHI").await.unwrap();
    assert!(
        matches!(val, EpicsValue::Enum(0) | EpicsValue::Short(0)),
        "bo HIGH momentary reset ran in SIMM mode: VAL back to 0, got {val:?}"
    );
    let tgt = db.get_pv("BOHI_TGT").await.unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if v.abs() < 1e-10),
        "SIOL target driven back to 0 by the HIGH momentary reset in sim mode, got {tgt:?}"
    );
}

/// Output IVOA veto parity: a SIMM=YES `ao` whose simulation severity SIMS is
/// INVALID raises SIMM_ALARM to INVALID; with IVOA = "Don't drive outputs" C
/// skips `writeValue` entirely (the SIOL write included), so the SIOL target is
/// NOT updated. The redirect runs through the same `skip_out` IVOA gate as a
/// real device write, so the simulated write is suppressed identically.
#[tokio::test]
async fn sim_output_ivoa_dont_drive_suppresses_siol_write() {
    let db = PvDatabase::new();
    db.add_record("AOIV_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("AOIV_TGT", Box::new(AoRecord::new(-99.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(5.0);
    ao.siml = "AOIV_SW".to_string();
    ao.siol = "AOIV_TGT".to_string();
    ao.sims = 3; // SIMM severity = INVALID
    ao.ivoa = 1; // Don't drive outputs
    db.add_record("AOIV", Box::new(ao)).await.unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("AOIV", &mut v1, 0)
        .await
        .unwrap();

    // SIMM_ALARM raised to INVALID.
    let sevr = db.get_pv("AOIV.SEVR").await.unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(3)),
        "SIMM_ALARM raised to INVALID from SIMS, got {sevr:?}"
    );
    // IVOA=Don't_drive vetoed the (simulated) write: target untouched.
    let tgt = db.get_pv("AOIV_TGT").await.unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v + 99.0).abs() < 1e-10),
        "IVOA Don't_drive suppressed the simulated SIOL write (target untouched), got {tgt:?}"
    );
}
