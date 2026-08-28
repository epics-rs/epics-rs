//! R11-2 — every sCalc string is a `char[SCALC_STRING_SIZE]` (40) in C
//! (`sCalcPostfixPvt.h:22-25`), so no value the engine produces can exceed 39
//! bytes, and none can contain a NUL: `strncat(..., SCALC_STRING_SIZE-strlen-1)`
//! (`sCalcPerform.c:975`), `strNcpy(..., SCALC_STRING_SIZE-1)` (:1566, :1801,
//! :1810, :1861) and the LITERAL_STRING copy loop (:1496) all bound the write,
//! and `strNcpy` stops at the source's NUL as well.
//!
//! The bound is tested at its boundary — 39 in, 39 out; 40 in, 39 out — not by
//! narrative.
//!
//! R12-8: there are TWO bounds, not one. `strNcpy(dest, src, N)` copies while
//! `ii < N-1` (`sCalcPerform.c:68-74`), so `strNcpy(..., SCALC_STRING_SIZE-1)`
//! yields **38** bytes, one fewer than `strNcpy(..., SCALC_STRING_SIZE)` and the
//! `strncat`/LITERAL_STRING sites. R11-2 made the bound structural, which was
//! right, but made it uniformly 39, which lost that distinction.

use epics_base_rs::calc::{SCALC_STRING_MAX, StackValue, StringInputs, scalc};

fn eval(expr: &str, inputs: &mut StringInputs) -> StackValue {
    scalc(expr, inputs).expect("sCalcPerform returns st=0 here")
}

fn bytes_of(v: &StackValue) -> Vec<u8> {
    match v {
        StackValue::Str(s) => s.as_bytes().to_vec(),
        StackValue::Double(d) => panic!("expected a string result, got {d}"),
    }
}

fn len_of(expr: &str, inputs: &mut StringInputs) -> f64 {
    match eval(expr, inputs) {
        StackValue::Double(d) => d,
        StackValue::Str(s) => panic!("LEN must produce a double, got {s:?}"),
    }
}

#[test]
fn max_is_thirty_nine() {
    assert_eq!(SCALC_STRING_MAX, 39);
}

#[test]
fn concat_saturates_at_the_bound_not_at_the_sum() {
    // C: the two 25-byte inputs concatenate into a char[40] -> 39 bytes.
    // Port before R11-2: 50.
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "a".repeat(25).into();
    inp.str_vars[1] = "b".repeat(25).into();
    assert_eq!(len_of("LEN(AA+BB)", &mut inp), 39.0);
    assert_eq!(
        bytes_of(&eval("AA+BB", &mut inp)),
        format!("{}{}", "a".repeat(25), "b".repeat(14)).into_bytes()
    );
}

#[test]
fn concat_below_the_bound_is_untouched() {
    // Negative control: 20 + 19 = 39 is exactly the bound and loses nothing.
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "a".repeat(20).into();
    inp.str_vars[1] = "b".repeat(19).into();
    assert_eq!(len_of("LEN(AA+BB)", &mut inp), 39.0);
    // 20 + 18 = 38 likewise.
    inp.str_vars[1] = "b".repeat(18).into();
    assert_eq!(len_of("LEN(AA+BB)", &mut inp), 38.0);
}

#[test]
fn an_input_longer_than_the_field_is_truncated_on_the_way_in() {
    // C's psarg[] point at char[40] record fields, and FETCH_AA copies one into
    // the 40-byte stack element (sCalcPerform.c:872).
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "x".repeat(60).into();
    assert_eq!(len_of("LEN(AA)", &mut inp), 39.0);
}

#[test]
fn an_over_long_literal_is_truncated_at_run_time() {
    // C LITERAL_STRING: `for (i=0; (i<SCALC_STRING_SIZE-1) && *post; )`.
    let mut inp = StringInputs::new();
    let long = "z".repeat(45);
    assert_eq!(len_of(&format!("LEN(\"{long}\")"), &mut inp), 39.0);
    // Negative control: a 39-byte literal survives whole.
    let exact = "z".repeat(39);
    assert_eq!(len_of(&format!("LEN(\"{exact}\")"), &mut inp), 39.0);
    let short = "z".repeat(38);
    assert_eq!(len_of(&format!("LEN(\"{short}\")"), &mut inp), 38.0);
}

#[test]
fn a_truncated_intermediate_is_what_the_next_operator_sees() {
    // The bound is not a display artefact: SUBRANGE, LEN and the comparisons all
    // read the truncated value, exactly as C's next operator reads the same
    // char[40].
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "a".repeat(30).into();
    inp.str_vars[1] = "b".repeat(30).into();
    // Byte 38 (the last one kept) is a 'b'; there is no byte 39.
    assert_eq!(bytes_of(&eval("(AA+BB)[38,38]", &mut inp)), b"b".to_vec());
    assert_eq!(
        bytes_of(&eval("(AA+BB)[39,39]", &mut inp)),
        Vec::<u8>::new()
    );
}

#[test]
fn a_nul_terminates_a_value_as_it_does_in_c() {
    // C's strings are NUL-terminated char arrays: `strNcpy` (sCalcPerform.c:68)
    // copies `while (*ss && ii < N-1)`, so a NUL byte ENDS the value.
    let mut inp = StringInputs::new();
    inp.str_vars[0] = vec![b'a', b'b', 0u8, b'c'].into();
    assert_eq!(len_of("LEN(AA)", &mut inp), 2.0);
    assert_eq!(bytes_of(&eval("AA", &mut inp)), b"ab".to_vec());
}

/// The eight opcodes that build their result in C's `tmpstr` scratch buffer copy
/// it back with `strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1)` — a THIRTY-EIGHT
/// byte result. Compiled sCalc, with a 39-byte AA: every one of these is 38,
/// while AA itself and `AA+""` (a `strncat`) stay 39.
#[test]
fn r12_8_the_strncpy_family_is_bounded_at_thirty_eight() {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "x".repeat(SCALC_STRING_MAX).into();
    assert_eq!(len_of("LEN(AA)", &mut inp), 39.0);
    assert_eq!(len_of(r#"LEN(AA+"")"#, &mut inp), 39.0);

    assert_eq!(len_of(r#"LEN(PRINTF("%s",AA))"#, &mut inp), 38.0);
    assert_eq!(len_of("LEN(TR_ESC(AA))", &mut inp), 38.0);
    assert_eq!(len_of("LEN(ESC(AA))", &mut inp), 38.0);
    assert_eq!(len_of(r#"LEN(SSCANF(AA,"%s"))"#, &mut inp), 38.0);

    // ESC of 20 escaped newlines: the escape doubles them back to 40 bytes and
    // the copy takes 38 (compiled C agrees).
    inp.str_vars[0] = r"\n".repeat(20).into();
    assert_eq!(len_of("LEN(ESC(TR_ESC(AA)))", &mut inp), 38.0);
}
