#![allow(clippy::field_reassign_with_default)]
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use scaler_rs::device_support::scaler_asyn::ScalerDriver;
use scaler_rs::device_support::scaler_soft::SoftScalerDriver;
use scaler_rs::records::scaler::{MAX_SCALER_CHANNELS, ScalerRecord};

// ============================================================
// Record basics
// ============================================================

#[test]
fn test_record_type() {
    let rec = ScalerRecord::default();
    assert_eq!(rec.record_type(), "scaler");
}

#[test]
fn test_default_values() {
    let rec = ScalerRecord::default();
    assert_eq!(rec.val, 0.0);
    assert_eq!(rec.freq, 1.0e7);
    assert_eq!(rec.cnt, 0);
    assert_eq!(rec.cont, 0);
    assert_eq!(rec.tp, 1.0);
    assert_eq!(rec.tp1, 1.0);
    assert_eq!(rec.rate, 10.0);
    assert_eq!(rec.vers, 3.19);
    assert_eq!(rec.d[0], 1); // D1 default is "Dn"
    assert_eq!(rec.d[1], 0); // D2 default is "Up"
}

#[test]
fn test_as_any_mut() {
    let mut rec = ScalerRecord::default();
    assert!(rec.as_any_mut().is_some());
}

// ============================================================
// Field access — scalar fields
// ============================================================

#[test]
fn test_get_put_scalar_fields() {
    let mut rec = ScalerRecord::default();

    rec.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(42.0)));

    rec.put_field("FREQ", EpicsValue::Double(1e6)).unwrap();
    assert_eq!(rec.get_field("FREQ"), Some(EpicsValue::Double(1e6)));

    rec.put_field("CNT", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("CNT"), Some(EpicsValue::Short(1)));

    rec.put_field("CONT", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("CONT"), Some(EpicsValue::Short(1)));

    rec.put_field("TP", EpicsValue::Double(5.0)).unwrap();
    assert_eq!(rec.get_field("TP"), Some(EpicsValue::Double(5.0)));

    rec.put_field("EGU", EpicsValue::String("counts".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("EGU"),
        Some(EpicsValue::String("counts".into()))
    );
}

#[test]
fn test_read_only_scalar_fields() {
    let mut rec = ScalerRecord::default();
    assert!(rec.put_field("PCNT", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("SS", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("US", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("NCH", EpicsValue::Short(8)).is_err());
    assert!(rec.put_field("T", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("VERS", EpicsValue::Float(1.0)).is_err());
}

// ============================================================
// Field access — indexed fields (S1-S64, PR1-PR64, etc.)
// ============================================================

#[test]
fn test_get_put_indexed_s() {
    let rec = ScalerRecord::default();
    // S fields are read-only
    assert_eq!(rec.get_field("S1"), Some(EpicsValue::Long(0)));
    assert_eq!(rec.get_field("S64"), Some(EpicsValue::Long(0)));

    let mut rec = rec;
    assert!(rec.put_field("S1", EpicsValue::Long(100)).is_err());
}

#[test]
fn test_get_put_indexed_pr() {
    let mut rec = ScalerRecord::default();
    rec.put_field("PR1", EpicsValue::Long(1000000)).unwrap();
    assert_eq!(rec.get_field("PR1"), Some(EpicsValue::Long(1000000)));

    rec.put_field("PR64", EpicsValue::Long(500)).unwrap();
    assert_eq!(rec.get_field("PR64"), Some(EpicsValue::Long(500)));
}

#[test]
fn test_get_put_indexed_g() {
    let mut rec = ScalerRecord::default();
    rec.put_field("G1", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("G1"), Some(EpicsValue::Short(1)));

    rec.put_field("G32", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("G32"), Some(EpicsValue::Short(1)));
}

#[test]
fn test_get_put_indexed_d() {
    let mut rec = ScalerRecord::default();
    rec.put_field("D1", EpicsValue::Short(0)).unwrap();
    assert_eq!(rec.get_field("D1"), Some(EpicsValue::Short(0)));
}

#[test]
fn test_get_put_indexed_nm() {
    let mut rec = ScalerRecord::default();
    rec.put_field("NM1", EpicsValue::String("clock".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("NM1"),
        Some(EpicsValue::String("clock".into()))
    );

    rec.put_field("NM10", EpicsValue::String("det1".into()))
        .unwrap();
    assert_eq!(
        rec.get_field("NM10"),
        Some(EpicsValue::String("det1".into()))
    );
}

#[test]
fn test_indexed_field_out_of_range() {
    let rec = ScalerRecord::default();
    assert!(rec.get_field("S0").is_none()); // 0 is out of range (1-based)
    assert!(rec.get_field("S65").is_none()); // > 64
    assert!(rec.get_field("PR0").is_none());
    assert!(rec.get_field("G65").is_none());
}

#[test]
fn test_indexed_field_invalid_prefix() {
    let rec = ScalerRecord::default();
    assert!(rec.get_field("X1").is_none());
    assert!(rec.get_field("NONEXISTENT").is_none());
}

#[test]
fn test_type_mismatch() {
    let mut rec = ScalerRecord::default();
    assert!(
        rec.put_field("VAL", EpicsValue::String("bad".into()))
            .is_err()
    );
    assert!(
        rec.put_field("PR1", EpicsValue::String("bad".into()))
            .is_err()
    );
    assert!(rec.put_field("G1", EpicsValue::Double(1.0)).is_err());
    assert!(rec.put_field("NM1", EpicsValue::Double(1.0)).is_err());
}

// ============================================================
// TP ↔ PR1 conversion
// ============================================================

#[test]
fn test_tp_to_pr1_conversion() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 2.0; // 2 seconds
    rec.special("TP", true).unwrap();
    assert_eq!(rec.pr[0], 20_000_000); // 2.0 * 1e7
    assert_eq!(rec.d[0], 1); // Direction set
    assert_eq!(rec.g[0], 1); // Gate set
}

#[test]
fn test_pr1_to_tp_conversion() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.pr[0] = 10_000_000; // 1 second
    rec.special("PR1", true).unwrap();
    assert!((rec.tp - 1.0).abs() < 1e-6);
    assert_eq!(rec.d[0], 1);
    assert_eq!(rec.g[0], 1);
}

#[test]
fn test_init_record_tp_conversion() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e6;
    rec.tp = 3.0;
    rec.init_record(1).unwrap();
    assert_eq!(rec.pr[0], 3_000_000);
}

#[test]
fn test_init_record_default_freq() {
    let mut rec = ScalerRecord::default();
    rec.freq = 0.0;
    rec.init_record(1).unwrap();
    assert_eq!(rec.freq, 1e7);
}

// ============================================================
// special() handler
// ============================================================

#[test]
fn test_special_rate_clamp() {
    let mut rec = ScalerRecord::default();
    rec.rate = 100.0;
    rec.special("RATE", true).unwrap();
    assert_eq!(rec.rate, 60.0);

    rec.rate = -5.0;
    rec.special("RATE", true).unwrap();
    assert_eq!(rec.rate, 0.0);
}

#[test]
fn test_special_pr_auto_enables_gate() {
    let mut rec = ScalerRecord::default();
    rec.pr[4] = 5000; // PR5
    rec.special("PR5", true).unwrap();
    assert_eq!(rec.d[4], 1); // D5 set
    assert_eq!(rec.g[4], 1); // G5 set
}

#[test]
fn test_special_gate_sets_default_preset() {
    let mut rec = ScalerRecord::default();
    rec.g[2] = 1; // G3
    rec.pr[2] = 0; // No preset
    rec.special("G3", true).unwrap();
    assert_eq!(rec.pr[2], 1000); // Default preset
}

#[test]
fn test_special_gate_no_change_if_preset_exists() {
    let mut rec = ScalerRecord::default();
    rec.g[2] = 1;
    rec.pr[2] = 5000; // Already has preset
    rec.special("G3", true).unwrap();
    assert_eq!(rec.pr[2], 5000); // Unchanged
}

// ============================================================
// State machine
// ============================================================

#[test]
fn test_initial_state() {
    let rec = ScalerRecord::default();
    assert_eq!(rec.ss, 0); // IDLE
    assert_eq!(rec.us, 0); // IDLE
    assert_eq!(rec.cnt, 0); // Done
}

#[test]
fn test_process_idle_no_change() {
    let mut rec = ScalerRecord::default();
    rec.process().unwrap();
    assert_eq!(rec.ss, 0);
    assert_eq!(rec.us, 0);
}

#[test]
fn test_count_start_via_special() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();

    // Start counting
    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    assert_eq!(rec.us, 2); // REQSTART

    // Process to actually start
    rec.process().unwrap();
    assert_eq!(rec.ss, 2); // COUNTING
    assert_eq!(rec.us, 3); // COUNTING
}

#[test]
fn test_count_stop() {
    let mut rec = ScalerRecord::default();
    rec.ss = 2; // COUNTING
    rec.us = 3; // COUNTING
    rec.cnt = 1;
    rec.pcnt = 1;

    // Stop counting
    rec.cnt = 0;
    rec.process().unwrap();
    assert_eq!(rec.ss, 0); // IDLE
    assert_eq!(rec.us, 0); // IDLE
}

#[test]
fn test_update_time() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.s[0] = 5_000_000; // Half a second of counts
    rec.update_time();
    assert!((rec.t - 0.5).abs() < 1e-10);
}

#[test]
fn test_val_set_on_completion() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.ss = 2; // COUNTING
    rec.us = 3; // USER COUNTING
    rec.cnt = 1;
    rec.pcnt = 1;
    rec.s[0] = 10_000_000; // 1 second

    // Set up a gated channel that reached preset to trigger "done"
    rec.g[0] = 1;
    rec.pr[0] = 10_000_000;

    rec.process().unwrap();
    // Should detect done and set VAL = T
    assert_eq!(rec.ss, 0); // IDLE
    assert!(
        (rec.val - 1.0).abs() < 1e-6,
        "VAL should be ~1.0, got {}",
        rec.val
    );
}

// ============================================================
// Soft scaler driver
// ============================================================

#[test]
fn test_soft_driver_basics() {
    let driver = SoftScalerDriver::new(8);
    assert_eq!(driver.num_channels(), 8);
    assert!(!driver.done());
}

#[test]
fn test_soft_driver_reset() {
    let mut driver = SoftScalerDriver::new(8);
    driver.arm(true).unwrap();
    driver.reset().unwrap();
    assert!(!driver.done());
}

#[test]
fn test_soft_driver_arm_disarm() {
    let mut driver = SoftScalerDriver::new(8);
    driver.arm(true).unwrap();
    driver.arm(false).unwrap();
}

#[test]
fn test_soft_driver_write_preset() {
    let mut driver = SoftScalerDriver::new(8);
    driver.write_preset(0, 1000).unwrap();
    driver.write_preset(1, 2000).unwrap();
}

#[test]
fn test_soft_driver_read_counts() {
    let mut driver = SoftScalerDriver::new(8);

    // Write values via shared_counts
    let shared = driver.shared_counts();
    {
        let mut guard = shared.lock().unwrap();
        guard[0] = 500;
        guard[1] = 1000;
    }

    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();
    assert_eq!(counts[0], 500);
    assert_eq!(counts[1], 1000);
}

#[test]
fn test_soft_driver_preset_done() {
    let mut driver = SoftScalerDriver::new(8);
    driver.write_preset(0, 1000).unwrap();
    driver.arm(true).unwrap();

    // Simulate counting reaching preset
    let shared = driver.shared_counts();
    {
        let mut guard = shared.lock().unwrap();
        guard[0] = 1000;
    }

    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();
    assert!(driver.done());
}

#[test]
fn test_soft_driver_preset_not_reached() {
    let mut driver = SoftScalerDriver::new(8);
    driver.write_preset(0, 1000).unwrap();
    driver.arm(true).unwrap();

    let shared = driver.shared_counts();
    {
        let mut guard = shared.lock().unwrap();
        guard[0] = 500; // Not yet at preset
    }

    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();
    assert!(!driver.done());
}

// ============================================================
// Factory
// ============================================================

#[test]
fn test_scaler_record_factory() {
    let (name, factory) = scaler_rs::scaler_record_factory();
    assert_eq!(name, "scaler");
    let rec = factory();
    assert_eq!(rec.record_type(), "scaler");
}
