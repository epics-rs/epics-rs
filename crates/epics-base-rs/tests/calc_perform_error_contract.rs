//! R8-7 — the two synApps evaluators disagree with base, and with each other,
//! about what a division by zero (and every other C `return(-1)` site) means.
//!
//! * `sCalcPerform` FAILS the whole perform (`return(-1)`, `*presult` left
//!   untouched); `sCalcoutRecord.c:357-364` then forces VAL=-1,
//!   SVAL="***ERROR***" and raises CALC_ALARM/INVALID.
//! * `aCalcPerform` has no such failure: a zero divisor yields `myMAXFLOAT`
//!   (`(float)1e35`) and status 0.
//! * base `calcPerform` divides in bare IEEE (`+Inf`) and never fails.
//!
//! Every expectation below is a line printed by the C compilers/evaluators
//! themselves — `sCalcPostfix.c`+`sCalcPerform.c` and
//! `aCalcPostfix.c`+`aCalcPerform.c` built standalone out of
//! `epics-modules/calc/calcApp/src` and driven with these expressions.
//! `st=-1` is `Err`, `st=0 d=<v>` is `Ok(<v>)`.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, NumericInputs, StackValue, StringInputs, acalc, calc, scalc,
};

/// C `myMAXFLOAT` (`aCalcPerform.c:60`) — `((float)1e+35)` widened back to
/// double; the C driver prints it as 1.0000000409184788e+35.
const MY_MAXFLOAT: f64 = 1e35f32 as f64;

fn s(expr: &str) -> Result<StackValue, epics_base_rs::calc::CalcError> {
    let mut inp = StringInputs::new();
    inp.num_vars[0] = 6.0; // A
    inp.num_vars[1] = 3.0; // B
    scalc(expr, &mut inp)
}

fn a_scalar(expr: &str) -> f64 {
    let mut inp = ArrayInputs::new(8);
    match acalc(expr, &mut inp).expect("aCalcPerform returns st=0 here") {
        ArrayStackValue::Double(d) => d,
        ArrayStackValue::Array(v) => panic!("expected a scalar result, got {v:?}"),
    }
}

fn a_array(expr: &str, aa: &[f64], bb: &[f64]) -> Vec<f64> {
    let mut inp = ArrayInputs::new(8);
    inp.arrays[0] = aa.to_vec();
    inp.arrays[1] = bb.to_vec();
    match acalc(expr, &mut inp).expect("aCalcPerform returns st=0 here") {
        ArrayStackValue::Array(v) => v,
        ArrayStackValue::Double(d) => panic!("expected an array result, got {d}"),
    }
}

// --- sCalc: `return(-1)` is the contract, not a value ---------------------

/// C: `A/0` `1/0` `0/0` all print `st=-1 d=0` — `sCalcPerform.c:495-500`
/// (no-string path) and `:1022-1030` (string path) return BEFORE the divide,
/// so `*presult` is never written. The port used to hand back base's bare
/// IEEE `+Inf`/`NaN` with no error at all.
#[test]
fn scalc_divide_by_zero_fails_the_perform() {
    assert!(s("A/0").is_err(), "C: PERFORM st=-1 for A/0");
    assert!(s("1/0").is_err(), "C: PERFORM st=-1 for 1/0");
    assert!(s("0/0").is_err(), "C: PERFORM st=-1 for 0/0");
    // The string evaluator path (`LEN` makes `uses_string` true) has the very
    // same rule: C `LEN(AA)/0` -> st=-1.
    assert!(s("LEN(AA)/0").is_err(), "C: PERFORM st=-1 for LEN(AA)/0");
}

/// The divide is a sample of the family: C returns -1 from SQU_RT/LOG_10/LOG_E
/// too (`sCalcPerform.c:521-541`, `:1056-1080`), BEFORE calling the libm
/// function, so the port must not hand back NaN.
#[test]
fn scalc_negative_sqrt_and_log_fail_the_perform() {
    assert!(s("SQRT(-1)").is_err(), "C: PERFORM st=-1 for SQRT(-1)");
    assert!(s("LOGE(-1)").is_err(), "C: PERFORM st=-1 for LOGE(-1)");
    assert!(s("LOG(-1)").is_err(), "C: PERFORM st=-1 for LOG(-1)");
    // String path (`LEN` selects it) — same rule: C st=-1.
    assert!(
        s("LEN(AA)+SQRT(-1)").is_err(),
        "C: PERFORM st=-1 for LEN(AA)+SQRT(-1)"
    );
}

/// Both C paths end with `return(((isnan(*presult)||isinf(*presult)) ? -1 : 0))`
/// (`sCalcPerform.c:833`, `:2056`): every operator can succeed and the perform
/// still fails because the RESULT is not finite.
#[test]
fn scalc_non_finite_result_fails_the_perform() {
    // C: `LOG(0)` -> st=-1 d=-inf. The `< 0` guard does not catch 0.
    assert!(s("LOG(0)").is_err(), "C: PERFORM st=-1 for LOG(0)");
    assert!(s("LOGE(0)").is_err(), "C: PERFORM st=-1 for LOGE(0)");
    // C: `1e300*1e300` -> st=-1 d=inf.
    assert!(
        s("1e300*1e300").is_err(),
        "C: PERFORM st=-1 for 1e300*1e300"
    );
    // C: `ACOS(2)` -> st=-1 d=nan (ACOS has no domain guard; the tail catches it).
    assert!(s("ACOS(2)").is_err(), "C: PERFORM st=-1 for ACOS(2)");
}

/// C's tail tests `*presult`, which for a STRING result is `to_double(ps)`
/// (`sCalcPerform.c:2046-2050`, `to_double` = `atof`). So a string result whose
/// atof is non-finite fails the perform, while an unparseable one (atof = 0)
/// does not.
#[test]
fn scalc_string_result_is_atof_d_for_the_finite_test() {
    // C: PRINTF("%s","inf") -> st=-1 d=inf s='inf'
    assert!(
        s(r#"PRINTF("%s","inf")"#).is_err(),
        r#"C: PERFORM st=-1 for PRINTF("%s","inf")"#
    );
    // C: PRINTF("%s","hello") -> st=0 d=0 s='hello'
    assert_eq!(
        s(r#"PRINTF("%s","hello")"#).unwrap(),
        StackValue::Str("hello".into())
    );
}

/// The healthy path is untouched: C `A/B` (A=6,B=3) -> st=0 d=2.
#[test]
fn scalc_ordinary_divide_still_succeeds() {
    assert_eq!(s("A/B").unwrap(), StackValue::Double(2.0));
    assert_eq!(s("SQRT(4)").unwrap(), StackValue::Double(2.0));
    assert_eq!(s("LOG(100)").unwrap(), StackValue::Double(2.0));
}

// --- base: bare IEEE, no failure -----------------------------------------

/// base `calcPerform.c:156-159` has no zero test and no non-finite tail — the
/// numeric engine must NOT inherit sCalc's rule.
#[test]
fn base_divide_by_zero_is_still_bare_ieee() {
    let mut inp = NumericInputs::new();
    assert!(calc("1/0", &mut inp).unwrap().is_infinite());
    assert!(calc("0/0", &mut inp).unwrap().is_nan());
    assert!(calc("SQRT(-1)", &mut inp).unwrap().is_nan());
}

// --- aCalc: myMAXFLOAT, and no failure -----------------------------------

/// C `aCalcPerform.c:690-696` (scalar/scalar): a zero divisor is `myMAXFLOAT`,
/// status 0 — never NaN. The port produced NaN.
#[test]
fn acalc_scalar_divide_by_zero_is_my_maxfloat() {
    assert_eq!(a_scalar("A/0"), MY_MAXFLOAT); // C: st=0 d=1.0000000409184788e+35
    assert_eq!(a_scalar("1/0"), MY_MAXFLOAT);
    assert_eq!(a_scalar("0/0"), MY_MAXFLOAT);
    assert_eq!(a_scalar("1/A"), MY_MAXFLOAT); // A=0
}

/// C `aCalcPerform.c:659-667` (array/scalar) and `:636-643` (array/array):
/// the same `myMAXFLOAT`, element by element.
///
/// C's stack arrays are always `arraySize` long (the driver's are 8, zero
/// filled), so the inputs here are written out to full length — this case is
/// about the zero-divisor VALUE, not about how the port sizes its arrays.
#[test]
fn acalc_array_divide_by_zero_is_my_maxfloat_per_element() {
    // C: AA/0 with AA=[1,2,3,0,0,0,0,0] -> every element MAXFLOAT.
    assert_eq!(
        a_array("AA/0", &[1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0], &[]),
        vec![MY_MAXFLOAT; 8]
    );
    // C: AA/BB with AA=[1,2,3,0..], BB=[1,0,2,0..] ->
    //    [1, MAXFLOAT, 1.5, MAXFLOAT, MAXFLOAT, MAXFLOAT, MAXFLOAT, MAXFLOAT]
    assert_eq!(
        a_array(
            "AA/BB",
            &[1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            &[1.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        ),
        vec![
            1.0,
            MY_MAXFLOAT,
            1.5,
            MY_MAXFLOAT,
            MY_MAXFLOAT,
            MY_MAXFLOAT,
            MY_MAXFLOAT,
            MY_MAXFLOAT
        ]
    );
}
