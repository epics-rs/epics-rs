#![allow(clippy::field_reassign_with_default)]
use epics_base_rs::server::record::Record;
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
    let (status, severity) = alarm.unwrap();
    assert_eq!(status, 3); // HIHI_ALARM
    assert_eq!(severity, 2);
}

#[test]
fn test_check_alarms_lolo() {
    let mut rec = EpidRecord::default();
    rec.lolo = 10.0;
    rec.llsv = 2;
    rec.val = 5.0;
    let alarm = rec.check_alarms();
    assert!(alarm.is_some());
    let (status, severity) = alarm.unwrap();
    assert_eq!(status, 4); // LOLO_ALARM
    assert_eq!(severity, 2);
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

    // First alarm triggers at 100
    rec.val = 100.0;
    rec.check_alarms();
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
