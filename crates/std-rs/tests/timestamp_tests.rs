#![allow(clippy::field_reassign_with_default)]

use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use std_rs::TimestampRecord;

#[test]
fn test_record_type() {
    let rec = TimestampRecord::default();
    assert_eq!(rec.record_type(), "timestamp");
}

#[test]
fn test_default_values() {
    let rec = TimestampRecord::default();
    assert_eq!(rec.val, "");
    assert_eq!(rec.oval, "");
    assert_eq!(rec.rval, 0);
    assert_eq!(rec.tst, 0);
}

#[test]
fn test_process_produces_timestamp() {
    let mut rec = TimestampRecord::default();
    rec.process().unwrap();
    assert!(
        !rec.val.is_empty(),
        "VAL should be a non-empty timestamp string"
    );
    assert!(
        rec.rval > 0,
        "RVAL should be positive (seconds past EPICS epoch)"
    );
}

#[test]
fn test_process_updates_oval() {
    let mut rec = TimestampRecord::default();
    rec.process().unwrap();
    let first = rec.val.clone();
    // oval should be the old (empty) value
    assert_eq!(rec.oval, "");

    rec.process().unwrap();
    // oval should now be the first timestamp
    assert_eq!(rec.oval, first);
}

#[test]
fn test_all_format_options() {
    for tst in 0..=10 {
        let mut rec = TimestampRecord::default();
        rec.tst = tst;
        rec.process().unwrap();
        assert!(
            !rec.val.is_empty(),
            "TST={} should produce a non-empty timestamp, got empty",
            tst
        );
    }
}

#[test]
fn test_format_0_contains_slashes() {
    // Format 0: "YY/MM/DD HH:MM:SS"
    let mut rec = TimestampRecord::default();
    rec.tst = 0;
    rec.process().unwrap();
    assert!(
        rec.val.contains('/'),
        "Format 0 should contain '/', got: {}",
        rec.val
    );
}

#[test]
fn test_format_4_time_only() {
    // Format 4: "HH:MM:SS"
    let mut rec = TimestampRecord::default();
    rec.tst = 4;
    rec.process().unwrap();
    assert!(
        rec.val.contains(':'),
        "Format 4 should contain ':', got: {}",
        rec.val
    );
    // Should be short (8 chars: HH:MM:SS)
    assert!(
        rec.val.len() <= 10,
        "Format 4 should be short, got: {}",
        rec.val
    );
}

#[test]
fn test_format_5_hour_minute() {
    // Format 5: "HH:MM"
    let mut rec = TimestampRecord::default();
    rec.tst = 5;
    rec.process().unwrap();
    assert!(rec.val.contains(':'), "Format 5 should contain ':'");
    assert!(
        rec.val.len() <= 6,
        "Format 5 should be 5 chars, got: {}",
        rec.val
    );
}

#[test]
fn test_format_8_vms() {
    // Format 8: "DD-Mon-YYYY HH:MM:SS" (VMS)
    let mut rec = TimestampRecord::default();
    rec.tst = 8;
    rec.process().unwrap();
    assert!(
        rec.val.contains('-'),
        "VMS format should contain '-', got: {}",
        rec.val
    );
}

#[test]
fn test_format_9_with_milliseconds() {
    // Format 9: includes ".nnn" milliseconds
    let mut rec = TimestampRecord::default();
    rec.tst = 9;
    rec.process().unwrap();
    assert!(
        rec.val.contains('.'),
        "Format 9 should contain '.', got: {}",
        rec.val
    );
}

#[test]
fn test_format_10_with_milliseconds() {
    // Format 10: includes ".nnn" milliseconds
    let mut rec = TimestampRecord::default();
    rec.tst = 10;
    rec.process().unwrap();
    assert!(
        rec.val.contains('.'),
        "Format 10 should contain '.', got: {}",
        rec.val
    );
}

#[test]
fn test_get_field() {
    let mut rec = TimestampRecord::default();
    rec.tst = 3;
    rec.process().unwrap();

    match rec.get_field("VAL") {
        Some(EpicsValue::String(s)) => assert_eq!(s, rec.val),
        other => panic!("expected String, got {:?}", other),
    }
    match rec.get_field("TST") {
        Some(EpicsValue::Short(v)) => assert_eq!(v, 3),
        other => panic!("expected Short(3), got {:?}", other),
    }
    match rec.get_field("RVAL") {
        Some(EpicsValue::Long(v)) => assert!(v > 0),
        other => panic!("expected Long, got {:?}", other),
    }
}

#[test]
fn test_put_tst() {
    let mut rec = TimestampRecord::default();
    rec.put_field("TST", EpicsValue::Short(7)).unwrap();
    assert_eq!(rec.tst, 7);
}

/// C `timestampRecord.dbd` declares TST as a plain DBF_MENU with no
/// field range; `timestampRecord.c:100-138` handles any out-of-range
/// value via the `switch` `default:` branch. The port must store the
/// raw value on write — NOT clamp it — so a read-back is faithful.
#[test]
fn test_put_tst_out_of_range_not_clamped() {
    let mut rec = TimestampRecord::default();
    rec.put_field("TST", EpicsValue::Short(99)).unwrap();
    assert_eq!(rec.tst, 99, "TST stores the raw value, no clamping");
    assert_eq!(rec.get_field("TST"), Some(EpicsValue::Short(99)));
}

/// An out-of-range TST renders with format 0 (`YY/MM/DD HH:MM:SS`) —
/// C `switch` `default:` branch (`timestampRecord.c:135-137`).
#[test]
fn test_out_of_range_tst_uses_format_0() {
    let mut rec0 = TimestampRecord::default();
    rec0.tst = 0;
    rec0.process().unwrap();

    let mut rec_oob = TimestampRecord::default();
    rec_oob.tst = 99;
    rec_oob.process().unwrap();

    // Format 0 contains two '/' separators in the date part.
    assert_eq!(
        rec_oob.val.matches('/').count(),
        rec0.val.matches('/').count(),
        "out-of-range TST must render like format 0"
    );
}

/// C `timestampRecord.c:140`: `epicsTimeToStrftime(val, sizeof(val), ...)`.
/// VAL is `char val[40]`; `strftime` reserves the last byte for the NUL
/// terminator, so the field holds at most 39 visible characters. A Rust
/// `String` carries no NUL, so the visible-byte bound is exactly 39.
#[test]
fn test_val_truncated_to_39_visible_bytes() {
    let mut rec = TimestampRecord::default();
    let long = "x".repeat(100);
    rec.put_field("VAL", EpicsValue::String(long)).unwrap();
    assert_eq!(
        rec.val.len(),
        39,
        "VAL must be capped at 39 visible bytes (char[40] minus NUL)"
    );
}

#[test]
fn test_oval_is_read_only() {
    let mut rec = TimestampRecord::default();
    let result = rec.put_field("OVAL", EpicsValue::String("test".into()));
    assert!(result.is_err());
}

#[test]
fn test_unknown_field() {
    let rec = TimestampRecord::default();
    assert!(rec.get_field("NONEXISTENT").is_none());
}

#[test]
fn test_type_mismatch() {
    let mut rec = TimestampRecord::default();
    let result = rec.put_field("TST", EpicsValue::Double(1.0));
    assert!(result.is_err());
}

#[test]
fn test_field_list() {
    let rec = TimestampRecord::default();
    let fields = rec.field_list();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0].name, "VAL");
    assert_eq!(fields[1].name, "OVAL");
    assert_eq!(fields[2].name, "RVAL");
    assert_eq!(fields[3].name, "TST");
}
