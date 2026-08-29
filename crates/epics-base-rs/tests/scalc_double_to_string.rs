//! R11-1 — a double becomes text in sCalc by `cvtDoubleToString(d, s, 8)`
//! (`sCalcPerform.c:89-96`), never by a shortest-round-trip formatter.
//!
//! The expectations below are the output of the real `cvtFast.c`
//! (`epics-base/modules/libcom/src/cvtFast/cvtFast.c`), compiled and run against
//! the same values. Its shape changes at magnitude boundaries — fixed-point with
//! 8 fractional digits up to 1e7, `%.3f` up to 1e16, width-15 `%e` beyond — so
//! the cases here are those boundaries, not a narrative.

use epics_base_rs::calc::{StackValue, StringInputs, scalc};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

fn eval_str(expr: &str) -> String {
    let mut inp = StringInputs::new();
    match scalc(expr, &mut inp).expect("st=0") {
        StackValue::Str(s) => s.as_str_lossy().into_owned(),
        StackValue::Double(d) => panic!("expected a string result, got {d}"),
    }
}

/// `STR()` — the TO_STRING operator, C `toString(ps)`.
#[test]
fn to_string_is_fixed_point_with_eight_fractional_digits() {
    // Port before R11-1: "3.141592653589793" (and "1" for 1.0 — it printed an
    // integral double as an integer).
    assert_eq!(eval_str("STR(PI)"), "3.14159265");
    assert_eq!(eval_str("STR(1)"), "1.00000000");
    assert_eq!(eval_str("STR(0)"), "0.00000000");
    assert_eq!(eval_str("STR(-1.5)"), "-1.50000000");
    assert_eq!(eval_str("STR(1/3)"), "0.33333333");
    // Rounding is C's half-up on the decimal digit, done on the fraction as an
    // integer — not Rust's round-half-even on the binary value.
    assert_eq!(eval_str("STR(0.123456785)"), "0.12345679");
}

/// The two magnitude boundaries inside `cvtDoubleToString`.
#[test]
fn the_form_changes_at_1e7_and_at_1e16() {
    // <= 1e7: fixed point, 8 fractional digits.
    assert_eq!(eval_str("STR(10000000)"), "10000000.00000000");
    // > 1e7: sprintf("%.3f") — the precision is CLAMPED to 3, not widened.
    assert_eq!(eval_str("STR(10000000.5)"), "10000000.500");
    assert_eq!(eval_str("STR(1e8)"), "100000000.000");
    assert_eq!(eval_str("STR(1e16)"), "10000000000000000.000");
    // > 1e16: sprintf("%*.*e", 15, 8) — width 15, so a positive value gets a
    // LEADING SPACE, and the exponent carries a sign and two digits.
    assert_eq!(eval_str("STR(1e17)"), " 1.00000000e+17");
    assert_eq!(eval_str("STR(-1e17)"), "-1.00000000e+17");
    assert_eq!(eval_str("STR(1e300)"), "1.00000000e+300");
}

/// A double that is only 15 bytes as text is 39 bytes as `%.3f`; the R11-2 bound
/// still holds, and nothing here overflows it.
#[test]
fn the_conversion_stays_inside_the_forty_byte_element() {
    // 1e15 -> "1000000000000000.000" (20 bytes), the widest fixed-point form.
    assert_eq!(eval_str("STR(1e15)").len(), 20);
    assert_eq!(eval_str("STR(1e16)").len(), 21);
    assert_eq!(eval_str("STR(1e300)").len(), 15);
}

/// C `to_string` spells NaN by hand (`strcpy(s,"NaN")`) rather than letting
/// `sprintf` write `nan`.
#[test]
fn nan_is_spelled_nan() {
    // ACOS(2) is C's NaN source with no domain check of its own (`x/0` is not:
    // DIV returns -1, `sCalcPerform.c:1026`). LEN keeps the perform finite — a
    // NaN result would fail it (`:2055`) before the string could be seen.
    let mut inp = StringInputs::new();
    match scalc("LEN(STR(ACOS(2)))", &mut inp).expect("st=0") {
        StackValue::Double(d) => assert_eq!(d, 3.0, "\"NaN\" is 3 bytes"),
        StackValue::Str(s) => panic!("LEN is a double, got {s:?}"),
    }
    // The whole text, with the perform still finite: `atof("xNaN")` is 0.
    // (`STR(ACOS(2))+"!"` would NOT survive — `atof("NaN!")` reads the NaN, and
    // C fails the perform on a non-finite *presult.)
    assert_eq!(eval_str("\"x\"+STR(ACOS(2))"), "xNaN");
}

/// PRINTF's `%s` conversion applies C's `toString` to a double argument
/// (`sCalcPerform.c:1553`), so it is the same conversion.
#[test]
fn printf_percent_s_of_a_double_is_the_same_conversion() {
    // Port before R11-1: "v=3.141592653589793".
    assert_eq!(eval_str("PRINTF(\"v=%s\", PI)"), "v=3.14159265");
    assert_eq!(eval_str("PRINTF(\"v=%s\", 1)"), "v=1.00000000");
}

/// `AA:=<double>` is C's `STORE_AA` (`sCalcPerform.c:888-895`), which `toString`s
/// the value and stores it in the STRING field — the double never reaches the
/// numeric args.
#[test]
fn a_double_stored_into_a_string_var_is_converted_the_same_way() {
    let mut inp = StringInputs::new();
    scalc("AA:=PI;0", &mut inp).expect("st=0");
    assert_eq!(inp.str_vars[0], "3.14159265");
    // Port before R11-1: PI landed in num_vars[0] and AA stayed empty.
    assert_eq!(inp.num_vars[0], 0.0, "STORE_AA never writes parg[]");
    // ...and the stored text is what the next fetch of AA sees.
    let mut inp = StringInputs::new();
    assert_eq!(
        match scalc("AA:=PI;LEN(AA)", &mut inp).expect("st=0") {
            StackValue::Double(d) => d,
            StackValue::Str(s) => panic!("LEN is a double, got {s:?}"),
        },
        10.0
    );
}

/// The record's SVAL is `psresult`, filled inside `sCalcPerform` for every
/// evaluation — and for a NUMERIC program that is the epilogue at
/// `sCalcPerform.c:826-831`, which renders it at the record's **PREC**
/// (`cvtDoubleToString(*pd, psresult, precision)`), not at a fixed 8.
///
/// PREC defaults to 0, so compiled sCalc answers "3" for VAL=2.5 — this is
/// R11-C6, and the port used to answer "2.50000000" at every PREC.
#[test]
fn the_record_sval_is_rendered_at_prec() {
    let mut rec = ScalcoutRecord::new();
    rec.put_field("CALC", EpicsValue::String("A+0.5".into()))
        .unwrap();
    rec.special("CALC", true).unwrap();
    rec.put_field("A", EpicsValue::Double(2.0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.val, 2.5);
    assert_eq!(rec.sval, "3", "the shipped default PREC=0");

    for (prec, want) in [(2i16, "2.50"), (3, "2.500"), (8, "2.50000000")] {
        rec.put_field("PREC", EpicsValue::Short(prec)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.sval, want, "PREC={prec}");
    }
}

/// The STRING evaluator's epilogue (`sCalcPerform.c:2036-2043`) never sees
/// `precision`: its `to_string` is `cvtDoubleToString(d, s, 8)`, hardcoded
/// (`:89-95`). So the same VAL renders differently depending on which evaluator
/// the program selected, and THAT is C. Compiled sCalc: `A+0.5` is "3" at
/// PREC=0 while `AA+0.5` — string-marked by the `AA` fetch — is "3.14159265"...
/// or here, "2.50000000", whatever the PREC.
#[test]
fn a_string_marked_program_ignores_prec() {
    let mut rec = ScalcoutRecord::new();
    // `AA` is FETCH_AA, which is what stamps the program USES_STRING.
    rec.put_field("CALC", EpicsValue::String("AA+0.5".into()))
        .unwrap();
    rec.special("CALC", true).unwrap();
    rec.put_field("AA", EpicsValue::String("2".into())).unwrap();
    for prec in [0i16, 2, 8] {
        rec.put_field("PREC", EpicsValue::Short(prec)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 2.5);
        assert_eq!(rec.sval, "2.50000000", "PREC={prec} must not reach it");
    }
}

/// OSV is the OCAL-side mirror (`sCalcoutRecord.c:768-769`), and DOPT=Use VAL
/// copies SVAL into it (`:760`), so both routes give the same text.
#[test]
fn the_record_osv_is_the_c_conversion_of_oval() {
    let mut rec = ScalcoutRecord::new();
    rec.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    rec.special("CALC", true).unwrap();
    rec.put_field("OCAL", EpicsValue::String("A*2".into()))
        .unwrap();
    rec.special("OCAL", true).unwrap();
    rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
    rec.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every Time
    rec.put_field("A", EpicsValue::Double(0.5)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.oval, 1.0);
    // The OCAL side gets the SAME `pcalc->prec` (`sCalcoutRecord.c:770`), so it
    // is rendered at PREC too — "1" at the default 0 (R11-C6).
    assert_eq!(rec.osv, "1");
    rec.put_field("PREC", EpicsValue::Short(8)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.osv, "1.00000000");
}

/// Negative control: a STRING result is not converted at all — it reaches SVAL
/// byte for byte, and VAL is its `atof`.
#[test]
fn a_string_result_is_not_reformatted() {
    let mut rec = ScalcoutRecord::new();
    rec.put_field("CALC", EpicsValue::String("\"3.5\"+\"x\"".into()))
        .unwrap();
    rec.special("CALC", true).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.sval, "3.5x");
    assert_eq!(rec.val, 3.5, "C: *presult = atof(ps->s)");
}
