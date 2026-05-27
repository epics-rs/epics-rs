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
    // C `throttleRecord.c::valuePut` (line 557) only writes — and only
    // sets STS=Success / advances SENT — for a non-CONSTANT OUT link.
    rec.out = "OUTPUT:PV".to_string();
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
    rec.out = "OUTPUT:PV".to_string();
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
    rec.out = "OUTPUT:PV".to_string();
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
    rec.out = "OUTPUT:PV".to_string();
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
    rec.out = "OUTPUT:PV".to_string();
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
    rec.out = "OUTPUT:PV".to_string();
    rec.init_record(1).unwrap();

    rec.val = 5.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 10.0);
    assert_eq!(rec.drvls, 1); // Low limit
}

#[test]
fn test_limit_rejection() {
    // C `throttleRecord.c:246-296`: an out-of-range value with clipping
    // Off sets `proc_flag = 0`, restores `prec->val = prec->oval`, and
    // skips `enterValue`. C does NOT touch `prec->sts` on this path —
    // STS is written only by `valuePut`/`valueSync` after a real link
    // operation. DRVLS is still updated by the limit block (line 258).
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.drvlc = 0; // Clipping OFF → reject
    rec.dly = 0.0;
    rec.init_record(1).unwrap();

    rec.oval = 50.0; // Previous good value
    rec.val = 150.0; // Out of range
    rec.process().unwrap();
    assert_eq!(rec.val, 50.0, "VAL restored to OVAL on rejection");
    assert_eq!(
        rec.sts, 0,
        "STS must stay Unknown — C never sets STS in the limit block"
    );
    assert_eq!(rec.drvls, 2, "DRVLS reports High limit (C line 272)");
    assert_eq!(rec.sent, 0.0, "nothing sent on a rejected value");
}

#[test]
fn test_no_limits_when_equal() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 0.0;
    rec.drvll = 0.0; // Equal → limits disabled
    rec.dly = 0.0;
    rec.out = "OUTPUT:PV".to_string();
    rec.init_record(1).unwrap();

    rec.val = 999.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 999.0);
    assert_eq!(rec.drvls, 0); // Normal
}

/// C-parity: a value arriving DURING the delay window is drive-limit
/// checked by the *queuing* `process()` — C `throttleRecord.c:242-283`
/// runs the limit block on every `process()` call regardless of the
/// delay state. With clipping ON the queued value is clamped to DRVLH
/// (DRVLS → High) before it is stashed; the post-delay drain sends the
/// already-clamped value as-is (C's `valuePut` does not re-run the
/// limit block). So the value reaching OUT is the clamped 100.0 and
/// DRVLS still reads High.
#[test]
fn test_pending_value_clamped_to_drive_limit_on_drain() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.drvlc = 1; // Clipping ON
    rec.dly = 0.05; // short delay so the drain is observable in-test
    rec.out = "OUTPUT:PV".to_string();
    rec.init_record(1).unwrap();

    // First (in-range) value: sent immediately, delay window opens.
    rec.val = 50.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 50.0);
    assert_eq!(rec.wait, 1);

    // Out-of-range value arrives DURING the delay window — queued RAW.
    rec.val = 150.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 50.0, "queued value must not be sent yet");

    // Wait past the delay, then reprocess to drain the queued value.
    std::thread::sleep(std::time::Duration::from_millis(60));
    let outcome = rec.process().unwrap();

    // The drained value must be CLAMPED to DRVLH, not the raw 150.0.
    assert_eq!(rec.sent, 100.0, "drained value must be clamped to DRVLH");
    assert_eq!(rec.drvls, 2, "DRVLS must report High limit");
    let written = outcome.actions.iter().find_map(|a| match a {
        ProcessAction::WriteDbLink { value, .. } => Some(value),
        _ => None,
    });
    assert_eq!(
        written,
        Some(&EpicsValue::Double(100.0)),
        "value written to OUT must be the clamped 100.0, not raw 150.0"
    );
}

/// C-parity: an out-of-range value arriving during the delay window
/// with clipping OFF is rejected by the drive-limit block of the
/// *queuing* `process()` itself (C `throttleRecord.c:246-296` runs the
/// limit block on every process()). The rejection restores
/// `val = oval`, so the value is never queued and the later drain has
/// nothing to send. C does NOT set STS on a limit rejection — STS is
/// written only by `valuePut`/`valueSync`.
#[test]
fn test_pending_value_rejected_on_drain_when_clipping_off() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.drvlc = 0; // Clipping OFF → reject out-of-range
    rec.dly = 0.05;
    rec.out = "OUTPUT:PV".to_string();
    rec.init_record(1).unwrap();

    rec.val = 50.0;
    rec.process().unwrap();
    assert_eq!(rec.sent, 50.0);

    rec.val = 150.0; // out of range — rejected by this process()'s limit block
    rec.process().unwrap();
    assert_eq!(rec.val, 50.0, "out-of-range value restored to OVAL=50");
    assert_eq!(rec.drvls, 2, "DRVLS reports High limit");

    std::thread::sleep(std::time::Duration::from_millis(60));
    let outcome = rec.process().unwrap();

    assert_eq!(rec.sent, 50.0, "rejected value must not reach OUT");
    assert_eq!(
        rec.sts, 2,
        "STS stays Success from the first send — C never sets STS on a limit rejection"
    );
    let has_write = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::WriteDbLink { .. }));
    assert!(!has_write, "drain has nothing queued — no OUT write");
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
    // With drvlc=0 (default off), the value is rejected: VAL restored
    // to OVAL, nothing sent. C never sets STS in the limit block, so
    // STS stays Unknown; DRVLS reports the High limit.
    assert_eq!(rec.sts, 0, "STS unchanged on a limit rejection");
    assert_eq!(rec.drvls, 2, "DRVLS reports High limit");
    assert_eq!(rec.sent, 0.0, "rejected value is not sent");
}

/// C `throttleRecord.c:411-440`: writing DRVLH/DRVLL with limiting
/// active recomputes DRVLS immediately against the current VAL.
#[test]
fn test_special_drvlh_drvll_recomputes_drvls() {
    let mut rec = ThrottleRecord::default();

    // VAL above the new high limit -> DRVLS High.
    rec.val = 500.0;
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.special("DRVLH", true).unwrap();
    assert_eq!(rec.drvls, 2, "VAL above DRVLH -> DRVLS High");

    // VAL below the new low limit -> DRVLS Low.
    rec.val = -5.0;
    rec.special("DRVLL", true).unwrap();
    assert_eq!(rec.drvls, 1, "VAL below DRVLL -> DRVLS Low");

    // VAL inside the limits -> DRVLS Normal.
    rec.val = 50.0;
    rec.special("DRVLH", true).unwrap();
    assert_eq!(rec.drvls, 0, "VAL within limits -> DRVLS Normal");

    // Disabling limits (drvlh <= drvll) -> DRVLS Normal.
    rec.val = 500.0;
    rec.drvlh = 0.0;
    rec.drvll = 0.0;
    rec.special("DRVLH", true).unwrap();
    assert_eq!(rec.drvls, 0, "limits disabled -> DRVLS Normal");
}

/// C `throttleRecord.c:51,149`: VER is the module version string
/// `"0-2-1"`, copied into the VER field by `init_record` pass 0.
#[test]
fn test_ver_string() {
    let rec = ThrottleRecord::default();
    assert_eq!(rec.ver, "0-2-1");
    assert_eq!(
        rec.get_field("VER"),
        Some(EpicsValue::String("0-2-1".into()))
    );
}

/// C `throttleRecord.c:156-157`: `init_record` pass 1 resets STS to
/// Unknown and VAL to 0.
#[test]
fn test_init_record_resets_state() {
    let mut rec = ThrottleRecord::default();
    rec.val = 42.0;
    rec.sts = 2;
    rec.init_record(1).unwrap();
    assert_eq!(rec.val, 0.0, "init_record pass 1 resets VAL to 0");
    assert_eq!(rec.sts, 0, "init_record pass 1 resets STS to Unknown");
}

/// C `throttleRecord.c:242-283`: the drive-limit block tests the low
/// limit before the high limit and updates DRVLS accordingly. With
/// clipping On, an out-of-range value is clamped to the violated
/// limit.
#[test]
fn test_limit_clipping_low_bound_order() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 10.0;
    rec.drvlc = 1; // clipping On
    rec.dly = 0.0;
    rec.out = "OUTPUT:PV".to_string();
    rec.init_record(1).unwrap();

    rec.val = -50.0; // below low limit
    rec.process().unwrap();
    assert_eq!(rec.sent, 10.0, "clamped to DRVLL");
    assert_eq!(rec.drvls, 1, "DRVLS Low");
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

// ============================================================
// BUG 3 — throttle FLNK fires only on a cycle that wrote OUT.
//
// C `throttleRecord.c:308` keeps `recGblFwdLink` commented out in
// `process()`; the forward link fires ONLY inside `valuePut`'s
// non-CONSTANT branch (`throttleRecord.c:580`) — i.e. only on a
// cycle where a real OUT write actually occurred. The throttle
// overrides `should_fire_forward_link` to report that.
// ============================================================

#[test]
fn test_should_fire_forward_link_only_when_out_written() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0;
    rec.out = "OUTPUT:PV".to_string();
    rec.val = 42.0;
    rec.process().unwrap();
    assert!(
        rec.should_fire_forward_link(),
        "a cycle that wrote OUT must fire FLNK"
    );
}

#[test]
fn test_no_forward_link_on_queuing_cycle() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 10.0; // long delay
    rec.out = "OUTPUT:PV".to_string();
    rec.val = 42.0;
    rec.process().unwrap(); // first value sent — wrote OUT
    assert!(
        rec.should_fire_forward_link(),
        "first send wrote OUT — FLNK fires"
    );

    // Second value arrives DURING the delay — queued, no OUT write.
    rec.val = 99.0;
    rec.process().unwrap();
    assert!(
        !rec.should_fire_forward_link(),
        "a queuing-during-delay cycle writes no OUT — C never fires FLNK \
         (recGblFwdLink commented out in process(), valuePut not reached)"
    );
}

#[test]
fn test_no_forward_link_on_rejected_cycle() {
    let mut rec = ThrottleRecord::default();
    rec.drvlh = 100.0;
    rec.drvll = 0.0;
    rec.drvlc = 0; // clipping OFF -> reject out-of-range
    rec.dly = 0.0;
    rec.out = "OUTPUT:PV".to_string();
    rec.init_record(1).unwrap();

    rec.oval = 50.0;
    rec.val = 150.0; // out of range -> rejected, no OUT write
    rec.process().unwrap();
    assert!(
        !rec.should_fire_forward_link(),
        "a rejected out-of-range cycle writes no OUT — no FLNK"
    );
}

#[test]
fn test_no_forward_link_on_drain_with_nothing_queued() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.05;
    rec.out = "OUTPUT:PV".to_string();
    rec.val = 10.0;
    rec.process().unwrap(); // send + arm delay timer
    assert!(rec.should_fire_forward_link());

    // Drain the delay window with NOTHING queued.
    std::thread::sleep(std::time::Duration::from_millis(60));
    rec.process().unwrap();
    assert!(
        !rec.should_fire_forward_link(),
        "a drain cycle with nothing queued writes no OUT — no FLNK"
    );
}

// ============================================================
// BUG 4 — throttle STS reflects the OUT link type.
//
// C `throttleRecord.c::valuePut` (lines 557-588): a non-CONSTANT
// OUT link gets `dbPutLink` and STS from its result; a CONSTANT/
// empty OUT link is NOT written and STS is forced to error.
// ============================================================

#[test]
fn test_constant_out_link_reports_error_and_no_write() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0;
    rec.out = "5.0".to_string(); // CONSTANT OUT link
    rec.val = 42.0;
    let outcome = rec.process().unwrap();

    assert_eq!(
        rec.sts, 1,
        "a CONSTANT OUT link must report STS=Error (C valuePut else branch)"
    );
    assert_eq!(
        rec.sent, 0.0,
        "a CONSTANT OUT link is never written — SENT must not advance"
    );
    let has_write = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::WriteDbLink { .. }));
    assert!(!has_write, "a CONSTANT OUT link emits no WriteDbLink");
    assert!(
        !rec.should_fire_forward_link(),
        "a CONSTANT OUT cycle writes no OUT — no FLNK (C fires FLNK only \
         in valuePut's non-CONSTANT branch)"
    );
}

#[test]
fn test_empty_out_link_reports_error_and_no_write() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0;
    // OUT left empty (default) — C `valuePut` treats it as non-PV.
    rec.val = 42.0;
    let outcome = rec.process().unwrap();

    assert_eq!(rec.sts, 1, "an empty OUT link must report STS=Error");
    assert_eq!(rec.sent, 0.0, "an empty OUT link is never written");
    let has_write = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::WriteDbLink { .. }));
    assert!(!has_write, "an empty OUT link emits no WriteDbLink");
}

#[test]
fn test_real_out_link_reports_success_and_writes() {
    let mut rec = ThrottleRecord::default();
    rec.dly = 0.0;
    rec.out = "OUTPUT:PV".to_string(); // real DB link
    rec.val = 42.0;
    let outcome = rec.process().unwrap();

    assert_eq!(rec.sts, 2, "a real OUT link write reports STS=Success");
    assert_eq!(rec.sent, 42.0, "a real OUT link advances SENT");
    let has_write = outcome
        .actions
        .iter()
        .any(|a| matches!(a, ProcessAction::WriteDbLink { .. }));
    assert!(has_write, "a real OUT link emits a WriteDbLink");
}

// ============================================================
// BUG 5 — a CA put of DLY = +inf / NaN must not panic the record.
//
// `process()` models the delay with `Duration::from_secs_f64`,
// which panics on a non-finite argument. `put_field` rejects a
// non-finite DLY so the record task cannot panic.
// ============================================================

#[test]
fn test_dly_infinity_rejected() {
    let mut rec = ThrottleRecord::default();
    assert!(
        rec.put_field("DLY", EpicsValue::Double(f64::INFINITY))
            .is_err(),
        "a CA put of DLY = +inf must be rejected, not stored"
    );
    assert_eq!(rec.dly, 0.0, "DLY must keep its prior finite value");
}

#[test]
fn test_dly_neg_infinity_rejected() {
    let mut rec = ThrottleRecord::default();
    assert!(
        rec.put_field("DLY", EpicsValue::Double(f64::NEG_INFINITY))
            .is_err(),
        "a CA put of DLY = -inf must be rejected"
    );
    assert_eq!(rec.dly, 0.0);
}

#[test]
fn test_dly_nan_rejected() {
    let mut rec = ThrottleRecord::default();
    assert!(
        rec.put_field("DLY", EpicsValue::Double(f64::NAN)).is_err(),
        "a CA put of DLY = NaN must be rejected"
    );
    assert_eq!(rec.dly, 0.0);
}

#[test]
fn test_dly_infinity_does_not_panic_process() {
    // Even if a non-finite DLY somehow reached the record, a
    // subsequent process() must not panic. With the put_field guard,
    // DLY stays finite, so process() with the rejected-then-unchanged
    // DLY=0.0 runs cleanly.
    let mut rec = ThrottleRecord::default();
    rec.out = "OUTPUT:PV".to_string();
    let _ = rec.put_field("DLY", EpicsValue::Double(f64::INFINITY));
    rec.val = 1.0;
    // Must not panic.
    rec.process().unwrap();
}

#[test]
fn test_dly_huge_finite_rejected() {
    // A huge-but-finite f64 like 1e300 passes an `is_finite()` check
    // yet is far too large for `Duration::from_secs_f64` to represent
    // (panic: "value is either too big or NaN"). put_field must reject
    // it so it never reaches `process()`.
    let mut rec = ThrottleRecord::default();
    assert!(
        rec.put_field("DLY", EpicsValue::Double(1e300)).is_err(),
        "a CA put of DLY = 1e300 must be rejected, not stored"
    );
    assert_eq!(rec.dly, 0.0, "DLY must keep its prior finite value");
}

#[test]
fn test_dly_huge_finite_does_not_panic_process() {
    // Regression: a CA put of DLY = 1e300
    // (finite, so it slipped past the old `is_finite()` guard) was
    // stored and then panicked the record task at
    // `Duration::from_secs_f64(self.dly)` because `self.dly > 0.0` is
    // true for 1e300. After the fix the put is rejected, DLY keeps its
    // prior value (0.0), and a full process() cycle that reaches the
    // send path completes without panicking.
    let mut rec = ThrottleRecord::default();
    rec.out = "OUTPUT:PV".to_string();
    assert!(
        rec.put_field("DLY", EpicsValue::Double(1e300)).is_err(),
        "DLY = 1e300 must be rejected"
    );
    assert_eq!(rec.dly, 0.0, "rejected put must leave DLY unchanged");

    rec.val = 1.0;
    // process() reaches the `self.dly > 0.0` branch decision and the
    // immediate-send path; with DLY = 0.0 it builds no Duration and
    // must not panic.
    let outcome = rec.process().unwrap();
    assert_eq!(rec.sent, 1.0, "value must have been sent");
    assert_eq!(rec.wait, 0, "no delay armed, WAIT stays clear");
    let _ = outcome;
}

#[test]
fn test_dly_huge_finite_assigned_directly_does_not_panic_process() {
    // Belt-and-braces: if a huge DLY were assigned to `self.dly` by
    // some path other than put_field, process() must still not panic.
    // `process()` builds `Duration::from_secs_f64(self.dly)` whenever
    // `self.dly > 0.0`, so a guard that lived only in put_field would
    // not cover this. The clamp lives in the writer/`special()` gate;
    // exercise a process() cycle after a special() clamp to prove the
    // armed-delay path is safe.
    let mut rec = ThrottleRecord::default();
    rec.out = "OUTPUT:PV".to_string();
    rec.dly = 1e300;
    rec.special("DLY", true).unwrap();
    assert!(
        rec.dly.is_finite() && rec.dly <= 86_400.0,
        "special() must clamp a huge DLY to a Duration-safe ceiling, got {}",
        rec.dly
    );

    rec.val = 1.0;
    // self.dly > 0.0 now, so process() builds a Duration and arms the
    // delay timer. Must not panic.
    let outcome = rec.process().unwrap();
    assert_eq!(rec.wait, 1, "a positive DLY arms the delay, WAIT set");
    let _ = outcome;
}
