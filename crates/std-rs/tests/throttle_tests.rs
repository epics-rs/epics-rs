#![allow(clippy::field_reassign_with_default)]
use epics_base_rs::server::record::{ProcessAction, Record};
use epics_base_rs::types::EpicsValue;
use std_rs::ThrottleRecord;

#[test]
fn test_record_type() {
    let rec = ThrottleRecord::default();
    assert_eq!(rec.record_type(), "throttle");
}

#[test]
fn test_default_values() {
    let rec = ThrottleRecord::default();
    assert_eq!(rec.val, 0.0);
    assert_eq!(rec.dly, 0.0);
    assert_eq!(rec.drvlh, 0.0);
    assert_eq!(rec.drvll, 0.0);
    assert_eq!(rec.drvlc, 0); // Off
    assert_eq!(rec.wait, 0); // False
    assert_eq!(rec.sts, 0); // Unknown
}

// ============================================================
// Field access
// ============================================================

#[test]
fn test_get_put_val() {
    let mut rec = ThrottleRecord::default();
    rec.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(42.0)));
}

#[test]
fn test_get_put_dly() {
    let mut rec = ThrottleRecord::default();
    rec.put_field("DLY", EpicsValue::Double(1.5)).unwrap();
    assert_eq!(rec.get_field("DLY"), Some(EpicsValue::Double(1.5)));
}

#[test]
fn test_get_put_limits() {
    let mut rec = ThrottleRecord::default();
    rec.put_field("DRVLH", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DRVLL", EpicsValue::Double(0.0)).unwrap();
    assert_eq!(rec.get_field("DRVLH"), Some(EpicsValue::Double(100.0)));
    assert_eq!(rec.get_field("DRVLL"), Some(EpicsValue::Double(0.0)));
}

#[test]
fn test_read_only_fields() {
    let mut rec = ThrottleRecord::default();
    assert!(rec.put_field("OVAL", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("SENT", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("OSENT", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("WAIT", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("DRVLS", EpicsValue::Short(1)).is_err());
    assert!(
        rec.put_field("VER", EpicsValue::String("x".into()))
            .is_err()
    );
    assert!(rec.put_field("STS", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("OV", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("SIV", EpicsValue::Short(1)).is_err());
}

#[test]
fn test_type_mismatch() {
    let mut rec = ThrottleRecord::default();
    assert!(
        rec.put_field("VAL", EpicsValue::String("bad".into()))
            .is_err()
    );
    assert!(rec.put_field("PREC", EpicsValue::Double(1.0)).is_err());
}

#[test]
fn test_unknown_field() {
    let rec = ThrottleRecord::default();
    assert!(rec.get_field("NONEXISTENT").is_none());
    let mut rec = rec;
    assert!(
        rec.put_field("NONEXISTENT", EpicsValue::Double(1.0))
            .is_err()
    );
}

// ============================================================
// Process — basic output
// ============================================================

#[test]
fn test_process_sends_value_no_delay() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0; // No delay
    rec.val = 42.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 42.0);
    assert_eq!(rec.sts, 2); // Success
    assert_eq!(rec.wait, 0); // Not busy (no delay)
}

#[test]
fn test_process_sends_value_with_delay() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 1.0; // 1 second delay
    rec.val = 42.0;
    let outcome = rec.process().unwrap();
    assert_eq!(rec.sent, 42.0);
    assert_eq!(rec.wait, 1); // Busy during delay
    // Should have ReprocessAfter action and WriteDbLink for OUT
    let has_reprocess = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::ReprocessAfter(_)));
    assert!(has_reprocess, "Should have ReprocessAfter action");
    let has_write = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::WriteDbLink { .. }));
    assert!(has_write, "Should have WriteDbLink action for OUT");
}

#[test]
fn test_process_queues_during_delay() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 10.0; // Long delay
    rec.val = 42.0;
    rec.process().unwrap(); // First value sent, delay starts
    assert_eq!(rec.sent, 42.0);
    assert_eq!(rec.wait, 1);

    // Second value during delay — should be queued
    rec.val = 99.0;
    let outcome = rec.process().unwrap();
    let has_reprocess = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::ReprocessAfter(_)));
    assert!(
        has_reprocess,
        "Should have ReprocessAfter for pending drain"
    );
    assert_eq!(rec.sent, 42.0); // Not sent yet — still in delay
}

#[test]
fn test_process_updates_oval() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0;
    rec.val = 10.0;
    rec.process().unwrap();
    assert_eq!(rec.oval, 10.0);

    rec.val = 20.0;
    rec.process().unwrap();
    assert_eq!(rec.oval, 20.0);
}

#[test]
fn test_process_osent_tracking() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0;
    rec.val = 10.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 10.0);
    assert_eq!(rec.osent, 0.0); // Previous sent was 0

    rec.val = 20.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 20.0);
    assert_eq!(rec.osent, 10.0); // Previous sent was 10
}

// ============================================================
// Limit checking
// ============================================================

#[test]
fn test_limit_clipping_on() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.drvlc = 1; // Clipping ON
    rec.dly = 0.0; // No delay for immediate send
    rec.init_record(1).unwrap();

    rec.val = 150.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 100.0);
    assert_eq!(rec.drvls, 2); // High limit
    assert_eq!(rec.sts, 2); // Success (clamped but sent)
}

#[test]
fn test_limit_clipping_low() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 10.0;
    rec.drvlc = 1; // Clipping ON
    rec.dly = 0.0;
    rec.init_record(1).unwrap();

    rec.val = 5.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 10.0);
    assert_eq!(rec.drvls, 1); // Low limit
}

#[test]
fn test_limit_rejection() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.drvlc = 0; // Clipping OFF → reject
    rec.dly = 0.0;
    rec.init_record(1).unwrap();

    rec.oval = 50.0; // Previous good value
    rec.val = 150.0; // Out of range
    rec.process().unwrap();
    assert_eq!(rec.val, 50.0);
    assert_eq!(rec.sts, 1); // Error
}

#[test]
fn test_no_limits_when_equal() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 0.0;
    rec.drvll = 0.0; // Equal → limits disabled
    rec.dly = 0.0;
    rec.init_record(1).unwrap();

    rec.val = 999.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 999.0);
    assert_eq!(rec.drvls, 0); // Normal
}

// ============================================================
// special() handler
// ============================================================

#[test]
fn test_special_dly_clamp_negative() {
    let mut rec = ThrottleRecord::default();
    rec.dly = -5.0;
    rec.special("DLY", true).unwrap();
    assert_eq!(rec.dly, 0.0);
}

#[test]
fn test_special_dly_positive() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 2.5;
    rec.special("DLY", true).unwrap();
    assert_eq!(rec.dly, 2.5); // Unchanged
}

#[test]
fn test_special_drvlh_drvll_enables_limits() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.special("DRVLH", true).unwrap();
    // limit_flag should be set internally

    rec.val = 150.0;
    rec.process().unwrap();
    // With drvlc=0 (default off), value is rejected
    assert_eq!(rec.sts, 1); // Error — rejected
}

#[test]
fn test_sync_pre_process_actions_and_reset() {
    use epics_base_rs::server::record::Record;

    let mut rec = ThrottleRecord::default();
    rec.sync = 1; // Process

    // pre_process_actions() returns ReadDbLink for SINP→VAL
    // and resets sync to 0.
    let actions = rec.pre_process_actions();
    assert_eq!(actions.len(), 1, "Should have one ReadDbLink action");
    assert_eq!(
        rec.sync, 0,
        "sync should be reset after pre_process_actions"
    );

    // Calling again with sync=0 returns empty
    let actions = rec.pre_process_actions();
    assert!(actions.is_empty());
}

// ============================================================
// can_device_write
// ============================================================

#[test]
fn test_can_device_write() {
    let rec = ThrottleRecord::default();
    assert!(rec.can_device_write());
}
