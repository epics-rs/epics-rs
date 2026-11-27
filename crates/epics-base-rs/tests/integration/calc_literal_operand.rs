//! A `LITERAL_OPERAND` element name says only WHERE a literal starts, never how
//! far it runs. C matches the element, then rewinds past the name it just
//! matched (`psrc -= strlen(pel->name)`, postfix.c:258 / sCalcPostfix.c:491 /
//! aCalcPostfix.c:461) and re-scans from the first character with strtod.
//!
//! The port instead had `INF`/`NAN` as fixed three-character constant symbols
//! and hand-rolled the numeric scan, so `INFINITY` stranded `INITY` and a lone
//! `.` was the wrong error. Every expectation below is the compiled C engine
//! (base `postfix.c` + `calcPerform.c`; the aCalc rows are `aCalcPostfix.c`).

use epics_base_rs::calc::{ArrayInputs, CalcError, NumericInputs, acalc, calc};

fn base(expr: &str) -> Result<f64, CalcError> {
    calc(expr, &mut NumericInputs::new())
}

/// Compiled C: `INFINITY` -> inf, `-INFINITY` -> -inf, `INFINITY*2` -> inf.
/// strtod takes all eight characters; the port took three and choked on `INITY`.
#[test]
fn strtod_extends_inf_to_infinity() {
    assert!(base("INFINITY").unwrap().is_infinite());
    assert!(base("INFINITY").unwrap().is_sign_positive());
    assert!(base("-INFINITY").unwrap().is_sign_negative());
    assert!(base("INFINITY*2").unwrap().is_infinite());
    assert!(base("1+INFINITY").unwrap().is_infinite());
    // The three-character spelling still works, and still is a literal.
    assert!(base("INF+1").unwrap().is_infinite());
}

/// Compiled C: `infinity` -> inf, `Inf` -> inf, `nan` -> nan. `get_element` is
/// `epicsStrnCaseCmp`, and strtod is case-insensitive in its own right.
#[test]
fn the_literal_words_are_case_insensitive() {
    assert!(base("infinity").unwrap().is_infinite());
    assert!(base("Inf").unwrap().is_infinite());
    assert!(base("nan").unwrap().is_nan());
}

/// Compiled C: `NAN(123)` -> nan, and `NAN(123)+1` -> nan, so strtod consumed
/// the whole parenthesised n-char-sequence (C99 7.22.1.3) — had it consumed only
/// `NAN`, the `(` would have followed an operand and been a syntax error.
#[test]
fn strtod_takes_the_nan_char_sequence() {
    assert!(base("NAN(123)").unwrap().is_nan());
    assert!(base("NAN(123)+1").unwrap().is_nan());
}

/// The other half of the rewind: strtod stops where it stops, and what is LEFT
/// must still lex. Compiled C answers CALC_ERR_SYNTAX (11) for all of these —
/// strtod takes `INF`/`NAN` and the trailing `O`, `I`, `OSECOND`, `NY` match no
/// element. A port that treats `INF` as a token would accept them as an operand
/// followed by more operands.
#[test]
fn text_left_over_after_the_literal_must_still_lex() {
    for expr in ["INFO", "INFI", "NANOSECOND", "NANNY"] {
        assert_eq!(
            base(expr),
            Err(CalcError::Syntax),
            "{expr}: strtod stops inside it and the remainder is not an element"
        );
    }
}

/// aCalcPostfix.c:98-108 has NO `INF` and no `NAN` element, so the literal words
/// are per-table: aCalc lexes `INF` as the operands I, N, F and fails on `N` in
/// operator position. Adding the words to the shared scanner must not leak them
/// into the engine whose C table lacks them.
#[test]
fn acalc_has_no_inf_or_nan_element() {
    let mut inputs = ArrayInputs::new(8);
    assert_eq!(acalc("INF", &mut inputs), Err(CalcError::Syntax));
    assert_eq!(acalc("NAN", &mut inputs), Err(CalcError::Syntax));
}

/// The `.` element (postfix.c:77) matches with no digit behind it, and C then
/// asks strtod, which converts nothing — `pnext == psrc` is
/// CALC_ERR_BAD_LITERAL (2), NOT a syntax error. Compiled C: `.` and `A+.` both
/// answer 2. The port reported Syntax because it required a digit after the `.`
/// before it would even call the literal reader.
#[test]
fn a_lone_dot_is_a_bad_literal_not_a_syntax_error() {
    assert_eq!(base("."), Err(CalcError::BadLiteral));
    assert_eq!(base("A+."), Err(CalcError::BadLiteral));
}

/// strtod's decimal grammar allows at most ONE `.`, so `1.2.3` is the literal
/// `1.2` followed by the literal `.3` — two adjacent operands, which C rejects
/// in the parser as CALC_ERR_SYNTAX (11), not as a bad literal. The port scanned
/// digits and dots greedily and handed `1.2.3` to a float parser, reporting
/// BadLiteral.
#[test]
fn a_second_dot_starts_a_new_literal() {
    assert_eq!(base("1.2.3"), Err(CalcError::Syntax));
    assert_eq!(base(".5").unwrap(), 0.5);
    assert_eq!(base("1.5").unwrap(), 1.5);
    assert_eq!(base("3.14e2").unwrap(), 314.0);
}
