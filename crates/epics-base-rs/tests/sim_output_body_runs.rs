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

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON suite.

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

    let mut ao = AoRecord::new(0.0);
    ao.siml = "AOROC_SW".to_string();
    ao.siol = "AOROC_TGT".to_string();
    ao.oroc = 1.0; // rate limit: at most 1.0 EGU per cycle
    // sdly left at the default -1.0 (synchronous sim write).
    db.add_record("AOROC", Box::new(ao)).await.unwrap();

    // VAL=10 arrives as a PUT, not as the record's initial value: C's ao
    // `init_record` tail seeds `oval = pval = val` (aoRecord.c:156, softIoc:
    // `field(VAL,"10")` comes up with OVAL=10), so an initial VAL of 10 would
    // leave nothing for OROC to rate-limit.
    db.put_pv("AOROC.VAL", EpicsValue::Double(10.0))
        .await
        .unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("AOROC", &mut v1, 0)
        .await
        .unwrap();

    // Body ran: OROC limited OVAL from 0 toward 10 by 1 -> OVAL == 1.
    let oval = db.get_pv("AOROC.OVAL").unwrap();
    assert!(
        matches!(oval, EpicsValue::Double(v) if (v - 1.0).abs() < 1e-10),
        "ao body ran in SIMM mode: OROC limited OVAL to 1, got {oval:?}"
    );

    // The simulated write sent the OROC-limited OVAL (1), not the raw VAL (10),
    // to the SIOL target. Pre-fix wrote VAL=10 (body skipped).
    let tgt = db.get_pv("AOROC_TGT").unwrap();
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

    let tgt = db.get_pv("BOHI_TGT").unwrap();
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

    let val = db.get_pv("BOHI").unwrap();
    assert!(
        matches!(val, EpicsValue::Enum(0) | EpicsValue::Short(0)),
        "bo HIGH momentary reset ran in SIMM mode: VAL back to 0, got {val:?}"
    );
    let tgt = db.get_pv("BOHI_TGT").unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if v.abs() < 1e-10),
        "SIOL target driven back to 0 by the HIGH momentary reset in sim mode, got {tgt:?}"
    );
}

/// IVOA is decided from the record's OWN alarm severity, NOT from a
/// `SIMS=INVALID` simulation severity. C `aoRecord.c::process` evaluates the
/// IVOA gate `if (prec->nsev < INVALID_ALARM)` (:197) using the severity
/// `checkAlarms` produced, and only THEN does `writeValue` raise
/// `recGblSetSevr(SIMM_ALARM, sims)` (:582). So a SIMM=YES `ao` with a finite,
/// in-range VAL (`nsev < INVALID` at the gate) takes the normal `writeValue`
/// branch and DOES write OVAL to SIOL, even though SIMS=INVALID makes the
/// final SEVR=INVALID. The IVOA Don't_drive veto is never consulted.
#[tokio::test]
async fn sim_output_sims_invalid_does_not_veto_siol_write() {
    let db = PvDatabase::new();
    db.add_record("AOIV_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("AOIV_TGT", Box::new(AoRecord::new(-99.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(5.0); // finite, in range -> own alarm NoAlarm
    ao.siml = "AOIV_SW".to_string();
    ao.siol = "AOIV_TGT".to_string();
    ao.sims = 3; // SIMM severity = INVALID
    ao.ivoa = 1; // Don't drive outputs — must NOT fire (own nsev is NoAlarm)
    db.add_record("AOIV", Box::new(ao)).await.unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("AOIV", &mut v1, 0)
        .await
        .unwrap();

    // SIMM_ALARM makes the committed SEVR INVALID...
    let sevr = db.get_pv("AOIV.SEVR").unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(3)),
        "SIMM_ALARM raised the committed SEVR to INVALID, got {sevr:?}"
    );
    // ...but the IVOA gate saw the record's own NoAlarm severity, so C writes
    // OVAL to SIOL. The veto must NOT have fired.
    let tgt = db.get_pv("AOIV_TGT").unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v - 5.0).abs() < 1e-10),
        "SIMS=INVALID did NOT trigger the IVOA veto; OVAL=5 written to SIOL, got {tgt:?}"
    );
}

/// The IVOA Don't_drive veto DOES fire when the record's OWN alarm is INVALID.
/// A `bo` with `OSV=INVALID` and `VAL=1` raises STATE_ALARM/INVALID in
/// `checkAlarms`, so the IVOA gate (`nsev == INVALID`) selects Don't_drive and
/// C skips `writeValue` — no SIOL write, and (because `writeValue` is skipped)
/// SIMM_ALARM is never raised, so STAT stays STATE_ALARM. This guards that the
/// switch to the pre-SIMM `real_sev` did not disable the genuine veto.
#[tokio::test]
async fn sim_output_real_invalid_alarm_ivoa_dont_drive_suppresses() {
    use epics_base_rs::server::records::bo::BoRecord;

    const STATE_ALARM: i16 = 7;
    let db = PvDatabase::new();
    db.add_record("BOIV_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("BOIV_TGT", Box::new(AoRecord::new(-99.0)))
        .await
        .unwrap();

    let mut bo = BoRecord::new(1);
    bo.siml = "BOIV_SW".to_string();
    bo.siol = "BOIV_TGT".to_string();
    bo.osv = 3; // one-state severity = INVALID -> real STATE_ALARM/INVALID
    bo.sims = 0; // isolate: INVALID comes from the record's own alarm
    bo.ivoa = 1; // Don't drive outputs
    db.add_record("BOIV", Box::new(bo)).await.unwrap();

    // Define VAL so the record is not UDF: a bare bo stays UDF=1 and
    // `boRecord.c:371` raises UDF_ALARM/INVALID first, which (severity INVALID)
    // would dominate the record's own INVALID STATE alarm this test is about.
    // A full CA put clears UDF, as `caput BOIV 1` would — same pattern as
    // `sim_output_simm_alarm_loses_stat_on_severity_tie`.
    db.put_pv_and_post("BOIV.VAL", EpicsValue::Enum(1))
        .await
        .unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("BOIV", &mut v1, 0)
        .await
        .unwrap();

    let sevr = db.get_pv("BOIV.SEVR").unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(3)),
        "record's own STATE_ALARM/INVALID, got {sevr:?}"
    );
    let stat = db.get_pv("BOIV.STAT").unwrap();
    assert!(
        matches!(stat, EpicsValue::Short(STATE_ALARM)),
        "STAT is the record's STATE_ALARM (writeValue/SIMM_ALARM skipped under Don't_drive), got {stat:?}"
    );
    let tgt = db.get_pv("BOIV_TGT").unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v + 99.0).abs() < 1e-10),
        "IVOA Don't_drive on a real INVALID alarm suppressed the SIOL write, got {tgt:?}"
    );
}

/// SIMM_ALARM is raised AFTER `checkAlarms` for outputs (C `writeValue` runs at
/// process end), so on a severity TIE the record's own limit/state alarm — set
/// first — owns STAT/AMSG (`recGblSetSevr` is strict-greater). A `bo` with
/// `OSV=MINOR`, `VAL=1`, `SIMS=MINOR` must report STAT=STATE_ALARM, not
/// STAT=SIMM_ALARM. (Pre-fix the SIMM raise preceded `checkAlarms`, so SIMM won
/// the tie and STAT was wrongly SIMM_ALARM.)
#[tokio::test]
async fn sim_output_simm_alarm_loses_stat_on_severity_tie() {
    use epics_base_rs::server::records::bo::BoRecord;

    const STATE_ALARM: i16 = 7;
    let db = PvDatabase::new();
    db.add_record("BOTIE_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("BOTIE_TGT", Box::new(AoRecord::new(-99.0)))
        .await
        .unwrap();

    let mut bo = BoRecord::new(1);
    bo.siml = "BOTIE_SW".to_string();
    bo.siol = "BOTIE_TGT".to_string();
    bo.osv = 1; // one-state severity = MINOR -> STATE_ALARM/MINOR (set first)
    bo.sims = 1; // SIMM severity = MINOR -> ties; must NOT override STAT
    db.add_record("BOTIE", Box::new(bo)).await.unwrap();

    // A bare bo stays UDF=1 until a value SOURCE defines it — `boRecord.c:371`
    // raises UDF_ALARM/INVALID on every process otherwise, which (severity
    // INVALID) would dominate the MINOR state/SIMM tie this test is about. Model
    // the client setpoint that defines VAL: a full CA put clears UDF in `dbPut`
    // (isValueField), exactly as `caput BOTIE 1` would. `bo` no longer re-derives
    // UDF from the stored VAL every cycle (it clears only on a source), matching
    // C, so the definition must be explicit rather than implied by `new(1)`.
    // (`put_pv` alone does not clear UDF — only the post-driving CA-put path does.)
    db.put_pv_and_post("BOTIE.VAL", EpicsValue::Enum(1))
        .await
        .unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("BOTIE", &mut v1, 0)
        .await
        .unwrap();

    let sevr = db.get_pv("BOTIE.SEVR").unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(1)),
        "tied MINOR severity, got {sevr:?}"
    );
    let stat = db.get_pv("BOTIE.STAT").unwrap();
    assert!(
        matches!(stat, EpicsValue::Short(STATE_ALARM)),
        "on a tie the record's STATE_ALARM owns STAT (SIMM raised after checkAlarms), got {stat:?}"
    );
}
