#![allow(clippy::field_reassign_with_default)]
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, CommonFields, Record};
use epics_base_rs::types::EpicsValue;
use std_rs::EpidRecord;
use std_rs::device_support::epid_soft::EpidSoftDeviceSupport;

// ============================================================
// Record basics
// ============================================================

#[test]
fn test_record_type() {
    let rec = EpidRecord::default();
    assert_eq!(rec.record_type(), "epid");
}

#[test]
fn test_default_values() {
    let rec = EpidRecord::default();
    assert_eq!(rec.val, 0.0);
    assert_eq!(rec.kp, 0.0);
    assert_eq!(rec.ki, 0.0);
    assert_eq!(rec.kd, 0.0);
    assert_eq!(rec.fmod, 0); // PID
    assert_eq!(rec.fbon, 0); // Off
    assert_eq!(rec.oval, 0.0);
    assert_eq!(rec.err, 0.0);
}

#[test]
fn test_as_any_mut() {
    let mut rec = EpidRecord::default();
    assert!(rec.as_any_mut().is_some());
}

// ============================================================
// Field access
// ============================================================

#[test]
fn test_get_put_val() {
    let mut rec = EpidRecord::default();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(50.0)));
}

#[test]
fn test_get_put_gains() {
    let mut rec = EpidRecord::default();
    rec.put_field("KP", EpicsValue::Double(1.0)).unwrap();
    rec.put_field("KI", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("KD", EpicsValue::Double(0.1)).unwrap();
    assert_eq!(rec.get_field("KP"), Some(EpicsValue::Double(1.0)));
    assert_eq!(rec.get_field("KI"), Some(EpicsValue::Double(0.5)));
    assert_eq!(rec.get_field("KD"), Some(EpicsValue::Double(0.1)));
}

#[test]
fn test_read_only_fields() {
    let mut rec = EpidRecord::default();
    assert!(rec.put_field("CVAL", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("OVAL", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("P", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("D", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("ERR", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("FBOP", EpicsValue::Short(1)).is_err());
}

#[test]
fn test_i_is_writable() {
    // I is writable for bumpless initialization
    let mut rec = EpidRecord::default();
    rec.put_field("I", EpicsValue::Double(5.0)).unwrap();
    assert_eq!(rec.get_field("I"), Some(EpicsValue::Double(5.0)));
}

#[test]
fn test_type_mismatch() {
    let mut rec = EpidRecord::default();
    assert!(
        rec.put_field("KP", EpicsValue::String("bad".into()))
            .is_err()
    );
    assert!(rec.put_field("FMOD", EpicsValue::Double(1.0)).is_err());
}

#[test]
fn test_unknown_field() {
    let rec = EpidRecord::default();
    assert!(rec.get_field("NONEXISTENT").is_none());
    let mut rec = rec;
    assert!(
        rec.put_field("NONEXISTENT", EpicsValue::Double(1.0))
            .is_err()
    );
}

#[test]
fn test_display_fields() {
    let mut rec = EpidRecord::default();
    rec.put_field("PREC", EpicsValue::Short(3)).unwrap();
    rec.put_field("EGU", EpicsValue::String("degC".into()))
        .unwrap();
    rec.put_field("HOPR", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LOPR", EpicsValue::Double(0.0)).unwrap();
    assert_eq!(rec.get_field("PREC"), Some(EpicsValue::Short(3)));
    assert_eq!(
        rec.get_field("EGU"),
        Some(EpicsValue::String("degC".into()))
    );
}

#[test]
fn test_alarm_fields() {
    let mut rec = EpidRecord::default();
    rec.put_field("HIHI", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("HIGH", EpicsValue::Double(80.0)).unwrap();
    rec.put_field("LOW", EpicsValue::Double(20.0)).unwrap();
    rec.put_field("LOLO", EpicsValue::Double(0.0)).unwrap();
    rec.put_field("HHSV", EpicsValue::Short(2)).unwrap();
    rec.put_field("HYST", EpicsValue::Double(1.0)).unwrap();
    assert_eq!(rec.get_field("HIHI"), Some(EpicsValue::Double(100.0)));
    assert_eq!(rec.get_field("HHSV"), Some(EpicsValue::Short(2)));
    assert_eq!(rec.get_field("HYST"), Some(EpicsValue::Double(1.0)));
}

// ============================================================
// Alarm logic
// ============================================================

#[test]
fn test_check_alarms_hihi() {
    let mut rec = EpidRecord::default();
    rec.hihi = 100.0;
    rec.hhsv = 2; // MAJOR
    rec.val = 105.0;
    let alarm = rec.check_alarms();
    assert!(alarm.is_some());
    let (status, severity, alev) = alarm.unwrap();
    assert_eq!(status, alarm_status::HIHI_ALARM);
    assert_eq!(severity, AlarmSeverity::Major);
    assert_eq!(alev, 100.0);
}

#[test]
fn test_check_alarms_lolo() {
    let mut rec = EpidRecord::default();
    rec.lolo = 10.0;
    rec.llsv = 2;
    rec.val = 5.0;
    let alarm = rec.check_alarms();
    assert!(alarm.is_some());
    let (status, severity, alev) = alarm.unwrap();
    assert_eq!(status, alarm_status::LOLO_ALARM);
    assert_eq!(severity, AlarmSeverity::Major);
    assert_eq!(alev, 10.0);
}

#[test]
fn test_check_alarms_no_alarm() {
    let mut rec = EpidRecord::default();
    rec.hihi = 100.0;
    rec.high = 80.0;
    rec.low = 20.0;
    rec.lolo = 10.0;
    rec.hhsv = 2;
    rec.hsv = 1;
    rec.lsv = 1;
    rec.llsv = 2;
    rec.val = 50.0; // In normal range
    let alarm = rec.check_alarms();
    assert!(alarm.is_none());
}

#[test]
fn test_check_alarms_hysteresis() {
    let mut rec = EpidRecord::default();
    rec.hihi = 100.0;
    rec.hhsv = 2;
    rec.hyst = 5.0;

    // First alarm triggers at 100. The trait hook commits LALM to the
    // alarmed threshold only after `recGblSetSevr` raises the severity.
    rec.val = 100.0;
    let mut common = CommonFields::default();
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(rec.lalm, 100.0);

    // Value drops but still within hysteresis band
    rec.val = 96.0;
    let alarm = rec.check_alarms();
    assert!(alarm.is_some(), "Should still alarm within hysteresis band");

    // Value drops below hysteresis band
    rec.val = 94.0;
    let alarm = rec.check_alarms();
    assert!(alarm.is_none(), "Should clear alarm below hysteresis band");
}

/// Regression: C `aiRecord.c:403-406` gates `prec->lalm = alev` on
/// `recGblSetSevr` actually raising the severity. A lower-severity epid
/// alarm that loses to an already-higher pending severity must NOT
/// advance LALM — otherwise the hysteresis band silently re-bases.
#[test]
fn test_check_alarms_lalm_gated_on_severity_raise() {
    let mut rec = EpidRecord::default();
    rec.high = 80.0;
    rec.hsv = 1; // MINOR
    rec.val = 85.0;
    let lalm_before = rec.lalm;

    let mut common = CommonFields::default();
    // Pre-seed a higher pending severity, as an upstream MS link would.
    common.nsev = AlarmSeverity::Invalid;
    common.nsta = alarm_status::COMM_ALARM;

    Record::check_alarms(&mut rec, &mut common);

    // The HIGH alarm lost the maximize-severity race, so it raised
    // nothing — LALM must stay where it was.
    assert_eq!(common.nsev, AlarmSeverity::Invalid);
    assert_eq!(
        rec.lalm, lalm_before,
        "LALM must not advance when the alarm did not raise the severity"
    );

    // A HIGH alarm that DOES raise the severity advances LALM to HIGH.
    let mut rec = EpidRecord::default();
    rec.high = 80.0;
    rec.hsv = 1; // MINOR
    rec.val = 85.0;
    let mut common = CommonFields::default();
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(common.nsev, AlarmSeverity::Minor);
    assert_eq!(
        rec.lalm, 80.0,
        "LALM must advance to the alarmed threshold when the severity was raised"
    );
}

/// Regression: the `Record::check_alarms` trait hook (the one the
/// framework actually invokes after `process()`) must apply the
/// computed severity/status to the record's pending alarm state.
/// Before the fix, `epid::process` called the inherent `check_alarms`
/// and discarded its return, so SEVR/STAT never moved — HIHI/HIGH/
/// LOW/LOLO limits were dead.
#[test]
fn test_check_alarms_hook_applies_severity() {
    // Crossing HIGH raises the configured HIGH severity/status.
    let mut rec = EpidRecord::default();
    rec.high = 80.0;
    rec.hsv = 1; // MINOR
    rec.val = 85.0;
    let mut common = CommonFields::default();
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::Minor,
        "HIGH crossing must raise SEVR"
    );
    assert_eq!(
        common.nsta,
        alarm_status::HIGH_ALARM,
        "HIGH crossing must set STAT"
    );

    // Crossing HIHI raises the configured HIHI severity/status.
    let mut rec = EpidRecord::default();
    rec.hihi = 100.0;
    rec.hhsv = 2; // MAJOR
    rec.val = 110.0;
    let mut common = CommonFields::default();
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::Major,
        "HIHI crossing must raise SEVR"
    );
    assert_eq!(
        common.nsta,
        alarm_status::HIHI_ALARM,
        "HIHI crossing must set STAT"
    );

    // A value inside the limits must NOT raise any alarm.
    let mut rec = EpidRecord::default();
    rec.hihi = 100.0;
    rec.high = 80.0;
    rec.low = 20.0;
    rec.lolo = 10.0;
    rec.hhsv = 2;
    rec.hsv = 1;
    rec.lsv = 1;
    rec.llsv = 2;
    rec.val = 50.0;
    let mut common = CommonFields::default();
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::NoAlarm,
        "in-limits must not raise SEVR"
    );
    assert_eq!(
        common.nsta,
        alarm_status::NO_ALARM,
        "in-limits must not set STAT"
    );
}

/// Regression: `recGblSetSevr` is raise-only (maximize-severity), so a
/// second, lower-severity alarm in the same cycle must not lower the
/// pending severity; a higher one must raise it.
#[test]
fn test_check_alarms_hook_maximizes_severity() {
    let mut rec = EpidRecord::default();
    rec.hihi = 100.0;
    rec.hhsv = 2; // MAJOR
    rec.val = 110.0;
    let mut common = CommonFields::default();

    // Pre-seed a lower pending severity, as an upstream MS link would.
    common.nsev = AlarmSeverity::Minor;
    common.nsta = alarm_status::LINK_ALARM;

    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::Major,
        "higher epid alarm must raise the pending severity"
    );
    assert_eq!(common.nsta, alarm_status::HIHI_ALARM);

    // A lower-severity epid alarm must not overwrite a higher pending one.
    let mut rec = EpidRecord::default();
    rec.high = 80.0;
    rec.hsv = 1; // MINOR
    rec.val = 85.0;
    let mut common = CommonFields::default();
    common.nsev = AlarmSeverity::Invalid;
    common.nsta = alarm_status::COMM_ALARM;
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::Invalid,
        "lower epid alarm must not lower a higher pending severity"
    );
    assert_eq!(common.nsta, alarm_status::COMM_ALARM);
}

// ============================================================
// PID algorithm (via device support)
// ============================================================

#[test]
fn test_pid_p_only() {
    let mut rec = EpidRecord::default();
    rec.kp = 2.0;
    rec.ki = 0.0;
    rec.kd = 0.0;
    rec.val = 100.0; // setpoint
    rec.cval = 90.0; // controlled value
    rec.fbon = 1; // feedback on
    rec.fbop = 1; // was already on
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 0.0; // no minimum dt

    // Need a small time delta for dt > mdt check
    std::thread::sleep(std::time::Duration::from_millis(5));

    EpidSoftDeviceSupport::do_pid(&mut rec);

    // P = KP * (setpoint - cval) = 2.0 * 10.0 = 20.0
    assert!(
        (rec.p - 20.0).abs() < 1e-6,
        "P should be ~20.0, got {}",
        rec.p
    );
    assert!(
        rec.i.abs() < 1e-6,
        "I should be ~0 with KI=0, got {}",
        rec.i
    );
    // Output = P + I + D = 20.0
    assert!(
        (rec.oval - 20.0).abs() < 1.0,
        "OVAL should be ~20.0, got {}",
        rec.oval
    );
}

#[test]
fn test_pid_output_clamping() {
    let mut rec = EpidRecord::default();
    rec.kp = 100.0;
    rec.ki = 0.0;
    rec.kd = 0.0;
    rec.val = 100.0;
    rec.cval = 0.0; // huge error
    rec.fbon = 1;
    rec.fbop = 1;
    rec.drvh = 50.0;
    rec.drvl = -50.0;
    rec.mdt = 0.0;

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // Output should be clamped to DRVH=50
    assert!(
        rec.oval <= 50.0,
        "Output should be clamped to DRVH, got {}",
        rec.oval
    );
}

#[test]
fn test_pid_feedback_off_no_change() {
    let mut rec = EpidRecord::default();
    rec.kp = 1.0;
    rec.ki = 1.0;
    rec.val = 100.0;
    rec.cval = 50.0;
    rec.fbon = 0; // feedback OFF
    rec.fbop = 0;
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 0.0;

    let i_before = rec.i;
    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // With feedback off, I should not change (KI anti-windup rule 3)
    // Actually ki=1 but fbon=0, so di is computed but not added
    // However with ki=1 and fbon=0, the integral doesn't accumulate
    // but ki != 0 so I is kept (not zeroed)
    assert_eq!(rec.i, i_before, "I should not change with feedback off");
}

#[test]
fn test_pid_mdt_skip() {
    let mut rec = EpidRecord::default();
    rec.kp = 1.0;
    rec.ki = 0.0;
    rec.kd = 0.0;
    rec.val = 100.0;
    rec.cval = 50.0;
    rec.fbon = 1;
    rec.fbop = 1;
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 100.0; // Very long minimum dt

    // Don't sleep — dt will be ~0 which is < mdt=100
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // Should have skipped — oval unchanged
    assert_eq!(rec.oval, 0.0, "Should skip when dt < mdt");
}

#[test]
fn test_pid_output_deadband() {
    let mut rec = EpidRecord::default();
    rec.kp = 1.0;
    rec.ki = 0.0;
    rec.kd = 0.0;
    rec.val = 100.0;
    rec.cval = 95.0; // error = 5.0, P = 5.0
    rec.fbon = 1;
    rec.fbop = 1;
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 0.0;
    rec.odel = 10.0; // Deadband = 10
    rec.oval = 7.0; // Current output is 7.0

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // New computed output: P = 1.0 * 5.0 = 5.0
    // Change from 7.0 to 5.0 = |2.0| < ODEL=10.0
    // So OVAL should NOT change
    assert_eq!(
        rec.oval, 7.0,
        "OVAL should not change within deadband, got {}",
        rec.oval
    );
}

#[test]
fn test_pid_output_deadband_exceeded() {
    let mut rec = EpidRecord::default();
    rec.kp = 10.0;
    rec.ki = 0.0;
    rec.kd = 0.0;
    rec.val = 100.0;
    rec.cval = 50.0; // error = 50, P = 500
    rec.fbon = 1;
    rec.fbop = 1;
    rec.drvh = 1000.0;
    rec.drvl = -1000.0;
    rec.mdt = 0.0;
    rec.odel = 10.0;
    rec.oval = 7.0;

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // New output: P = 10 * 50 = 500, change = |500 - 7| >> 10
    // So OVAL SHOULD change
    assert_ne!(rec.oval, 7.0, "OVAL should change when deadband exceeded");
}

#[test]
fn test_pid_bumpless_turn_on() {
    let mut rec = EpidRecord::default();
    rec.kp = 1.0;
    rec.ki = 1.0;
    rec.kd = 0.0;
    rec.val = 100.0;
    rec.cval = 50.0;
    rec.fbon = 1; // Feedback ON
    rec.fbop = 0; // Was OFF → bumpless transition
    rec.oval = 42.0; // Current output before turn-on
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 0.0;

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // On bumpless turn-on, I is set to current OVAL (42.0)
    assert!(
        (rec.i - 42.0).abs() < 1e-6,
        "I should be set to OVAL on bumpless turn-on, got {}",
        rec.i
    );
}

/// Regression for BUG 1 — epid_soft.rs MaxMin-mode error.
///
/// `do_pid` captured the previous controlled value from `epid.cval`, the
/// SAME field as the current `cval`, so `e = cval - pcval` was identically
/// 0.0 and the sign-detection block degenerated: the output step had a
/// fixed sign regardless of which way CVAL actually moved. The previous
/// controlled value is `epid.cvlp` (maintained by `update_monitors`).
///
/// With the fix, a CVAL that moved UP vs a CVAL that moved DOWN must
/// produce output steps of OPPOSITE sign.
#[test]
fn test_maxmin_error_uses_cvlp_previous_value() {
    // Helper: one MaxMin cycle. cvlp = previous CVAL, cval = current.
    fn one_cycle(cvlp: f64, cval: f64) -> f64 {
        let mut rec = EpidRecord::default();
        rec.fmod = 1; // MaxMin
        rec.kp = 1.0;
        rec.fbon = 1;
        rec.fbop = 1; // already on — exercises the e = cval - pcval path
        rec.d = 1.0; // previous d > 0 → base sign +1
        rec.drvh = 1000.0;
        rec.drvl = -1000.0;
        rec.mdt = 0.0;
        rec.oval = 0.0;
        rec.cvlp = cvlp; // previous controlled value
        rec.cval = cval; // current controlled value
        std::thread::sleep(std::time::Duration::from_millis(5));
        EpidSoftDeviceSupport::do_pid(&mut rec);
        rec.oval
    }

    // CVAL moved UP (e = +10 > 0, kp > 0): sign stays +1 → output step +kp.
    let oval_up = one_cycle(100.0, 110.0);
    // CVAL moved DOWN (e = -10 < 0, kp > 0): sign flips → output step -kp.
    let oval_down = one_cycle(100.0, 90.0);

    assert!(
        oval_up > 0.0,
        "CVAL rising should drive output step positive, got {oval_up}"
    );
    assert!(
        oval_down < 0.0,
        "CVAL falling should drive output step negative, got {oval_down}"
    );
    assert_ne!(
        oval_up.signum(),
        oval_down.signum(),
        "output step sign must depend on CVAL movement direction (non-zero error)"
    );
}

#[test]
fn test_maxmin_mode() {
    let mut rec = EpidRecord::default();
    rec.fmod = 1; // MaxMin mode
    rec.kp = 1.0;
    rec.fbon = 1;
    rec.fbop = 1; // Was already on
    rec.cval = 100.0;
    rec.d = 1.0; // Previous d > 0
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 0.0;
    rec.oval = 50.0;

    // Set previous cval via cvlp isn't used directly in do_pid,
    // but cval at entry is the "previous" and then cval is updated from INP.
    // In the test, cval is already set before do_pid is called.

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // In MaxMin mode, output should change from previous
    assert_ne!(rec.oval, 50.0, "MaxMin should change output");
}

// ============================================================
// Monitor logic
// ============================================================

#[test]
fn test_update_monitors_tracks_previous() {
    let mut rec = EpidRecord::default();
    rec.p = 10.0;
    rec.i = 20.0;
    rec.d = 30.0;
    rec.dt = 0.5;
    rec.err = 5.0;
    rec.cval = 42.0;

    rec.update_monitors();

    assert_eq!(rec.pp, 10.0);
    assert_eq!(rec.ip, 20.0);
    assert_eq!(rec.dp, 30.0);
    assert_eq!(rec.dtp, 0.5);
    assert_eq!(rec.errp, 5.0);
    assert_eq!(rec.cvlp, 42.0);
}

// ============================================================
// Link declarations
// ============================================================

#[test]
fn test_multi_input_links() {
    let rec = EpidRecord::default();
    let links = rec.multi_input_links();
    // Only INP->CVAL is unconditional; STPL->VAL is conditional on SMSL
    // and handled in process(), not in multi_input_links().
    assert_eq!(links.len(), 1);
    assert_eq!(links[0], ("INP", "CVAL"));
}

#[test]
fn test_multi_output_links() {
    let rec = EpidRecord::default();
    let links = rec.multi_output_links();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0], ("OUTL", "OVAL"));
}

// ============================================================
// ERR field — C devEpidSoft.c:208 writes ERR for EVERY mode
// ============================================================

/// Regression: C `devEpidSoft.c:98` declares `double e = 0.;` and
/// `devEpidSoft.c:208` writes `pepid->err = e;` unconditionally,
/// regardless of feedback mode. The port previously suppressed the
/// ERR write entirely in MaxMin mode. In MaxMin mode with feedback
/// already on, C sets `e = cval - pcval` (devEpidSoft.c:186), so ERR
/// must hold the CVAL delta.
#[test]
fn test_maxmin_err_is_cval_delta() {
    let mut rec = EpidRecord::default();
    rec.fmod = 1; // MaxMin
    rec.kp = 1.0;
    rec.fbon = 1;
    rec.fbop = 1; // already on — exercises the e = cval - pcval path
    rec.d = 1.0;
    rec.drvh = 1000.0;
    rec.drvl = -1000.0;
    rec.mdt = 0.0;
    rec.cvlp = 100.0; // previous controlled value
    rec.cval = 130.0; // current controlled value
    rec.err = -999.0; // stale value that must be overwritten

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // C devEpidSoft.c:186 + :208 — ERR = cval - pcval = 130 - 100 = 30.
    assert!(
        (rec.err - 30.0).abs() < 1e-9,
        "MaxMin ERR must be cval - pcval = 30.0, got {}",
        rec.err
    );
}

/// Regression: MaxMin bumpless OFF->ON edge. C `e` keeps its initial
/// value 0.0 (devEpidSoft.c:98) because the `cval - pcval` assignment
/// is in the else-branch; `pepid->err = e;` then writes 0.0.
#[test]
fn test_maxmin_err_zero_on_bumpless_edge() {
    let mut rec = EpidRecord::default();
    rec.fmod = 1; // MaxMin
    rec.kp = 1.0;
    rec.fbon = 1;
    rec.fbop = 0; // OFF -> ON bumpless edge
    rec.drvh = 1000.0;
    rec.drvl = -1000.0;
    rec.mdt = 0.0;
    rec.cvlp = 100.0;
    rec.cval = 130.0;
    rec.err = -999.0; // stale

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // C devEpidSoft.c:98 initial e=0.0 survives the bumpless edge.
    assert_eq!(
        rec.err, 0.0,
        "MaxMin bumpless-edge ERR must be 0.0, got {}",
        rec.err
    );
}

// ============================================================
// Fast device support — C devEpidFast.c
// ============================================================

use std_rs::device_support::epid_fast::EpidFastPvt;

/// C `devEpidFast.c::computeNumAverage` (devEpidFast.c:356-362):
/// `numAverage = 0.5 + timePerPointRequested/callbackInterval`,
/// clamped to `>= 1`; `timePerPointActual = numAverage *
/// callbackInterval`.
#[test]
fn test_fast_compute_num_average() {
    let mut pvt = EpidFastPvt::default();
    pvt.callback_interval = 0.001; // 1 ms driver callback
    pvt.time_per_point_requested = 0.010; // 10 ms requested

    pvt.compute_num_average();

    // 0.5 + 10/1 = 10.5 -> 10
    assert_eq!(pvt.num_average, 10);
    assert!((pvt.time_per_point_actual - 0.010).abs() < 1e-12);

    // Requested shorter than one callback -> clamp to 1.
    pvt.time_per_point_requested = 0.0;
    pvt.compute_num_average();
    assert_eq!(pvt.num_average, 1);
    assert!((pvt.time_per_point_actual - 0.001).abs() < 1e-12);
}

/// C `devEpidFast.c::intervalCallback` (devEpidFast.c:367-375):
/// updates `callbackInterval` and recomputes `numAverage`.
#[test]
fn test_fast_interval_callback_recomputes_average() {
    let mut pvt = EpidFastPvt::default();
    pvt.time_per_point_requested = 0.010;
    pvt.interval_callback(0.002); // 2 ms callback interval

    // 0.5 + 10/2 = 5.5 -> 5
    assert_eq!(pvt.num_average, 5);
    assert!((pvt.callback_interval - 0.002).abs() < 1e-12);
    assert!((pvt.time_per_point_actual - 0.010).abs() < 1e-12);
}

/// C `do_PID` (devEpidFast.c:430) uses `dt = pPvt->callbackInterval`,
/// the configured interval — not a measured wall-clock difference.
/// The derivative term D = KP*KD*(dError/dt) must use that interval.
#[test]
fn test_fast_do_pid_uses_callback_interval_as_dt() {
    let mut pvt = EpidFastPvt::default();
    pvt.callback_interval = 0.5; // dt
    pvt.num_average = 1;
    pvt.kp = 1.0;
    pvt.ki = 0.0;
    pvt.kd = 2.0;
    pvt.drvh = 1000.0;
    pvt.drvl = -1000.0;
    pvt.val = 100.0;
    pvt.fbon = true;
    pvt.fbop = true;
    pvt.err = 0.0; // previous error

    pvt.do_pid(90.0); // cval = 90 -> e = 10, de = 10 - 0 = 10

    // D = KP*KD*(de/dt) = 1*2*(10/0.5) = 40
    assert!(
        (pvt.d - 40.0).abs() < 1e-9,
        "D must use callback_interval (0.5s) as dt -> 40.0, got {}",
        pvt.d
    );
    // P = KP*e = 10
    assert!((pvt.p - 10.0).abs() < 1e-9, "P must be 10.0, got {}", pvt.p);
}

/// Regression: the anti-windup clamp and the output clamp must be
/// panic-free even when the drive limits are inverted (drvl > drvh) —
/// which is exactly the state C `devEpidFast.c:121-123` seeds before
/// `update_params` runs (`lowLimit=1, highLimit=-1`). `f64::clamp`
/// panics when min > max; C uses sequential `if` clamps.
#[test]
fn test_fast_do_pid_inverted_limits_no_panic() {
    // Default EpidFastPvt seeds the inverted C init limits.
    let mut pvt = EpidFastPvt::default();
    assert!(pvt.drvl > pvt.drvh, "default must seed inverted C limits");
    pvt.callback_interval = 0.001;
    pvt.num_average = 1;
    pvt.ki = 1.0;
    pvt.val = 100.0;
    pvt.fbon = true;
    pvt.fbop = true;

    // Must not panic despite drvl=1.0 > drvh=-1.0.
    pvt.do_pid(50.0);
}

/// C `dataCallback` (devEpidFast.c:384-395): when numAverage > 1 the
/// driver accumulates points and only runs `do_PID` once the count is
/// reached, on the averaged value.
#[test]
fn test_fast_do_pid_averaging() {
    let mut pvt = EpidFastPvt::default();
    pvt.callback_interval = 0.001;
    pvt.num_average = 4;
    pvt.kp = 1.0;
    pvt.ki = 0.0;
    pvt.kd = 0.0;
    pvt.drvh = 1000.0;
    pvt.drvl = -1000.0;
    pvt.val = 100.0;
    pvt.fbon = true;
    pvt.fbop = true;

    // First 3 points accumulate, no compute yet.
    pvt.do_pid(10.0);
    pvt.do_pid(20.0);
    pvt.do_pid(30.0);
    assert_eq!(pvt.p, 0.0, "no compute before num_average points");

    // 4th point triggers compute on the average (10+20+30+40)/4 = 25.
    pvt.do_pid(40.0);
    assert!(
        (pvt.cval - 25.0).abs() < 1e-9,
        "averaged cval must be 25.0, got {}",
        pvt.cval
    );
    // e = 100 - 25 = 75, P = KP*e = 75.
    assert!((pvt.p - 75.0).abs() < 1e-9, "P must be 75.0, got {}", pvt.p);
}

/// C `devEpidFast.c::do_PID` (devEpidFast.c:400-482) runs the PID
/// algorithm UNCONDITIONALLY — `epidFastPvt` (devEpidFast.c:35-72) has
/// no `fmod` field and `do_PID` never branches on FMOD. FMOD/MaxMin is
/// honoured only by the Soft device supports (`devEpidSoft.c:137`,
/// `devEpidSoftCallback.c:173` have `switch (pepid->fmod)`).
///
/// Two `EpidFastPvt` instances with identical PID inputs must produce
/// identical output — the Fast support has no FMOD knob to diverge on.
#[test]
fn test_fast_do_pid_ignores_fmod() {
    let make = || {
        let mut pvt = EpidFastPvt::default();
        pvt.callback_interval = 0.5;
        pvt.num_average = 1;
        pvt.kp = 1.0;
        pvt.ki = 0.0;
        pvt.kd = 2.0;
        pvt.drvh = 1000.0;
        pvt.drvl = -1000.0;
        pvt.val = 100.0;
        pvt.fbon = true;
        pvt.fbop = true;
        pvt
    };

    let mut a = make();
    let mut b = make();
    a.do_pid(90.0);
    b.do_pid(90.0);

    // PID output: P = KP*e = 10, I = 0 (KI=0), D = KP*KD*de/dt = 40.
    assert!(
        (a.oval - 50.0).abs() < 1e-9,
        "Fast do_pid must run PID unconditionally -> oval 50.0, got {}",
        a.oval
    );
    assert_eq!(
        a.oval, b.oval,
        "Fast support has no FMOD branch; output must be deterministic"
    );
    assert_eq!(a.p, b.p);
    assert_eq!(a.d, b.d);
}
