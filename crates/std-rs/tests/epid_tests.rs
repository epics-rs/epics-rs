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
    rec.oval = 42.0; // Last commanded output before turn-on
    rec.drvh = 200.0;
    rec.drvl = -200.0;
    rec.mdt = 0.0;

    // C `devEpidSoft.c:153-158`: on the bumpless edge the integral
    // term is seeded from the OUTL output link's *actual current
    // value*, NOT from the last commanded OVAL. The framework reads
    // OUTL into `I` via the `ReadDbLink` pre-process action emitted by
    // `EpidRecord::pre_process_actions` BEFORE `do_pid` runs. Simulate
    // that readback here: `I` already holds OUTL's value (37.0).
    rec.i = 37.0;

    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);

    // do_pid must KEEP the readback value on the bumpless edge —
    // it must not overwrite I with OVAL (the old approximation).
    assert!(
        (rec.i - 37.0).abs() < 1e-6,
        "I must keep the OUTL readback value on bumpless turn-on, got {}",
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

/// C `devEpidSoft.c:218-224`: the OUTL `dbPutLink` fires only when
/// `pepid->fbon && outl.type != CONSTANT`, and only when `do_pid`
/// reached that line — NOT after the sub-MDT (`:125`) or CONSTANT-INP
/// (`:110-112`) early returns. The framework drives the OUTL write via
/// `multi_output_links`, so it must be empty unless `do_pid` ran with
/// feedback ON. Boundary test, one case per gate condition.
#[test]
fn test_multi_output_links_gated_on_fbon_and_compute() {
    // Before any compute: no OUTL write (C writes OUTL only inside do_pid).
    let rec = EpidRecord::default();
    assert!(
        rec.multi_output_links().is_empty(),
        "OUTL must not be written before do_pid runs"
    );

    // Feedback ON, dt >= mdt, non-constant INP → C reaches the OUTL write.
    let mut on = EpidRecord::default();
    on.kp = 1.0;
    on.val = 100.0;
    on.cval = 90.0;
    on.fbon = 1;
    on.fbop = 1;
    on.drvh = 200.0;
    on.drvl = -200.0;
    on.mdt = 0.0;
    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut on);
    let links = on.multi_output_links();
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0],
        ("OUTL", "OVAL"),
        "feedback ON must write OUTL <- OVAL"
    );

    // Feedback OFF → C `:220` `if (fbon && ...)` is false, no OUTL write,
    // even though do_pid still computes/updates OVAL.
    let mut off = EpidRecord::default();
    off.kp = 1.0;
    off.val = 100.0;
    off.cval = 90.0;
    off.fbon = 0;
    off.drvh = 200.0;
    off.drvl = -200.0;
    off.mdt = 0.0;
    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut off);
    assert!(
        off.multi_output_links().is_empty(),
        "feedback OFF must not write OUTL (C devEpidSoft.c:220)"
    );

    // Sub-MDT cycle → C `:125` returns before the OUTL write.
    let mut fast = EpidRecord::default();
    fast.kp = 1.0;
    fast.val = 100.0;
    fast.cval = 90.0;
    fast.fbon = 1;
    fast.fbop = 1;
    fast.drvh = 200.0;
    fast.drvl = -200.0;
    fast.mdt = 3600.0; // far larger than the elapsed dt → sub-MDT skip
    EpidSoftDeviceSupport::do_pid(&mut fast);
    assert!(
        fast.multi_output_links().is_empty(),
        "sub-MDT cycle must not write OUTL (C devEpidSoft.c:125)"
    );

    // CONSTANT INP → C `:110-112` returns before the OUTL write.
    let mut konst = EpidRecord::default();
    konst.inp = "5.0".to_string();
    assert_eq!(link_field_type(&konst.inp), LinkType::Constant);
    konst.fbon = 1;
    konst.fbop = 1;
    konst.drvh = 200.0;
    konst.drvl = -200.0;
    konst.mdt = 0.0;
    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut konst);
    assert!(
        konst.multi_output_links().is_empty(),
        "CONSTANT INP must not write OUTL (C devEpidSoft.c:110-112)"
    );
}

/// Regression for the suppressed VAL deadband monitor. C
/// `epidRecord.c:346-374` `monitor()` posts VAL when `mlst-val > mdel`
/// and only THEN advances `mlst = val`. The port's `update_monitors`
/// pre-advanced MLST/ALST to VAL, so the framework's `check_deadband_ext`
/// (which owns the post + advance) then saw a zero delta and never posted
/// VAL. `update_monitors` must leave MLST/ALST untouched.
#[test]
fn test_update_monitors_does_not_advance_val_deadband_baselines() {
    let mut rec = EpidRecord::default();
    // Default MLST/ALST are 0.0; a VAL far beyond the deadband would,
    // under the old code, advance them to VAL inside update_monitors.
    rec.mdel = 0.5;
    rec.adel = 0.5;
    rec.val = 100.0;

    rec.update_monitors();

    assert_eq!(
        rec.get_field("MLST").and_then(|v| v.to_f64()),
        Some(0.0),
        "update_monitors must NOT advance MLST — the framework \
         check_deadband_ext owns that, after firing the VAL post"
    );
    assert_eq!(
        rec.get_field("ALST").and_then(|v| v.to_f64()),
        Some(0.0),
        "update_monitors must NOT advance ALST"
    );
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

/// C `do_PID` (devEpidFast.c:445-448): on the feedback OFF->ON edge
/// (`feedbackOn && !prevFeedbackOn`) the integral term is seeded from
/// the output port's *present value* via `pPvt->pfloat64Output->read`,
/// not from the last value the loop commanded. When an `output_reader`
/// is wired, the bumpless edge must use the reader's value.
#[test]
fn test_fast_bumpless_edge_seeds_i_from_output_reader() {
    use std::sync::{Arc, Mutex};

    let mut pvt = EpidFastPvt::default();
    pvt.callback_interval = 0.001;
    pvt.num_average = 1;
    pvt.kp = 1.0;
    pvt.ki = 1.0; // KI != 0 so the seeded I survives (I=0 only when KI==0)
    pvt.kd = 0.0;
    pvt.drvh = 1000.0;
    pvt.drvl = -1000.0;
    pvt.val = 100.0;

    // Stale last-commanded output — the `oval` fallback value.
    pvt.oval = 7.0;
    // The output port's actual present value, distinct from oval.
    let reader_value = 42.0;
    pvt.output_reader = Some(Arc::new(Mutex::new(move || Some(reader_value))));

    // OFF -> ON edge: feedback turns on, previous state was off.
    pvt.fbon = true;
    pvt.fbop = false;

    pvt.do_pid(90.0); // e = 100 - 90 = 10

    // Bumpless: I seeded from the reader (42.0), NOT the stale oval (7.0).
    assert!(
        (pvt.i - 42.0).abs() < 1e-9,
        "bumpless edge must seed I from output_reader value 42.0, got {}",
        pvt.i
    );
    // oval = P + I + D = (1*10) + 42 + 0 = 52.
    assert!(
        (pvt.oval - 52.0).abs() < 1e-9,
        "oval must reflect reader-seeded I -> 52.0, got {}",
        pvt.oval
    );
}

/// Safety net: when no `output_reader` is wired, the bumpless OFF->ON
/// edge falls back to the last commanded `OVAL`. C `devEpidFast.c`
/// always has a real `pfloat64Output->read`; the Rust port keeps the
/// `oval` fallback for records whose output is not a readable asyn
/// port. (devEpidFast.c:445-448 reference.)
#[test]
fn test_fast_bumpless_edge_falls_back_to_oval_without_reader() {
    let mut pvt = EpidFastPvt::default();
    pvt.callback_interval = 0.001;
    pvt.num_average = 1;
    pvt.kp = 1.0;
    pvt.ki = 1.0;
    pvt.kd = 0.0;
    pvt.drvh = 1000.0;
    pvt.drvl = -1000.0;
    pvt.val = 100.0;
    pvt.oval = 7.0; // last commanded output — the fallback
    assert!(pvt.output_reader.is_none(), "no reader wired");

    pvt.fbon = true;
    pvt.fbop = false;

    pvt.do_pid(90.0); // e = 10

    // No reader -> I seeded from the oval fallback (7.0).
    assert!(
        (pvt.i - 7.0).abs() < 1e-9,
        "bumpless edge without reader must fall back to oval 7.0, got {}",
        pvt.i
    );
}

/// `set_output_port` wires both the output writer and the bumpless
/// reader from a single asyn Float64 port handle — mirroring C
/// `devEpidFast.c` `init_record`, which captures one
/// `pPvt->pfloat64Output` interface used for both `read`
/// (devEpidFast.c:446-448) and `write` (devEpidFast.c:471-473).
#[test]
fn test_fast_set_output_port_wires_reader_from_asyn_port() {
    use asyn_rs::manager::PortManager;
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
    use asyn_rs::sync_io::SyncIOHandle;
    use std::time::Duration;
    use std_rs::device_support::epid_fast::EpidFastDeviceSupport;

    // Minimal parameter-library port acting as the asyn Float64
    // output port (the DAC the PID loop drives).
    struct OutputPort {
        base: PortDriverBase,
    }
    impl PortDriver for OutputPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    let mut base = PortDriverBase::new("EPID_OUT_PORT", 1, PortFlags::default());
    let reason = base.create_param("OUTPUT", ParamType::Float64).unwrap();
    // Preload the output port's present value (the real DAC output).
    base.set_float64_param(reason, 0, 33.0).unwrap();

    let manager = PortManager::new();
    manager.register_port(OutputPort { base }).unwrap();

    let sync_io = SyncIOHandle::connect(&manager, "EPID_OUT_PORT", 0, Duration::from_secs(1))
        .expect("connect to output port");
    // Sanity: the port reports the value we seeded.
    assert!((sync_io.read_float64(reason).unwrap() - 33.0).abs() < 1e-9);

    let dev = EpidFastDeviceSupport::new();
    dev.set_output_port(sync_io, reason);

    // Configure for a bumpless OFF->ON edge.
    let pvt_arc = dev.pvt();
    let mut pvt = pvt_arc.lock().unwrap();
    pvt.callback_interval = 0.001;
    pvt.num_average = 1;
    pvt.kp = 1.0;
    pvt.ki = 1.0;
    pvt.kd = 0.0;
    pvt.drvh = 1000.0;
    pvt.drvl = -1000.0;
    pvt.val = 100.0;
    pvt.oval = 7.0; // stale fallback — must NOT be used
    pvt.fbon = true;
    pvt.fbop = false;
    // Reader and writer installed by set_output_port.
    assert!(
        pvt.output_reader.is_some(),
        "set_output_port must wire reader"
    );
    assert!(
        pvt.output_writer.is_some(),
        "set_output_port must wire writer"
    );
    pvt.do_pid(90.0);
    // I seeded from the asyn port's present value (33.0), not oval.
    assert!(
        (pvt.i - 33.0).abs() < 1e-9,
        "bumpless edge must seed I from asyn output port value 33.0, got {}",
        pvt.i
    );
}

// ============================================================
// Link-type discrimination — C `link.h` CONSTANT / DB_LINK /
// CA_LINK; epid consumers `devEpidSoft.c` / `devEpidSoftCallback.c`.
// ============================================================

use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::{LinkType, ProcessAction, link_field_type};
use std_rs::device_support::epid_soft_callback::EpidSoftCallbackDeviceSupport;

/// C `devEpidSoft.c:110-112`: a CONSTANT `INP` link means "nothing to
/// control" — `do_pid` must skip the PID compute and the record must
/// be flagged SOFT_ALARM/INVALID_ALARM by the `check_alarms` hook.
#[test]
fn test_constant_inp_raises_soft_alarm_and_skips_compute() {
    let mut rec = EpidRecord::default();
    // CONSTANT INP link — a literal value, not a PV reference.
    rec.inp = "3.14".to_string();
    rec.kp = 2.0;
    rec.val = 100.0;
    rec.cval = 10.0;
    rec.fbon = 1;
    rec.fmod = 0;
    let oval_before = rec.oval;

    assert_eq!(link_field_type(&rec.inp), LinkType::Constant);

    EpidSoftDeviceSupport::do_pid(&mut rec);

    // Compute skipped: OVAL unchanged, and the constant flag is set.
    assert!(rec.inp_constant, "CONSTANT INP must set inp_constant");
    assert_eq!(
        rec.oval, oval_before,
        "PID compute must be skipped for a CONSTANT INP link"
    );

    // check_alarms hook raises SOFT_ALARM/INVALID_ALARM.
    let mut common = CommonFields::default();
    Record::check_alarms(&mut rec, &mut common);
    assert_eq!(common.nsta, alarm_status::SOFT_ALARM);
    assert_eq!(common.nsev, AlarmSeverity::Invalid);
}

/// A DB `INP` link does NOT trip the constant path — PID runs normally.
#[test]
fn test_db_inp_does_not_raise_soft_alarm() {
    let mut rec = EpidRecord::default();
    rec.inp = "OTHER:REC.VAL".to_string();
    assert_eq!(link_field_type(&rec.inp), LinkType::Db);
    rec.kp = 2.0;
    rec.val = 100.0;
    rec.cval = 10.0;
    rec.fbon = 1;
    rec.fbop = 1; // not the bumpless edge
    rec.mdt = 0.0;

    EpidSoftDeviceSupport::do_pid(&mut rec);
    assert!(!rec.inp_constant, "DB INP must not set inp_constant");
}

/// C `devEpidSoftCallback.c:120-151`: a DB (`type != CA_LINK`) TRIG
/// link is written synchronously and the record falls through to PID
/// in the SAME pass — no second-pass deferral.
///
/// The trigger write must land BEFORE this cycle's `INP -> CVAL` fetch
/// (C `dbPutLink` at `:121-127` precedes `dbGetLink(&pepid->inp)` at
/// `:149`). The framework resolves input links before device-support
/// `read()` runs, so the TRIG write is NOT a device-support action —
/// it is emitted by `EpidRecord::pre_input_link_actions`, which the
/// framework executes strictly before the input-link fetch. The
/// device-support `read()` therefore runs PID with no actions of its
/// own and no second-pass deferral.
#[test]
fn test_db_trig_link_synchronous_fallthrough() {
    let mut rec = EpidRecord::default();
    rec.trig = "READBACK:REC.VAL".to_string();
    assert_eq!(link_field_type(&rec.trig), LinkType::Db);
    rec.kp = 1.0;
    rec.fbon = 1;
    rec.fbop = 1;

    // The TRIG write is a pre-input-link action emitted by the record,
    // gated on the callback DSET (DTYP "Async Soft Channel").
    rec.set_process_context(&ctx_with_dtyp("Async Soft Channel"));
    let pre = rec.pre_input_link_actions();
    assert!(
        pre.iter().any(|a| matches!(
            a,
            ProcessAction::WriteDbLink {
                link_field: "TRIG",
                ..
            }
        )),
        "DB TRIG must emit a WriteDbLink as a pre-input-link action"
    );

    let mut dev = EpidSoftCallbackDeviceSupport::new();
    let outcome = dev.read(&mut rec).expect("read ok");

    // DB link: PID ran synchronously this pass (did_compute is true),
    // and `read()` emits no actions and no ReprocessAfter — the TRIG
    // write already happened in the pre-input phase.
    assert!(outcome.did_compute(), "DB TRIG: PID runs this pass");
    assert!(
        outcome.actions.is_empty(),
        "DB TRIG: read() emits no actions — TRIG write is pre-input"
    );
    assert!(
        !outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "DB TRIG must NOT defer to a second pass"
    );
}

/// A non-callback DSET (`devEpidSoft`, DTYP "Soft Channel") has no
/// TRIG handling — `pre_input_link_actions` must emit nothing even
/// when a DB TRIG link is configured.
#[test]
fn test_non_callback_dtyp_emits_no_trig_write() {
    let mut rec = EpidRecord::default();
    rec.trig = "READBACK:REC.VAL".to_string();
    assert_eq!(link_field_type(&rec.trig), LinkType::Db);

    rec.set_process_context(&ctx_with_dtyp("Soft Channel"));
    assert!(
        rec.pre_input_link_actions().is_empty(),
        "non-callback DSET must not drive the TRIG link"
    );
}

/// C `devEpidSoftCallback.c:131-146`: a CA TRIG link cannot be waited
/// on synchronously — the trigger is fired, `pact` is set, and the
/// record is re-processed (PID deferred to the second pass).
#[test]
fn test_ca_trig_link_deferred_second_pass() {
    let mut rec = EpidRecord::default();
    rec.trig = "ca://REMOTE:READBACK".to_string();
    assert_eq!(link_field_type(&rec.trig), LinkType::Ca);
    rec.kp = 1.0;
    rec.fbon = 1;
    rec.fbop = 1;

    rec.set_process_context(&ctx_with_dtyp("Async Soft Channel"));

    // CA link: both a WriteDbLink for TRIG and a ReprocessAfter — the
    // PID compute is deferred to the re-process pass.
    let actions = rec.pre_input_link_actions();
    assert!(
        actions.iter().any(|a| matches!(
            a,
            ProcessAction::WriteDbLink {
                link_field: "TRIG",
                ..
            }
        )),
        "CA TRIG must emit a WriteDbLink"
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "CA TRIG must defer PID to a second pass"
    );

    // Second pass is the callback pass — C `devEpidSoftCallback.c:116`
    // `if (!pepid->pact)` skips the whole trigger block, so nothing is
    // re-fired and the PID runs.
    assert!(
        rec.pre_input_link_actions().is_empty(),
        "second pass must not re-trigger"
    );
}

/// TASK 2 — bumpless transfer: on the feedback OFF->ON edge in PID
/// mode, the epid record's `pre_process_actions` emits a `ReadDbLink`
/// to seed the integral term `I` from the `OUTL` output link's actual
/// current value (C `devEpidSoft.c:153-158`).
#[test]
fn test_bumpless_edge_emits_outl_readback_for_db_link() {
    let mut rec = EpidRecord::default();
    rec.outl = "DAC:REC.VAL".to_string();
    rec.fmod = 0; // PID mode
    rec.fbon = 1;
    rec.fbop = 0; // OFF -> ON edge

    let actions = rec.pre_process_actions();
    assert!(
        actions.iter().any(|a| matches!(
            a,
            ProcessAction::ReadDbLink {
                link_field: "OUTL",
                target_field: "__OUTL_SEED"
            }
        )),
        "bumpless OFF->ON edge with a DB OUTL must read OUTL — into the \
         internal staging cell, since C only commits it to I inside do_pid, \
         past the MDT gate (devEpidSoft.c:125,155), got {actions:?}"
    );
}

/// A CONSTANT `OUTL` link must NOT emit a readback — C only reads OUTL
/// when `outl.type != CONSTANT` (`devEpidSoft.c:153`).
#[test]
fn test_bumpless_edge_no_readback_for_constant_outl() {
    let mut rec = EpidRecord::default();
    rec.outl = "5.0".to_string(); // CONSTANT link
    assert_eq!(link_field_type(&rec.outl), LinkType::Constant);
    rec.fmod = 0;
    rec.fbon = 1;
    rec.fbop = 0;

    let actions = rec.pre_process_actions();
    assert!(
        actions.is_empty(),
        "CONSTANT OUTL must not emit a bumpless readback"
    );
}

/// No edge (feedback steady ON) — no bumpless readback.
#[test]
fn test_no_bumpless_readback_when_no_edge() {
    let mut rec = EpidRecord::default();
    rec.outl = "DAC:REC.VAL".to_string();
    rec.fmod = 0;
    rec.fbon = 1;
    rec.fbop = 1; // steady ON, not an edge

    assert!(rec.pre_process_actions().is_empty());
}

// ============================================================
// UDF gate — C `epidRecord.c:195-202`. While `udf==TRUE`, process()
// skips `do_pid` entirely and returns; the framework pushes
// `dbCommon.udf` into the record via `set_process_context`.
// ============================================================
use epics_base_rs::server::record::ProcessContext;

fn ctx_with_udf(udf: bool) -> ProcessContext {
    ProcessContext {
        udf,
        udfs: AlarmSeverity::Invalid,
        nsev: AlarmSeverity::NoAlarm,
        phas: 0,
        tse: 0,
        time: std::time::SystemTime::UNIX_EPOCH,
        tsel: String::new(),
        dtyp: String::new(),
        callback_priority: epics_base_rs::runtime::task::CallbackPriority::Low,
    }
}

fn ctx_with_dtyp(dtyp: &str) -> ProcessContext {
    ProcessContext {
        udf: false,
        udfs: AlarmSeverity::Invalid,
        nsev: AlarmSeverity::NoAlarm,
        phas: 0,
        tse: 0,
        time: std::time::SystemTime::UNIX_EPOCH,
        tsel: String::new(),
        dtyp: dtyp.to_string(),
        callback_priority: epics_base_rs::runtime::task::CallbackPriority::Low,
    }
}

/// C `epidRecord.c:195-202`: with `udf==TRUE`, `process()` must not run
/// `do_pid` — it returns 0 after `recGblSetSevr(UDF_ALARM, udfs)`. The
/// Rust framework pushes `udf` via `set_process_context`; with it set,
/// `process()` leaves OVAL/P/ERR untouched.
#[test]
fn test_process_skips_do_pid_while_udf_set() {
    let mut rec = EpidRecord::default();
    rec.kp = 2.0;
    rec.val = 100.0; // setpoint
    rec.cval = 10.0; // controlled value -> non-zero error
    rec.fbon = 1;
    rec.fbop = 1;
    rec.mdt = 0.0;
    let oval_before = rec.oval;
    let p_before = rec.p;

    // Framework pushes udf=TRUE before process() (record uninitialized).
    rec.set_process_context(&ctx_with_udf(true));
    rec.process().unwrap();

    assert_eq!(
        rec.oval, oval_before,
        "do_pid must be skipped while UDF is set -> OVAL unchanged"
    );
    assert_eq!(
        rec.p, p_before,
        "do_pid must be skipped while UDF is set -> P unchanged"
    );
}

/// Once the framework clears UDF (value became defined), the next
/// `process()` runs `do_pid` normally — the C "one cycle later"
/// behaviour. epidRecord.c runs do_pid as soon as `udf==FALSE`.
#[test]
fn test_process_runs_do_pid_once_udf_cleared() {
    let mut rec = EpidRecord::default();
    rec.kp = 2.0;
    rec.val = 100.0;
    rec.cval = 10.0; // error = 90
    rec.fbon = 1;
    rec.fbop = 1;
    rec.mdt = 0.0;
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Framework pushes udf=FALSE — record now has a defined value.
    rec.set_process_context(&ctx_with_udf(false));
    rec.process().unwrap();

    // P = KP * (VAL - CVAL) = 2.0 * 90.0 = 180.0 — proves do_pid ran.
    assert!(
        (rec.p - 180.0).abs() < 1e-6,
        "do_pid must run once UDF is clear; P should be ~180.0, got {}",
        rec.p
    );
}

/// C `epidRecord.c:191-193`: in closed-loop mode (SMSL=1) a successful
/// `dbGetLink(stpl)` clears `udf` *before* the `if (udf==TRUE)` check.
/// The framework fetches STPL->VAL before process() and reports the
/// fetch success via `set_resolved_input_links(&["STPL"])`; with that
/// signal, a closed-loop epid runs do_pid that cycle even though the
/// pushed `udf` is still the stale-TRUE from before the fetch.
#[test]
fn test_process_closed_loop_runs_do_pid_when_stpl_gave_value() {
    let mut rec = EpidRecord::default();
    rec.kp = 2.0;
    rec.smsl = 1; // closed-loop
    rec.val = 100.0; // STPL already fetched a defined setpoint
    rec.cval = 10.0;
    rec.fbon = 1;
    rec.fbop = 1;
    rec.mdt = 0.0;
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Framework reports the STPL fetch succeeded this cycle, then
    // pushes the stale udf=TRUE (recomputed only after process()).
    rec.set_resolved_input_links(&["STPL"]);
    rec.set_process_context(&ctx_with_udf(true));
    rec.process().unwrap();

    assert!(
        (rec.p - 180.0).abs() < 1e-6,
        "closed-loop epid whose STPL fetch succeeded must run do_pid \
         even when the pushed udf is stale-TRUE; P should be ~180.0, \
         got {}",
        rec.p
    );
}

/// C `epidRecord.c:191-193`: in closed-loop mode the `udf` clear is
/// gated on `RTN_SUCCESS(dbGetLink(&prec->stpl, ...))` — an ACTUAL
/// fetch success. A closed-loop epid whose STPL link is empty / its
/// DB/CA fetch failed (the framework reports NO resolved STPL) must
/// keep `udf` set and skip do_pid — Bug 2 regression guard.
#[test]
fn test_process_closed_loop_keeps_udf_when_stpl_fetch_failed() {
    let mut rec = EpidRecord::default();
    rec.kp = 2.0;
    rec.smsl = 1; // closed-loop
    rec.val = 100.0; // default 0.0 would also exercise this; explicit here
    rec.cval = 10.0;
    rec.fbon = 1;
    rec.fbop = 1;
    rec.mdt = 0.0;
    let p_before = rec.p;
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Framework reports NO link resolved this cycle (STPL fetch failed
    // or STPL empty), then pushes udf=TRUE.
    rec.set_resolved_input_links(&[]);
    rec.set_process_context(&ctx_with_udf(true));
    rec.process().unwrap();

    assert_eq!(
        rec.p, p_before,
        "closed-loop epid whose STPL fetch failed must keep udf set and \
         skip do_pid — !val.is_nan() must NOT proxy fetch success"
    );
    assert!(
        rec.value_is_undefined(),
        "value_is_undefined() must stay true so the framework keeps \
         common.udf set for the next cycle"
    );
}

/// The UDF gate applies only on the non-callback pass. When device
/// support already ran the PID (`set_device_did_compute(true)`),
/// `process()` must NOT re-run `do_pid` and must NOT consult UDF —
/// matching C `epidRecord.c`'s `if (!pact)` guard around the UDF check.
#[test]
fn test_process_with_device_did_compute_ignores_udf_gate() {
    let mut rec = EpidRecord::default();
    rec.kp = 2.0;
    rec.val = 100.0;
    rec.cval = 10.0;
    rec.fbon = 1;
    rec.fbop = 1;
    let oval_before = rec.oval;
    let p_before = rec.p;

    rec.set_process_context(&ctx_with_udf(true));
    rec.set_device_did_compute(true); // device support already ran do_pid
    rec.process().unwrap();

    assert_eq!(
        rec.oval, oval_before,
        "device-computed pass must not re-run do_pid"
    );
    assert_eq!(
        rec.p, p_before,
        "device-computed pass must not run the built-in PID"
    );
}

// ============================================================
// CRITICAL 1 — CA-TRIG epid: the process tail (checkAlarms /
// monitor / recGblFwdLink) must run EXACTLY ONCE per CA-trigger
// cycle, on the reprocess pass — NOT on the trigger pass.
//
// C `devEpidSoftCallback.c:143-145` sets `pepid->pact = TRUE` and
// `return(0)` on the CA TRIG path; C `epidRecord.c:207`
// `if (!pact && pepid->pact) return(0)` then returns BEFORE
// `recGblGetTimeStamp` / `checkAlarms` / `monitor` /
// `recGblFwdLink` — so the trigger pass runs NONE of the tail.
// The reprocess (callback) pass runs the tail once.
//
// In the Rust port the framework skips the alarm/timestamp/
// snapshot/OUT/FLNK tail exactly when `process()` returns
// `RecordProcessResult::AsyncPending`. The trigger pass must
// therefore return `AsyncPending`; the reprocess pass must return
// `Complete` (tail runs once).
// ============================================================
use epics_base_rs::server::record::RecordProcessResult;

#[test]
fn test_ca_trig_trigger_pass_returns_async_pending_tail_skipped() {
    let mut rec = EpidRecord::default();
    rec.trig = "ca://REMOTE:READBACK".to_string();
    assert_eq!(link_field_type(&rec.trig), LinkType::Ca);
    rec.kp = 1.0;
    rec.fbon = 1;
    rec.fbop = 1;
    // A CONSTANT STPL clears UDF (C `epidRecord.c:160-164`) so the
    // UDF gate does not pre-empt the trigger path — isolate the
    // CA-TRIG path under test.
    rec.stpl = "10.0".to_string();
    let mut udf = true;
    rec.post_init_finalize_undef(&mut udf).unwrap();

    rec.set_process_context(&ctx_with_dtyp("Async Soft Channel"));

    // --- Trigger pass: pre_input_link_actions fires the CA trigger ---
    let actions1 = rec.pre_input_link_actions();
    assert!(
        actions1.iter().any(|a| matches!(
            a,
            ProcessAction::WriteDbLink {
                link_field: "TRIG",
                ..
            }
        )),
        "CA TRIG trigger pass must emit a WriteDbLink{{TRIG}}"
    );
    assert!(
        actions1
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "CA TRIG trigger pass must emit a ReprocessAfter"
    );

    // No device attaches for this DTYP (is_soft_dtyp short-circuits),
    // so the record body owns the compute.
    rec.set_device_did_compute(false);
    let proc1 = rec.process().expect("trigger-pass process ok");
    assert_eq!(
        proc1.result,
        RecordProcessResult::AsyncPending,
        "CA-TRIG trigger pass: process() MUST return AsyncPending so the \
         framework skips the checkAlarms / monitor / recGblFwdLink tail \
         (C epidRecord.c:207 returns before the tail)"
    );

    // --- Reprocess (callback) pass: the trigger block is skipped ---
    assert!(
        rec.pre_input_link_actions().is_empty(),
        "reprocess pass must not re-trigger"
    );
    rec.set_device_did_compute(false);
    let proc2 = rec.process().expect("reprocess-pass process ok");
    assert_eq!(
        proc2.result,
        RecordProcessResult::Complete,
        "CA-TRIG reprocess pass: process() MUST return Complete so the \
         process tail (FLNK / monitors) runs — exactly once for the cycle"
    );
}

/// BUG 2 — epid MaxMin bumpless OFF->ON edge must read the OUTL
/// link into OVAL.
///
/// C `devEpidSoft.c:178-184` / `devEpidSoftCallback.c:214-220`: on
/// the MaxMin (FMOD==1) feedback OFF->ON edge, when `outl.type !=
/// CONSTANT`, `dbGetLink(&pepid->outl, DBR_DOUBLE, &oval, ...)` —
/// the OUTL target's current value is read into OVAL (NOT into the
/// integral term `I`, which is the PID-mode target).
#[test]
fn test_maxmin_bumpless_edge_emits_outl_readback_into_oval() {
    let mut rec = EpidRecord::default();
    rec.outl = "DAC:REC.VAL".to_string();
    rec.fmod = 1; // MaxMin mode
    rec.fbon = 1;
    rec.fbop = 0; // OFF -> ON edge

    let actions = rec.pre_process_actions();
    assert!(
        actions.iter().any(|a| matches!(
            a,
            ProcessAction::ReadDbLink {
                link_field: "OUTL",
                target_field: "__OUTL_SEED"
            }
        )),
        "MaxMin bumpless OFF->ON edge with a DB OUTL must read OUTL into the \
         internal staging cell, got {actions:?}"
    );
    // The readback must not be committed to ANY CA-visible field before
    // `do_pid` runs — C reads OUTL inside do_pid, past the MDT gate.
    assert!(
        !actions.iter().any(|a| matches!(
            a,
            ProcessAction::ReadDbLink {
                target_field: "I" | "OVAL",
                ..
            }
        )),
        "the pre-process read must not land in I or OVAL, got {actions:?}"
    );

    // `do_pid` is where C routes the seed by FMOD: MaxMin (fmod==1) puts it in
    // OVAL (devEpidSoft.c:181 reads into &oval), never in I (:155, PID-only).
    rec.mdt = 0.0;
    rec.i = 0.0;
    rec.oval = 0.0;
    rec.drvh = 100.0;
    rec.drvl = -100.0;
    rec.inp = "SENSOR".to_string();
    rec.put_field_internal("__OUTL_SEED", EpicsValue::Double(7.0))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);
    assert!(
        (rec.oval - 7.0).abs() < 1e-9,
        "MaxMin edge seeds OVAL from the OUTL readback, got {}",
        rec.oval
    );
    assert_eq!(rec.i, 0.0, "MaxMin edge must not touch I");
}

/// R7-64: C `devEpidSoft.c:125` — `if (dt < pepid->mdt) return(1);` runs
/// BEFORE the OUTL seed (`:150-158`). On a sub-MDT cycle the staged readback
/// must NOT reach `.I`, and FBOP must stay 0 so the edge survives to the next
/// cycle that actually computes.
#[test]
fn test_bumpless_seed_is_gated_by_mdt() {
    let mut rec = EpidRecord::default();
    rec.inp = "SENSOR".to_string();
    rec.fmod = 0; // PID
    rec.kp = 1.0;
    rec.ki = 0.5;
    rec.drvh = 100.0;
    rec.drvl = -100.0;
    rec.fbon = 1;
    rec.fbop = 0; // OFF -> ON edge
    rec.i = 0.0;
    // A long minimum delta-t: this cycle is sub-MDT.
    rec.mdt = 100.0;

    // The framework stages the OUTL readback before process().
    rec.put_field_internal("__OUTL_SEED", EpicsValue::Double(7.0))
        .unwrap();
    EpidSoftDeviceSupport::do_pid(&mut rec);

    assert_eq!(
        rec.i, 0.0,
        "a sub-MDT cycle must not commit the OUTL readback to I"
    );
    assert_eq!(
        rec.fbop, 0,
        "a sub-MDT cycle commits nothing, so the OFF->ON edge survives"
    );

    // Same staged value, now on a cycle that clears the MDT gate: seeded.
    rec.mdt = 0.0;
    std::thread::sleep(std::time::Duration::from_millis(5));
    EpidSoftDeviceSupport::do_pid(&mut rec);
    assert!(
        (rec.i - 7.0).abs() < 1e-9,
        "the post-MDT cycle seeds I from the OUTL readback, got {}",
        rec.i
    );
}

/// BUG 2 — a CONSTANT OUTL link on the MaxMin edge emits no readback:
/// C only reads OUTL when `outl.type != CONSTANT`
/// (`devEpidSoft.c:180`).
#[test]
fn test_maxmin_bumpless_edge_no_readback_for_constant_outl() {
    let mut rec = EpidRecord::default();
    rec.outl = "5.0".to_string(); // CONSTANT link
    assert_eq!(link_field_type(&rec.outl), LinkType::Constant);
    rec.fmod = 1; // MaxMin
    rec.fbon = 1;
    rec.fbop = 0; // OFF -> ON edge

    assert!(
        rec.pre_process_actions().is_empty(),
        "CONSTANT OUTL must not emit a MaxMin bumpless readback"
    );
}

/// BUG 2 — the MaxMin edge then uses the read-back OVAL. The
/// framework's `ReadDbLink{OUTL->OVAL}` has already populated
/// `epid.oval` by the time `do_pid` runs; `do_pid`'s MaxMin edge
/// outputs that value (devEpidSoft.c:181 `dbGetLink(...&oval...)`
/// then the output is set to it).
#[test]
fn test_maxmin_bumpless_edge_output_uses_readback_oval() {
    let mut rec = EpidRecord::default();
    rec.fmod = 1; // MaxMin
    rec.fbon = 1;
    rec.fbop = 0; // OFF -> ON edge
    rec.kp = 1.0;
    rec.drvh = 1000.0;
    rec.drvl = -1000.0;
    rec.mdt = 0.0;
    // Simulate the framework having applied ReadDbLink{OUTL->OVAL}:
    // the OUTL target's current value (7.0) is now in OVAL.
    rec.oval = 7.0;

    EpidSoftDeviceSupport::do_pid(&mut rec);

    assert_eq!(
        rec.oval, 7.0,
        "MaxMin bumpless edge output must be the OUTL read-back value"
    );
}
