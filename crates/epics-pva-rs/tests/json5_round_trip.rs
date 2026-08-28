//! The JSON5 dialect has two owners in this workspace, and this is where they
//! are made to agree.
//!
//! [`epics_pva_rs::format::format_json`] transcribes base's `yajl_gen` with
//! `yajl_gen_json5` set — bare identifier keys (`yajl_gen.c:273-285`), `NaN` and
//! `+Infinity` for a non-finite double (`yajl_gen.c:228-232`), and the JSON5
//! string escapes `\0`, `\v` and `\xNN` (`yajl_encode.c:31-95`).
//! [`epics_base_rs::json5::relaxed_to_strict`] is the reader that turns that
//! dialect back into strict JSON.
//!
//! Two transcriptions of one grammar agreeing by inspection is what let the
//! reader fall behind the writer — the reader refused `NaN`, hex, trailing
//! commas, single quotes and the `\0`/`\v`/`\xNN` escapes the writer emits. So
//! the closure is mechanical: everything the generator can write is fed through
//! the reader and then through `serde_json`, and any token only one of them
//! knows about fails here.

use epics_base_rs::json5::relaxed_to_strict;
use epics_pva_rs::format::format_json;
use epics_pva_rs::{PvField, PvStructure, ScalarValue};

/// Strip the `pv_name ` prefix and the trailing newline `format_json` adds
/// (`printJSON` prints the name and the value on one line), leaving the JSON
/// document itself.
fn body(pv_name: &str, value: &PvField) -> String {
    let line = format_json(pv_name, value, None);
    let rest = line
        .strip_prefix(pv_name)
        .expect("format_json leads with the PV name");
    rest.trim().to_string()
}

/// Read one emitted document back the way an IOC reads a `.db` link body.
fn read_back(emitted: &str) -> serde_json::Value {
    let strict = relaxed_to_strict(emitted).expect("the generator writes no comments");
    serde_json::from_str(&strict)
        .unwrap_or_else(|e| panic!("emitted {emitted}\n  strict {strict}\n  {e}"))
}

fn scalar(name: &str, v: ScalarValue) -> PvField {
    let mut s = PvStructure::new("");
    s.fields.push((name.to_string(), PvField::Scalar(v)));
    PvField::Structure(s)
}

/// Every non-finite double the generator can write. `yajl_gen_double` has no
/// other spelling for them (`yajl_gen.c:228-232`), and strict JSON has none at
/// all, so the reader lands them on `null` — lossy, and the only lossy rewrite
/// in the converter.
#[test]
fn non_finite_doubles_survive_the_round_trip_as_null() {
    for (name, x) in [
        ("nan", f64::NAN),
        ("pinf", f64::INFINITY),
        ("ninf", f64::NEG_INFINITY),
    ] {
        let emitted = body("X", &scalar(name, ScalarValue::Double(x)));
        let seen = read_back(&emitted);
        assert!(
            seen[name].is_null(),
            "{name}: emitted {emitted}, read back {seen}"
        );
    }
    // ... and the spellings really are the JSON5 ones, not `null` already.
    assert!(body("X", &scalar("v", ScalarValue::Double(f64::NAN))).contains("NaN"));
    assert!(body("X", &scalar("v", ScalarValue::Double(f64::INFINITY))).contains("+Infinity"));
    assert!(body("X", &scalar("v", ScalarValue::Double(f64::NEG_INFINITY))).contains("-Infinity"));
}

/// Finite doubles go out as `%.17g` with a `.0` suffix on an all-digit
/// rendering, and must come back as the same number.
#[test]
fn finite_doubles_survive_the_round_trip_exactly() {
    for x in [0.0, 1.0, -1.5, 1e30, -2.5e-3, f64::MIN_POSITIVE, f64::MAX] {
        let emitted = body("X", &scalar("v", ScalarValue::Double(x)));
        let seen = read_back(&emitted);
        let back = seen["v"].as_f64().unwrap_or_else(|| {
            panic!("{x}: emitted {emitted}, read back {seen}");
        });
        assert_eq!(back, x, "emitted {emitted}");
    }
}

/// The generator writes `\0`, `\v` and `\xNN` where strict JSON has `\u00nn`
/// (`yajl_encode.c:44,71,76,80-84`); the reader has to know all three.
#[test]
fn json5_string_escapes_survive_the_round_trip() {
    // Every control character, plus the strict-JSON escapes and a non-ASCII
    // character that must not be escaped at all.
    let mut payload = String::new();
    for c in 0u8..0x20 {
        payload.push(c as char);
    }
    payload.push_str("\u{7f}\"\\/ plain \u{00e9}\u{1f600}");

    let emitted = body(
        "X",
        &scalar("v", ScalarValue::String(payload.as_str().into())),
    );
    assert!(emitted.contains("\\0"), "no NUL escape in {emitted}");
    assert!(
        emitted.contains("\\v"),
        "no vertical-tab escape in {emitted}"
    );
    assert!(emitted.contains("\\x1B"), "no hex escape in {emitted}");

    let seen = read_back(&emitted);
    assert_eq!(seen["v"].as_str(), Some(payload.as_str()));
}

/// `yajl_gen_string` leaves a key bare whenever
/// `yajl_string_validate_identifier` passes, and quotes it otherwise; both
/// spellings have to read back to the same name.
#[test]
fn bare_and_quoted_keys_both_read_back() {
    let mut s = PvStructure::new("");
    for name in [
        "value",
        "_x",
        "$id",
        "a9",
        "has space",
        "has:colon",
        "9lead",
    ] {
        s.fields
            .push((name.to_string(), PvField::Scalar(ScalarValue::Int(1))));
    }
    let emitted = body("X", &PvField::Structure(s));
    assert!(emitted.contains("value:"), "key not bare in {emitted}");
    assert!(
        emitted.contains("\"has space\":"),
        "key not quoted in {emitted}"
    );

    let seen = read_back(&emitted);
    for name in [
        "value",
        "_x",
        "$id",
        "a9",
        "has space",
        "has:colon",
        "9lead",
    ] {
        assert_eq!(seen[name], 1, "{name} missing from {seen}");
    }
}

/// A whole nested document — the shape `pvget -M json` actually prints.
#[test]
fn a_nested_document_survives_the_round_trip() {
    let mut alarm = PvStructure::new("alarm_t");
    alarm
        .fields
        .push(("severity".into(), PvField::Scalar(ScalarValue::Int(0))));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String("".into())),
    ));

    let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
    root.fields.push((
        "value".into(),
        PvField::Scalar(ScalarValue::Double(f64::NAN)),
    ));
    root.fields
        .push(("alarm".into(), PvField::Structure(alarm)));
    root.fields.push((
        "arr".into(),
        PvField::ScalarArray(vec![
            ScalarValue::Double(1.0),
            ScalarValue::Double(f64::INFINITY),
        ]),
    ));

    let emitted = body("X", &PvField::Structure(root));
    let seen = read_back(&emitted);
    assert!(seen["value"].is_null());
    assert_eq!(seen["alarm"]["severity"], 0);
    assert_eq!(seen["arr"][0], 1.0);
    assert!(seen["arr"][1].is_null());
}
