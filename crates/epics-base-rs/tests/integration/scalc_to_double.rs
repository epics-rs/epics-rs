//! R11-3 — text becomes a double in sCalc by one of TWO C rules, and neither is
//! a Rust `parse::<f64>()`:
//!
//!   * `toDouble` (`sCalcPerform.c:80-83`) — `atof(s)`, i.e. `strtod`: the
//!     longest numeric PREFIX, 0 when there is none. Every numeric operand
//!     position coerces this way.
//!   * `TO_DOUBLE` (the `DBL` operator, `:1504-1513`) — a HUNT: `strpbrk` for
//!     the first digit, step back over a `.` and then a `-`, `atof` from there.
//!
//! Every expectation below is the output of both C fragments compiled and run on
//! this host. The cases are the edges where the two rules disagree, and where a
//! strict parse (what the port did) disagrees with both.

use epics_base_rs::calc::{StackValue, StringInputs, scalc, scalc_compile, scalc_perform};

fn dbl(s: &str) -> f64 {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = s.into();
    match scalc("DBL(AA)", &mut inp).expect("st=0") {
        StackValue::Double(d) => d,
        StackValue::Str(v) => panic!("DBL is a double, got {v:?}"),
    }
}

/// The plain coercion: `AA+0` puts AA in a numeric operand position.
fn coerce(s: &str) -> f64 {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = s.into();
    match scalc("AA+0", &mut inp).expect("st=0") {
        StackValue::Double(d) => d,
        StackValue::Str(v) => panic!("expected a double, got {v:?}"),
    }
}

#[test]
fn dbl_hunts_a_number_out_of_surrounding_text() {
    // Port before R11-3: 0.0 for every one of these (a strict parse).
    assert_eq!(dbl("12abc"), 12.0);
    assert_eq!(dbl("ab12cd34"), 12.0, "the FIRST digit run, not the last");
    assert_eq!(dbl("v=-12.5V"), -12.5);
    assert_eq!(dbl("T: 25.0 C"), 25.0);
    assert_eq!(dbl("a.5"), 0.5, "a '.' before the digit is taken");
    assert_eq!(dbl("x-3"), -3.0, "and a '-' before that");
}

#[test]
fn the_hunts_asymmetries_are_cs() {
    // A '+' is never taken back: strpbrk stops at the digit and only '.'/'-'
    // are stepped over.
    assert_eq!(dbl("x+3"), 3.0);
    // Only ONE '-' is stepped over.
    assert_eq!(dbl("--5"), -5.0);
    // No digit at all: strpbrk returns NULL, so the result is 0 even for text
    // atof would have read.
    assert_eq!(dbl("abc"), 0.0);
    assert_eq!(dbl("-abc"), 0.0);
    assert_eq!(
        dbl("inf"),
        0.0,
        "DBL(\"inf\") is 0 — there is no digit in it"
    );
    assert_eq!(dbl("nan"), 0.0);
    // atof from the hunt point, so trailing junk stops the conversion, C-style.
    assert_eq!(dbl("1-2"), 1.0);
    assert_eq!(dbl("3.5.7"), 3.5);
    // ...and atof's own syntax applies from there: hex and exponents included.
    assert_eq!(dbl("0x1A"), 26.0);
    assert_eq!(dbl("1.5e3"), 1500.0);
    assert_eq!(dbl("-1e-5"), -1e-5);
}

/// Negative control: the hunt is the DBL operator's alone. A numeric operand
/// position coerces with plain `atof`, which reads a PREFIX and nothing else.
#[test]
fn the_coercion_is_atof_not_the_hunt_and_not_a_strict_parse() {
    assert_eq!(coerce("12abc"), 12.0, "atof takes the numeric prefix");
    assert_eq!(coerce("  12"), 12.0, "...after leading whitespace");
    assert_eq!(coerce("+12"), 12.0);
    // Where the two rules disagree: no numeric PREFIX means 0, however much of a
    // number is further in.
    assert_eq!(coerce("v=-12.5V"), 0.0, "atof: no prefix, so 0");
    assert_eq!(dbl("v=-12.5V"), -12.5, "DBL: hunts it out");
    assert_eq!(coerce("x-3"), 0.0);
    assert_eq!(coerce("abc"), 0.0);
    assert_eq!(coerce(""), 0.0);
}

/// The coercion is also what decides whether the perform SUCCEEDS: C ends with
/// `return((isnan(*presult)||isinf(*presult)) ? -1 : 0)` (`sCalcPerform.c:2056`)
/// and `*presult` for a string result is `atof(s)` (`:2049`). So a string result
/// that `atof` reads as NaN or Inf fails the record — a strict parse (0.0) would
/// have let it through.
#[test]
fn a_string_result_whose_atof_is_not_finite_fails_the_perform() {
    // C writes the cells and THEN returns -1 (`sCalcPerform.c:2034-2056`), so the
    // failing status comes WITH the value — `ScalcResult::non_finite` is that -1.
    let perform = |sval: &str| {
        let mut inp = StringInputs::new();
        inp.str_vars[0] = sval.into();
        scalc_perform(&scalc_compile("AA").unwrap(), &mut inp, 6).unwrap()
    };
    let r = perform("NaN!");
    assert!(r.val.is_nan() && r.non_finite, "atof(\"NaN!\") is NaN");
    let r = perform("inf");
    assert!(
        r.val == f64::INFINITY && r.non_finite,
        "atof(\"inf\") is +inf"
    );
    // Negative control: text with no numeric prefix is 0.0, and 0.0 is finite.
    let r = perform("xNaN");
    assert!(r.val == 0.0 && !r.non_finite, "atof(\"xNaN\") is 0");
}

/// SSCANF's `%f` is a C conversion, so it is strtod on the input — a prefix, not
/// an all-or-nothing parse.
#[test]
fn sscanf_percent_f_reads_a_prefix() {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "1.5V".into();
    // Port before R11-3: 0.0 (parse::<f64>("1.5V") fails).
    match scalc("SSCANF(AA, \"%f\")", &mut inp).expect("st=0") {
        StackValue::Double(d) => assert_eq!(d, 1.5),
        StackValue::Str(v) => panic!("%f yields a double, got {v:?}"),
    }
}
