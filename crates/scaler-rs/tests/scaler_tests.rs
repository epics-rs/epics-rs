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
    // scalerRecord.dbd: TP has no `initial` — raw default is 0.0.
    // The 1.0 default is applied by init_record (scalerRecord.c:320-323).
    assert_eq!(rec.tp, 0.0);
    // scalerRecord.dbd: TP1 `initial("1")`.
    assert_eq!(rec.tp1, 1.0);
    // scalerRecord.dbd: RATE `initial("10")`.
    assert_eq!(rec.rate, 10.0);
    assert_eq!(rec.vers, 3.19);
    // scalerRecord.dbd: D1 `initial("1")` (Dn); D2..D64 default 0 (Up).
    assert_eq!(rec.d[0], 1);
    assert_eq!(rec.d[1], 0);
    // scalerRecord.dbd: G1 `initial("1")` (Y); G2..G64 default 0 (N).
    assert_eq!(rec.g[0], 1);
    assert_eq!(rec.g[1], 0);
}

/// init_record applies the both-zero TP/PR1 rule (scalerRecord.c:320-323):
/// with the dbd default (TP=0, PR1=0) the count time becomes 1.0 s.
#[test]
fn test_init_record_applies_default_count_time() {
    let mut rec = ScalerRecord::default();
    assert_eq!(rec.tp, 0.0);
    rec.init_record(1).unwrap();
    assert_eq!(rec.tp, 1.0);
    assert_eq!(rec.pr[0], 10_000_000); // 1.0 s * 1e7 Hz
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
    // S fields are read-only and DBF_ULONG (scalerRecord.dbd:1334-1649).
    assert_eq!(rec.get_field("S1"), Some(EpicsValue::ULong(0)));
    assert_eq!(rec.get_field("S64"), Some(EpicsValue::ULong(0)));

    let mut rec = rec;
    assert!(rec.put_field("S1", EpicsValue::ULong(100)).is_err());
}

#[test]
fn test_get_put_indexed_pr() {
    let mut rec = ScalerRecord::default();
    // PR1..PR64 are DBF_ULONG (scalerRecord.dbd:945-1323); the legacy signed
    // Long put is tolerated, the read-back is the native ULong.
    rec.put_field("PR1", EpicsValue::Long(1000000)).unwrap();
    assert_eq!(rec.get_field("PR1"), Some(EpicsValue::ULong(1000000)));

    rec.put_field("PR64", EpicsValue::ULong(500)).unwrap();
    assert_eq!(rec.get_field("PR64"), Some(EpicsValue::ULong(500)));
}

// DBF_ULONG high-bit round-trip: a PR/S count >= 2^31 must survive without
// sign loss. PR1..PR64 and S1..S64 are DBF_ULONG (scalerRecord.dbd:945-1649).
#[test]
fn test_scaler_pr_s_high_bit_round_trip() {
    let mut rec = ScalerRecord::default();
    rec.put_field("PR1", EpicsValue::ULong(0x8000_0000))
        .unwrap();
    assert_eq!(rec.get_field("PR1"), Some(EpicsValue::ULong(0x8000_0000)));
    // S1 is read-only over put_field; device support fills it directly.
    rec.s[0] = 0xDEAD_BEEF;
    assert_eq!(rec.get_field("S1"), Some(EpicsValue::ULong(0xDEAD_BEEF)));
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

// C `scalerRecord.c:471,770-772` — `process()` calls `monitor()` (the
// S1..Snch DBE_LOG sweep) ONLY while `ss == SCALER_STATE_IDLE`. So the
// record advertises the active-channel sweep set only when idle, and an
// empty set while counting or waiting.
#[test]
fn test_log_swept_fields_idle_active_channels_only() {
    let mut rec = ScalerRecord::default();
    rec.nch = 3;

    rec.ss = 0; // IDLE — sweeps exactly the active channels S1..S3.
    assert_eq!(rec.log_swept_fields(), &["S1", "S2", "S3"]);

    rec.ss = 2; // COUNTING — C does not call monitor(); no sweep.
    assert_eq!(rec.log_swept_fields(), &[] as &[&str]);

    rec.ss = 1; // WAITING — also not idle; no sweep.
    assert_eq!(rec.log_swept_fields(), &[] as &[&str]);

    // nch is clamped to the channel cap, never out of bounds.
    rec.ss = 0;
    rec.nch = (MAX_SCALER_CHANNELS as i16) + 5;
    assert_eq!(rec.log_swept_fields().len(), MAX_SCALER_CHANNELS);
}

// Every C `scalerRecord.c` post outside the idle `monitor()` sweep is a literal
// `DBE_VALUE`: the process/updateCounts posts of CNT/T/VAL/PR1/TP/FREQ and the
// active channels' Sn (:316/322/329/334/372/425/427/430/478/530/582/588), AND
// the `special()` posts of PRn/Gn/Dn (:673-676, :682-687, :705-706, :719). The
// only `DBE_LOG` post is the idle sweep of S1..Snch (line 771). The record
// advertises all of them as value-only so the framework strips the LOG bit —
// which for PRn/Gn/Dn means a DBE_LOG subscriber never sees them at all, as in C.
#[test]
fn test_value_only_change_fields_scalars_plus_active_channels() {
    let mut rec = ScalerRecord::default();
    rec.nch = 3;

    // Counting, waiting, or idle: the value-only set is identical (C's posts are
    // DBE_VALUE regardless of the record's state).
    for ss in [0i16, 1, 2] {
        rec.ss = ss;
        let set = rec.value_only_change_fields();
        for f in [
            "CNT", "T", "VAL", "PR1", "TP", "FREQ", // the six scalars
            "S1", "S2", "S3", // active channels
            "PR2", "PR3", // remaining presets (PR1 is a scalar above)
            "G1", "G2", "G3", "D1", "D2", "D3", // gates + directions
        ] {
            assert!(set.contains(&f), "{f} must be value-only (ss={ss})");
        }
        // Inactive channels contribute nothing.
        for f in ["S4", "PR4", "G4", "D4"] {
            assert!(!set.contains(&f), "{f} is beyond nch=3");
        }
        assert_eq!(set.len(), 17, "6 scalars + 3 Sn + 2 PRn + 3 Gn + 3 Dn");
    }

    // Zero active channels → just the six fixed scalar fields.
    rec.nch = 0;
    assert_eq!(
        rec.value_only_change_fields(),
        &["CNT", "T", "VAL", "PR1", "TP", "FREQ"]
    );

    // nch clamped to the channel cap; never out of bounds.
    rec.nch = (MAX_SCALER_CHANNELS as i16) + 5;
    assert_eq!(
        rec.value_only_change_fields().len(),
        6 + 4 * MAX_SCALER_CHANNELS - 1,
        "6 scalars + (Sn + Gn + Dn) per channel + PRn for every channel but PR1"
    );
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

/// C scalerRecord.c:367 — the record learns counting finished from
/// device support's `done()` (here `done_flag`), NOT by inspecting
/// presets itself. On a user count completing, process() sets CNT=0,
/// us=IDLE, ss=IDLE and (scalerRecord.c:475-479) copies VAL = T.
#[test]
fn test_val_set_on_completion() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.ss = 2; // COUNTING
    rec.us = 3; // USER COUNTING
    rec.cnt = 1;
    rec.pcnt = 1;
    rec.s[0] = 10_000_000; // 1 second of clock ticks

    // Device support's read() marks counting done before process() runs.
    rec.set_done();

    rec.process().unwrap();
    // process() detects done, finishes the user count, sets VAL = T.
    assert_eq!(rec.ss, 0); // IDLE
    assert_eq!(rec.us, 0); // IDLE
    assert_eq!(rec.cnt, 0); // user count cleared
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
    let mut driver = SoftScalerDriver::new(8);
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

// C `drvScalerSoft.c:319-322` clears the counts on EVERY arm command,
// disarm included, before setting `acquiring`. A disarm must therefore
// zero what a subsequent idle read returns.
#[test]
fn test_soft_driver_disarm_clears_counts() {
    let mut driver = SoftScalerDriver::new(8);
    // Simulate accumulated counts via the external source.
    {
        let shared = driver.shared_counts();
        let mut guard = shared.lock().unwrap();
        guard[0] = 1234;
        guard[1] = 5678;
    }
    // A read latches them into the driver's local buffer.
    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();
    assert_eq!(counts[0], 1234);
    assert_eq!(counts[1], 5678);

    // Disarm: C clears unconditionally — the next read must return 0.
    driver.arm(false).unwrap();
    let mut after = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut after).unwrap();
    assert_eq!(after[0], 0, "disarm must zero the counts (C parity)");
    assert_eq!(after[1], 0, "disarm must zero the counts (C parity)");
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

/// C `devScalerAsyn.c:292-301` `scaler_done()` is read-and-clear: it
/// returns 1 exactly once per completed count, then `pPvt->done` is 0
/// so the next poll returns 0. `ScalerDriver::done` must consume the
/// flag the same way.
#[test]
fn test_soft_driver_done_is_read_and_clear() {
    let mut driver = SoftScalerDriver::new(8);
    driver.write_preset(0, 1000).unwrap();
    driver.arm(true).unwrap();

    let shared = driver.shared_counts();
    {
        let mut guard = shared.lock().unwrap();
        guard[0] = 1000;
    }

    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();

    // First poll reports the completed count.
    assert!(driver.done(), "first done() poll must report completion");
    // The flag is cleared — a second poll without a new count is false.
    assert!(
        !driver.done(),
        "done() must clear the flag (C scaler_done read-and-clear)"
    );
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

// ============================================================
// BUG 3 regression: nch out of range must not panic
// ============================================================

/// A driver reporting more channels than the record's fixed array bound.
/// Device support sets `nch` from `num_channels()`; an unclamped value
/// would index the 64-element `g`/`pr` arrays out of bounds.
#[test]
fn test_count_start_does_not_panic_when_nch_exceeds_max() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();

    // A custom driver could report far more channels than the array holds.
    rec.nch = 200;
    // Gate channel 0 so the start sequence builds at least one preset action.
    rec.g[0] = 1;

    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    // process() calls build_start_actions, which iterates 0..nch and indexes
    // g[i]/pr[i]. Must not panic with nch > MAX_SCALER_CHANNELS.
    rec.process().unwrap();
    assert_eq!(rec.ss, 2); // COUNTING — start sequence completed
}

/// Negative `nch` (i16 wrap) must not produce a huge `usize` loop bound.
#[test]
fn test_count_start_does_not_panic_when_nch_negative() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();

    rec.nch = -1;
    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.ss, 2); // COUNTING — no panic, loop bound clamped to 0
}

/// Auto-count start path (`build_autocount_actions`) with an oversized nch.
#[test]
fn test_autocount_start_does_not_panic_when_nch_exceeds_max() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.nch = 128;
    rec.g[0] = 1;
    rec.pr[0] = 1000;
    // tp1 below the auto-PR1 threshold so the per-channel preset loop runs.
    rec.tp1 = 0.0;
    rec.cont = 1; // CONT mode triggers the auto-count path

    rec.process().unwrap();
    // No panic; build_autocount_actions iterated a clamped channel range.
}

/// Device support clamps `num_channels()` so `nch` is in range.
#[test]
fn test_asyn_device_support_clamps_nch() {
    use epics_base_rs::server::device_support::DeviceSupport;
    use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;

    struct WideDriver;
    impl ScalerDriver for WideDriver {
        fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
            Ok(())
        }
        fn arm(&mut self, _start: bool) -> epics_base_rs::error::CaResult<()> {
            Ok(())
        }
        fn write_preset(
            &mut self,
            _channel: usize,
            preset: u32,
        ) -> epics_base_rs::error::CaResult<u32> {
            Ok(preset)
        }
        fn read(
            &mut self,
            _counts: &mut [u32; MAX_SCALER_CHANNELS],
        ) -> epics_base_rs::error::CaResult<()> {
            Ok(())
        }
        fn done(&mut self) -> bool {
            false
        }
        fn num_channels(&self) -> usize {
            999
        }
    }

    let mut support = ScalerAsynDeviceSupport::new(Box::new(WideDriver));
    let mut rec = ScalerRecord::default();
    support.init(&mut rec).unwrap();
    assert_eq!(rec.nch as usize, MAX_SCALER_CHANNELS);
}

// ============================================================
// PR1 / TP conversion consistency
// ============================================================

/// TP -> PR1 conversion rounding differs between code paths in the C
/// record, and the port must reproduce that exactly:
///
/// - `special()` TP handler — scalerRecord.c:672 — truncating cast
///   `(epicsUInt32)(tp * freq)`.
/// - `process()` REQSTART path — scalerRecord.c:409-410 — `NINT`
///   (round-to-nearest).
///
/// For `tp * freq` with a fractional part of 0.5 the two paths produce
/// values that differ by one tick.
#[test]
fn test_tp_to_pr1_special_truncates_process_rounds() {
    // 1.000_000_05 s * 1e7 Hz = 10_000_000.5 ticks.
    let tp = 1.000_000_05;

    // special() TP — truncating: 10_000_000.5 -> 10_000_000.
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = tp;
    rec.special("TP", true).unwrap();
    assert_eq!(rec.pr[0], 10_000_000, "special() TP must truncate (C:672)");

    // process() REQSTART — NINT: 10_000_000.5 -> 10_000_001.
    let mut rec2 = ScalerRecord::default();
    rec2.freq = 1e7;
    rec2.tp = tp;
    rec2.init_record(1).unwrap();
    rec2.cnt = 1;
    rec2.special("CNT", true).unwrap();
    rec2.process().unwrap();
    assert_eq!(
        rec2.pr[0], 10_000_001,
        "process() REQSTART must round-to-nearest (C:409-410)"
    );
}

/// A very large TP must saturate `pr[0]` at `u32::MAX` rather than wrap.
#[test]
fn test_tp_to_pr1_saturates_on_large_tp() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1e30; // tp * freq overflows u32 by many orders of magnitude
    rec.special("TP", true).unwrap();
    assert_eq!(rec.pr[0], u32::MAX);
}

// ============================================================
// C-parity regression tests (this audit)
// ============================================================

/// C scalerRecord.c:670-677 — special() TP truncates `tp * freq`
/// and unconditionally sets D1 = G1 = 1.
#[test]
fn test_special_tp_truncates() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    // 1.9 ticks of fractional part: 0.999_999_99 s * 1e7 = 9_999_999.9.
    rec.tp = 0.999_999_99;
    rec.special("TP", true).unwrap();
    assert_eq!(rec.pr[0], 9_999_999, "special() TP must truncate");
    assert_eq!(rec.d[0], 1);
    assert_eq!(rec.g[0], 1);
}

/// C scalerRecord.c has NO special() case for RAT1 — putting RAT1 must
/// NOT clamp it (only RATE is clamped, scalerRecord.c:690-693).
#[test]
fn test_special_rat1_not_clamped() {
    let mut rec = ScalerRecord::default();
    rec.rat1 = 100.0;
    rec.special("RAT1", true).unwrap();
    assert_eq!(rec.rat1, 100.0, "RAT1 has no special() handler in C");
}

/// C scalerRecord.c:367 — process() polls device support's done()
/// every cycle. A done report while the user is counting clears CNT,
/// returns US/SS to IDLE, and finishes the user count.
#[test]
fn test_process_done_detection_unconditional() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.ss = 2; // COUNTING
    rec.us = 3; // USER_COUNTING
    rec.cnt = 1;
    rec.pcnt = 1;
    rec.set_done();
    rec.process().unwrap();
    assert_eq!(rec.ss, 0); // IDLE
    assert_eq!(rec.us, 0); // IDLE
    assert_eq!(rec.cnt, 0); // user count cleared (C:371)
}

/// C scalerRecord.c:369-376 — an auto-count cycle is NOT allowed to
/// reset CNT. When done() fires during an auto-count (us != COUNTING),
/// ss returns to IDLE but CNT is untouched.
#[test]
fn test_process_done_during_autocount_does_not_clear_cnt() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.ss = 2; // COUNTING
    rec.us = 0; // IDLE — this is an auto-count, not a user count
    rec.cnt = 0;
    rec.set_done();
    rec.process().unwrap();
    assert_eq!(rec.ss, 0); // IDLE
    assert_eq!(rec.cnt, 0); // unchanged
}

/// C scalerRecord.c:571-575 — while US == WAITING, updateCounts()
/// forces the displayed scaler values to 0; T is recomputed from S1.
#[test]
fn test_process_zeroes_counts_while_waiting() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.dly = 100.0; // long delay so we stay WAITING
    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    assert_eq!(rec.us, 1); // WAITING

    // Device support left stale counts in S1..; process() must zero them.
    rec.s[0] = 12_345;
    rec.s[3] = 999;
    rec.process().unwrap();
    assert_eq!(rec.s[0], 0, "S1 zeroed while WAITING");
    assert_eq!(rec.s[3], 0, "S4 zeroed while WAITING");
    assert_eq!(rec.t, 0.0, "T recomputed from zeroed S1");
}

/// C scalerRecord.c:487-490 — after a user count finishes, the
/// auto-count hold time is `MAX(dly1, scaler_wait_time)` (>= 10 s),
/// not the raw DLY1. The record enters SCALER_STATE_WAITING.
#[test]
fn test_autocount_uses_long_hold_after_user_count() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.cont = 1; // AutoCount enabled
    rec.dly1 = 0.0; // raw auto-delay is zero
    rec.ss = 2; // COUNTING (a user count in progress)
    rec.us = 3; // USER_COUNTING
    rec.cnt = 1;
    rec.pcnt = 1;

    // User count completes this cycle.
    rec.set_done();
    rec.process().unwrap();

    // just_finished_user_count -> dly_sec = MAX(0, 10) = 10 -> WAITING.
    assert_eq!(
        rec.ss, 1,
        "auto-count must wait (SCALER_STATE_WAITING), not start immediately"
    );
    assert_eq!(rec.us, 0); // IDLE
}

/// C scalerRecord.c:485-540 — with CONT set and no delay, auto-count
/// starts immediately (SCALER_STATE_COUNTING) on an idle process cycle.
#[test]
fn test_autocount_starts_immediately_when_no_delay() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.cont = 1;
    rec.dly1 = 0.0;
    rec.tp1 = 1.0;
    rec.process().unwrap();
    assert_eq!(rec.ss, 2, "auto-count starts immediately with DLY1=0");
}

/// C drvScalerSoft.c:303-313 — scalerResetCommand clears all presets.
#[test]
fn test_soft_driver_reset_clears_presets() {
    let mut driver = SoftScalerDriver::new(8);
    driver.write_preset(0, 1000).unwrap();
    driver.arm(true).unwrap();
    driver.reset().unwrap();
    // After reset the preset is gone, so a count that previously would
    // have completed no longer triggers done.
    let shared = driver.shared_counts();
    {
        let mut g = shared.lock().unwrap();
        g[0] = 5000;
    }
    driver.arm(true).unwrap();
    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();
    assert!(!driver.done(), "no preset after reset -> never done");
}

/// C drvScalerSoft.c:315-329 — scalerArmCommand clears the scaler data
/// so a stale count cannot make the scaler report done immediately.
#[test]
fn test_soft_driver_arm_clears_counts() {
    let mut driver = SoftScalerDriver::new(8);
    driver.write_preset(0, 1000).unwrap();

    // A stale count above the preset is sitting in shared state.
    let shared = driver.shared_counts();
    {
        let mut g = shared.lock().unwrap();
        g[0] = 5000;
    }

    // Arming must wipe that stale count, so the first read is not "done".
    driver.arm(true).unwrap();
    let mut counts = [0u32; MAX_SCALER_CHANNELS];
    driver.read(&mut counts).unwrap();
    assert_eq!(counts[0], 0, "arm() cleared the stale count");
    assert!(!driver.done(), "arm() prevented an immediate done");
}

// ============================================================
// REQSTART driver write-back reconciliation
// C scalerRecord.c:405-432 — a driver that owns its clock (Joerger
// VS64) adjusts the preset and reports its own frequency; the record
// must reconcile PR1/TP/FREQ with what the driver actually programmed.
// ============================================================

/// A hardware-like scaler driver that QUANTIZES the channel-0 preset
/// and runs at a clock frequency it chooses itself — the Joerger VS64
/// behaviour documented at scalerRecord.c:397-404.
///
/// `write_preset` for channel 0 rounds the requested preset up to the
/// next multiple of `QUANTUM` and returns that value. `actual_frequency`
/// reports a clock different from the record's requested 1e7.
struct QuantizingDriver {
    /// Number of channel-0 write_preset calls, shared so the test can
    /// observe it after the driver is moved into device support — used
    /// to prove the C `save_pr1 != pr1` second write happened.
    ch0_writes: std::sync::Arc<std::sync::Mutex<u32>>,
}

impl QuantizingDriver {
    /// Preset granularity — channel-0 presets are rounded up to a
    /// multiple of this.
    const QUANTUM: u32 = 4096;
    /// The clock the driver runs at, different from the record's
    /// default 1e7 so the FREQ reconciliation is observable.
    const DRIVER_FREQ: f64 = 1.25e7;

    fn new() -> Self {
        Self {
            ch0_writes: std::sync::Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Shared handle to the channel-0 write counter.
    fn ch0_writes_handle(&self) -> std::sync::Arc<std::sync::Mutex<u32>> {
        std::sync::Arc::clone(&self.ch0_writes)
    }

    fn quantize(preset: u32) -> u32 {
        preset.div_ceil(Self::QUANTUM) * Self::QUANTUM
    }
}

impl ScalerDriver for QuantizingDriver {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        *self.ch0_writes.lock().unwrap() = 0;
        Ok(())
    }
    fn read(
        &mut self,
        _counts: &mut [u32; MAX_SCALER_CHANNELS],
    ) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn write_preset(&mut self, channel: usize, preset: u32) -> epics_base_rs::error::CaResult<u32> {
        if channel == 0 {
            *self.ch0_writes.lock().unwrap() += 1;
            Ok(Self::quantize(preset))
        } else {
            Ok(preset)
        }
    }
    fn arm(&mut self, _start: bool) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn actual_frequency(&self) -> Option<f64> {
        Some(Self::DRIVER_FREQ)
    }
    fn done(&mut self) -> bool {
        false
    }
    fn num_channels(&self) -> usize {
        8
    }
}

/// Dispatch the actions a record's process() returned through device
/// support, mirroring `Database::execute_process_actions`' handling of
/// `ProcessAction::DeviceCommand` (processing.rs DeviceCommand arm).
///
/// Returns the accumulated record-field names every `handle_command`
/// reported as changed — these are exactly the fields the framework
/// posts `DBE_VALUE` monitor events for (C `db_post_events`,
/// `scalerRecord.c:425-430`).
fn run_device_commands(
    support: &mut dyn epics_base_rs::server::device_support::DeviceSupport,
    rec: &mut ScalerRecord,
    actions: &[epics_base_rs::server::record::ProcessAction],
) -> Vec<&'static str> {
    use epics_base_rs::server::record::ProcessAction;
    let mut posted = Vec::new();
    for action in actions {
        if let ProcessAction::DeviceCommand { command, args } = action {
            posted.extend(support.handle_command(rec, command, args).unwrap());
        }
    }
    posted
}

/// C scalerRecord.c:405-432 — REQSTART: after the per-channel
/// write_preset loop, a driver that quantized preset 0 leaves
/// `save_pr1 != pr1`, so the record recalculates PR1 from TP/FREQ,
/// re-writes preset 0, and recomputes TP from the effective PR1/FREQ.
/// The driver also reports its own clock, which must land in FREQ.
#[test]
fn test_reqstart_reconciles_pr1_tp_freq_with_adjusting_driver() {
    use epics_base_rs::server::device_support::DeviceSupport;
    use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;

    let driver = QuantizingDriver::new();
    let ch0_writes = driver.ch0_writes_handle();
    let mut support = ScalerAsynDeviceSupport::new(Box::new(driver));

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0; // requested count time 1 s
    support.init(&mut rec).unwrap(); // nch <- 8
    rec.init_record(1).unwrap();

    // Requested PR1 from TP*FREQ at the record's default clock.
    // 1.0 s * 1e7 Hz = 10_000_000 ticks, which is NOT a multiple of
    // QUANTUM (4096), so the driver will quantize it.
    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    assert_eq!(rec.us, 2, "CNT request -> REQSTART");

    let outcome = rec.process().unwrap();
    // process() left us/ss in COUNTING and emitted CMD_RESET + CMD_START_COUNT.
    assert_eq!(rec.ss, 2, "COUNTING");
    assert_eq!(rec.us, 3, "COUNTING");

    // Dispatch the device commands — this runs run_start_count.
    run_device_commands(&mut support, &mut rec, &outcome.actions);

    // The driver adopted its own clock frequency.
    assert_eq!(
        rec.freq,
        QuantizingDriver::DRIVER_FREQ,
        "FREQ must reflect the driver's actual clock (C:399-403,429-430)"
    );

    // PR1 must equal what the driver actually programmed: the record
    // recomputed PR1 = NINT(tp * driver_freq) and the driver quantized
    // that up to the next QUANTUM multiple.
    // tp=1.0, driver_freq=1.25e7 -> NINT = 12_500_000;
    // quantize(12_500_000) = ceil(12_500_000/4096)*4096 = 12_500_992.
    let expected_pr1 = QuantizingDriver::quantize(12_500_000);
    assert_eq!(
        rec.pr[0], expected_pr1,
        "PR1 must equal the driver-programmed (quantized) preset (C:420-425)"
    );
    assert_ne!(
        rec.pr[0], 10_000_000,
        "PR1 must NOT remain the stale pre-write value"
    );

    // TP must be recomputed from the effective PR1 / FREQ (C:426-427).
    let expected_tp = expected_pr1 as f64 / QuantizingDriver::DRIVER_FREQ;
    assert!(
        (rec.tp - expected_tp).abs() < 1e-12,
        "TP must be recomputed from effective PR1/FREQ (C:426-427): got {}, want {}",
        rec.tp,
        expected_tp
    );

    // The driver's channel-0 preset must have been written twice:
    // once in the per-channel loop, once in the C:422 re-write after
    // the record detected the adjustment.
    assert_eq!(
        *ch0_writes.lock().unwrap(),
        2,
        "driver-adjusted preset must trigger the C:422 second write_preset"
    );
}

/// C scalerRecord.c:508-535 — auto-count: the driver-adjustment
/// re-write applies (C:514-522), the driver clock is adopted into FREQ
/// (C:530), but the user's PR1 is RESTORED afterward (C:532) and TP is
/// NOT recomputed.
#[test]
fn test_autocount_restores_user_pr1_after_driver_adjustment() {
    use epics_base_rs::server::device_support::DeviceSupport;
    use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;

    let driver = QuantizingDriver::new();
    let ch0_writes = driver.ch0_writes_handle();
    let mut support = ScalerAsynDeviceSupport::new(Box::new(driver));

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp1 = 1.0; // auto-count time 1 s, >= 1ms threshold
    support.init(&mut rec).unwrap();

    // The user's PR1 — auto-count must NOT disturb it.
    let user_pr1 = 777_777u32;
    rec.pr[0] = user_pr1;
    let user_tp = rec.tp;

    rec.cont = 1; // CONT mode
    rec.dly1 = 0.0; // start immediately
    let outcome = rec.process().unwrap();
    assert_eq!(rec.ss, 2, "auto-count COUNTING");

    run_device_commands(&mut support, &mut rec, &outcome.actions);

    // C:532 — user's channel-1 preset is restored.
    assert_eq!(
        rec.pr[0], user_pr1,
        "auto-count must restore the user's PR1 (C:532)"
    );
    // C auto-count does not recompute TP.
    assert_eq!(rec.tp, user_tp, "auto-count must not recompute TP");
    // C:530 — the driver clock is still adopted into FREQ.
    assert_eq!(
        rec.freq,
        QuantizingDriver::DRIVER_FREQ,
        "auto-count must adopt the driver clock into FREQ (C:530)"
    );

    // The driver was written twice for channel 0: the tp1*freq write
    // plus the C:521 re-write after it detected the quantization.
    assert_eq!(
        *ch0_writes.lock().unwrap(),
        2,
        "auto-count driver adjustment must trigger the C:521 second write_preset"
    );
}

/// A non-adjusting driver (SoftScalerDriver semantics) must leave
/// PR1/TP/FREQ exactly as the record set them — the reconciliation is
/// a no-op when `write_preset` returns the requested value unchanged.
#[test]
fn test_reqstart_no_reconciliation_when_driver_does_not_adjust() {
    use epics_base_rs::server::device_support::DeviceSupport;
    use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;

    let mut support = ScalerAsynDeviceSupport::new(Box::new(SoftScalerDriver::new(8)));

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    support.init(&mut rec).unwrap();
    rec.init_record(1).unwrap();

    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    let outcome = rec.process().unwrap();

    run_device_commands(&mut support, &mut rec, &outcome.actions);

    // SoftScalerDriver returns the preset unchanged and reports no
    // clock, so PR1 stays NINT(1.0 * 1e7) and FREQ/TP are untouched.
    assert_eq!(rec.pr[0], 10_000_000, "non-adjusting driver leaves PR1");
    assert_eq!(rec.freq, 1e7, "non-adjusting driver leaves FREQ");
    assert!(
        (rec.tp - 1.0).abs() < 1e-12,
        "non-adjusting driver leaves TP"
    );
}

/// BUG 3 regression — C scalerRecord.c:537-538 schedules the first periodic
/// display update (`callbackRequestDelayed(pupdateCallback, 1.0/rat1)`) when
/// autocount transitions to `ss = SCALER_STATE_COUNTING` and `rat1 > .1`.
/// A freshly-started autocount must emit a `ReprocessAfter` so its displayed
/// counts refresh on the RAT1 cadence.
#[test]
fn test_autocount_start_schedules_periodic_update() {
    use epics_base_rs::server::record::ProcessAction;

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp1 = 1.0;
    rec.rat1 = 5.0; // > 0.1 — periodic update cadence 1/5 s
    rec.cont = 1; // CONT mode triggers the autocount path
    rec.dly1 = 0.0; // no hold delay — autocount starts immediately

    let outcome = rec.process().unwrap();
    assert_eq!(rec.ss, 2, "autocount must transition to COUNTING");

    let reprocess: Vec<_> = outcome
        .actions
        .iter()
        .filter_map(|a| match a {
            ProcessAction::ReprocessAfter(d) => Some(*d),
            _ => None,
        })
        .collect();
    assert!(
        !reprocess.is_empty(),
        "autocount start must schedule a periodic ReprocessAfter (C:537-538)"
    );
    assert!(
        reprocess
            .iter()
            .any(|d| (d.as_secs_f64() - 1.0 / 5.0).abs() < 1e-9),
        "periodic update must be at 1.0/rat1 = 0.2s, got {reprocess:?}"
    );
}

/// BUG 3 regression — when `rat1 <= 0.1`, C schedules no periodic update;
/// the autocount start must not emit a periodic `ReprocessAfter`.
#[test]
fn test_autocount_start_no_periodic_update_when_rat1_too_low() {
    use epics_base_rs::server::record::ProcessAction;

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp1 = 1.0;
    rec.rat1 = 0.05; // <= 0.1 — no periodic update
    rec.cont = 1;
    rec.dly1 = 0.0;

    let outcome = rec.process().unwrap();
    assert_eq!(rec.ss, 2, "autocount must transition to COUNTING");
    assert!(
        !outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "rat1 <= 0.1 must not schedule a periodic update"
    );
}

/// BUG 4 regression — C scalerRecord.c:623-624 fires the COUTP link on every
/// CNT write, from inside `special()` itself. `special()` hands the put to the
/// framework through `take_special_actions()` (R10-64), which executes it inside
/// the put; `process()` must NOT carry it.
#[test]
fn test_special_cnt_fires_coutp() {
    use epics_base_rs::server::record::ProcessAction;

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();

    // CNT write — special() must queue the COUTP put.
    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    let special = rec.take_special_actions();
    assert!(
        special.iter().any(|a| matches!(
            a,
            ProcessAction::WriteDbLink {
                link_field: "COUTP",
                ..
            }
        )),
        "special(CNT) must make the COUTP put itself (C:623-624)"
    );

    // The queue is consumed by the drain — the CNT-triggered process() does not
    // repeat it, and neither does any later cycle.
    let outcome = rec.process().unwrap();
    assert!(
        !outcome.actions.iter().any(|a| matches!(
            a,
            ProcessAction::WriteDbLink {
                link_field: "COUTP",
                ..
            }
        )),
        "C's process() makes no COUTP put on a start (:463 is guarded by \
         justFinishedUserCount)"
    );
    assert!(
        rec.take_special_actions().is_empty(),
        "COUTP must not re-fire without a new CNT write"
    );
}

/// BUG 4 regression — a CNT=0 (stop) write also fires COUTP, matching C's
/// unconditional `dbPutLink(&pscal->coutp, ...)` after the redundant guard.
#[test]
fn test_special_cnt_stop_fires_coutp() {
    use epics_base_rs::server::record::ProcessAction;

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();

    // CNT=0 while idle — not a redundant in-progress request, so special()
    // still fires COUTP.
    rec.cnt = 0;
    rec.special("CNT", true).unwrap();
    assert!(
        rec.take_special_actions().iter().any(|a| matches!(
            a,
            ProcessAction::WriteDbLink {
                link_field: "COUTP",
                ..
            }
        )),
        "special(CNT=0) must also fire the COUTP link"
    );
}

// ============================================================
// Forward link (FLNK) — C scalerRecord.c:470-481
// ============================================================

/// C `scalerRecord.c:476-481` fires `recGblFwdLink` *inside* process,
/// while `ss==IDLE && pcnt==0 && us==IDLE`, and only *then* runs the
/// auto-count block (`:484-541`), which re-arms and drives `ss` to
/// COUNTING. The FLNK therefore fires on every completed auto-count
/// cycle even though the record leaves process() counting again.
#[test]
fn test_fwd_link_fires_on_completed_autocount_cycle() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.cont = 1; // AutoCount
    rec.dly1 = 0.0; // re-arm immediately
    rec.tp1 = 1.0;
    rec.ss = 2; // COUNTING (auto-count in progress)
    rec.us = 0; // IDLE — not a user count
    rec.cnt = 0;
    rec.pcnt = 0;

    // Device support reports the auto-count cycle finished.
    rec.set_done();
    rec.process().unwrap();

    assert_eq!(rec.ss, 2, "auto-count immediately re-armed (COUNTING)");
    assert!(
        rec.should_fire_forward_link(),
        "FLNK must fire for the completed auto-count cycle even though \
         the auto-count block already re-armed ss to COUNTING"
    );
}

/// Same cycle, but DLY1 > 0: the auto-count block parks in
/// SCALER_STATE_WAITING instead of COUNTING. C's `recGblFwdLink` ran
/// before that state change, so the link still fires.
#[test]
fn test_fwd_link_fires_on_completed_autocount_cycle_with_delay() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.cont = 1;
    rec.dly1 = 5.0; // hold before re-arming
    rec.tp1 = 1.0;
    rec.ss = 2; // COUNTING
    rec.us = 0;
    rec.cnt = 0;
    rec.pcnt = 0;

    rec.set_done();
    rec.process().unwrap();

    assert_eq!(rec.ss, 1, "auto-count parked in WAITING");
    assert!(
        rec.should_fire_forward_link(),
        "FLNK must fire even though the auto-count block moved ss to WAITING"
    );
}

/// C `scalerRecord.c:470` — the `ss==IDLE` guard: a process cycle taken
/// while the hardware is still counting fires nothing.
#[test]
fn test_fwd_link_does_not_fire_while_counting() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.cont = 1;
    rec.ss = 2; // COUNTING, device support has NOT reported done
    rec.us = 0;
    rec.cnt = 0;
    rec.pcnt = 0;

    rec.process().unwrap();

    assert_eq!(rec.ss, 2);
    assert!(
        !rec.should_fire_forward_link(),
        "no FLNK while the auto-count is still running"
    );
}

/// OneShot behaviour is unchanged: the user count completes, the record
/// ends IDLE/IDLE with PCNT=0, and C fires the forward link.
#[test]
fn test_fwd_link_fires_on_oneshot_completion() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.cont = 0; // OneShot
    rec.ss = 2; // COUNTING
    rec.us = 3; // USER_COUNTING
    rec.cnt = 1;
    rec.pcnt = 1;
    rec.s[0] = 10_000_000;

    rec.set_done();
    rec.process().unwrap();

    assert_eq!(rec.ss, 0);
    assert_eq!(rec.us, 0);
    assert_eq!(rec.pcnt, 0);
    assert!(
        rec.should_fire_forward_link(),
        "OneShot completion still fires FLNK"
    );
}

/// C `scalerRecord.c:470` — `us != IDLE` (a start request waiting out
/// DLY) reaches no `recGblFwdLink`.
#[test]
fn test_fwd_link_does_not_fire_while_waiting_out_dly() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.dly = 100.0;
    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    assert_eq!(rec.us, 1); // USER_STATE_WAITING

    rec.process().unwrap();

    assert!(
        !rec.should_fire_forward_link(),
        "no FLNK on a cycle spent waiting out DLY"
    );
}

// ============================================================
// R7-66: REQSTART gate -> direction copy is bounded by NCH
// ============================================================

/// C `scalerRecord.c:413-414` (REQSTART):
///   for (i=0; i<pscal->nch; i++) { pdir[i] = pgate[i]; ... }
/// The copy stops at NCH. `D` fields for channels the hardware does not
/// have are user-writable scratch (`special()` sets D on a PR put) and C
/// never touches them at count start.
#[test]
fn test_count_start_copies_gates_to_directions_only_up_to_nch() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.init_record(1).unwrap();

    // A 4-channel card.
    rec.nch = 4;
    rec.g[0] = 1;
    rec.g[1] = 0;
    rec.g[2] = 1;
    rec.g[3] = 0;
    rec.pr[0] = 10_000_000;
    rec.pr[2] = 500;

    // Channels the card does not have: gate clear, direction set by the user.
    rec.d[4] = 1;
    rec.d[10] = 1;
    rec.d[MAX_SCALER_CHANNELS - 1] = 1;

    rec.cnt = 1;
    rec.special("CNT", true).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.ss, 2, "counting");

    // Active channels took their gate value.
    assert_eq!(&rec.d[0..4], &[1, 0, 1, 0], "D1..D4 copied from G1..G4");
    // Beyond NCH, C leaves D alone.
    assert_eq!(rec.d[4], 1, "D5 is beyond NCH and must not be cleared");
    assert_eq!(rec.d[10], 1, "D11 is beyond NCH and must not be cleared");
    assert_eq!(
        rec.d[MAX_SCALER_CHANNELS - 1],
        1,
        "D64 is beyond NCH and must not be cleared"
    );
}

// ============================================================
// R7-65: auto-count sub-millisecond branch copies gates -> directions
// ============================================================

/// C `scalerRecord.c:524-528` — when `TP1 < 1 ms`, auto-count falls back on
/// the user's per-channel presets, and the gate array is copied into the
/// direction array first:
///   for (i=0; i<pscal->nch; i++) {
///       pdir[i] = pgate[i];
///       if (pgate[i]) (*pdset->write_preset)(pscal, i, ppreset[i]);
///   }
/// The copy is unconditional; only the preset write is gated on G.
#[test]
fn test_autocount_sub_ms_copies_gates_to_directions() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.nch = 4;
    rec.tp1 = 0.0; // below 1 ms → user-preset branch
    rec.cont = 1; // auto-count mode

    rec.g[0] = 1;
    rec.g[1] = 0;
    rec.g[2] = 1;
    rec.g[3] = 0;
    rec.pr[0] = 1000;
    rec.pr[2] = 500;
    // Stale directions from an earlier configuration.
    rec.d[0] = 0;
    rec.d[1] = 1;
    rec.d[2] = 0;
    rec.d[3] = 1;
    // Beyond NCH: C's loop stops at nch, so these survive.
    rec.d[10] = 1;

    rec.process().unwrap();
    assert_eq!(rec.ss, 2, "auto-count armed");

    assert_eq!(
        &rec.d[0..4],
        &[1, 0, 1, 0],
        "D1..D4 copied from G1..G4 on the sub-ms auto-count re-arm"
    );
    assert_eq!(rec.d[10], 1, "beyond NCH, D is untouched");
}

/// The other side of C's `tp1 >= 1.e-3` boundary (`scalerRecord.c:512-523`):
/// that branch programs only channel 0 from `TP1*FREQ` "regardless of any
/// presets the user may have set" and never assigns `pdir`. The directions
/// must be left exactly as the user left them.
#[test]
fn test_autocount_timed_branch_does_not_touch_directions() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.nch = 4;
    rec.tp1 = 1.0; // >= 1 ms → timed branch
    rec.cont = 1;

    rec.g[0] = 1;
    rec.g[1] = 0;
    rec.g[2] = 1;
    rec.g[3] = 0;
    rec.d[0] = 0;
    rec.d[1] = 1;
    rec.d[2] = 0;
    rec.d[3] = 1;

    rec.process().unwrap();
    assert_eq!(rec.ss, 2, "auto-count armed");

    assert_eq!(
        &rec.d[0..4],
        &[0, 1, 0, 1],
        "the timed auto-count branch leaves D alone"
    );
}

// ── F5: the failing scaler read ────────────────────────────────────────────

#[derive(Default)]
struct FlakyScalerState {
    fail_read: bool,
    done: bool,
    done_reads: usize,
}

/// A scaler whose count read can be made to fail while a completed count is
/// still waiting in the read-and-clear `done()` latch.
struct FlakyScalerDriver(std::sync::Arc<std::sync::Mutex<FlakyScalerState>>);

impl ScalerDriver for FlakyScalerDriver {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn arm(&mut self, _start: bool) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn write_preset(
        &mut self,
        _channel: usize,
        preset: u32,
    ) -> epics_base_rs::error::CaResult<u32> {
        Ok(preset)
    }
    fn read(
        &mut self,
        counts: &mut [u32; MAX_SCALER_CHANNELS],
    ) -> epics_base_rs::error::CaResult<()> {
        if self.0.lock().unwrap().fail_read {
            return Err(epics_base_rs::error::CaError::Timeout);
        }
        counts[0] = 4242;
        Ok(())
    }
    fn done(&mut self) -> bool {
        let mut st = self.0.lock().unwrap();
        st.done_reads += 1;
        std::mem::take(&mut st.done)
    }
    fn num_channels(&self) -> usize {
        1
    }
}

/// A scaler that has finished counting but whose count read fails.
fn flaky_scaler() -> (
    std::sync::Arc<std::sync::Mutex<FlakyScalerState>>,
    scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport,
) {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FlakyScalerState {
        fail_read: true,
        done: true,
        done_reads: 0,
    }));
    let dev = scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport::new(Box::new(
        FlakyScalerDriver(std::sync::Arc::clone(&state)),
    ));
    (state, dev)
}

/// F5: `processing` raises READ_ALARM/INVALID only on an `Err` from device
/// support, so a swallowed driver read error left the record showing the
/// previous scan's counts at NO_ALARM.
#[test]
fn test_asyn_device_support_read_error_reaches_the_record() {
    use epics_base_rs::server::device_support::DeviceSupport;

    let (_state, mut dev) = flaky_scaler();
    let mut rec = ScalerRecord::default();
    rec.s[0] = 7;

    assert!(
        dev.read(&mut rec).is_err(),
        "a failed driver read must reach the record as an error"
    );
    assert_eq!(rec.s[0], 7, "a failed read must not publish counts");
}

/// F5: `done()` is read-and-clear — it reports a completed count exactly once —
/// so the failing scan must not consume it, or the completion is lost and no
/// later scan can re-deliver it.
#[test]
fn test_asyn_device_support_read_error_does_not_consume_the_done_latch() {
    use epics_base_rs::server::device_support::DeviceSupport;

    let (state, mut dev) = flaky_scaler();
    let mut rec = ScalerRecord::default();

    let _ = dev.read(&mut rec);
    assert_eq!(
        state.lock().unwrap().done_reads,
        0,
        "the read-and-clear done latch must survive the failed scan"
    );

    // Next scan, the link is back: the completion is still there to deliver.
    state.lock().unwrap().fail_read = false;
    assert!(dev.read(&mut rec).is_ok());
    assert_eq!(rec.s[0], 4242);
    let st = state.lock().unwrap();
    assert_eq!(st.done_reads, 1);
    assert!(!st.done, "the completion was delivered exactly once");
}
