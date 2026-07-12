//! sCalc COERCES a string in a numeric position; it never rejects one.
//!
//! ```c
//! #define toDouble(ps)  {if (isString(ps)) to_double(ps);}
//! #define to_double(ps) {(ps)->d = atof((ps)->s); (ps)->s = NULL;}
//! ```
//! (sCalcPerform.c:80-83), and `atof` is `strtod` — the leading numeric prefix,
//! or 0 when there is none. Every numeric operand in the C evaluator goes
//! through it: MULT, DIV, POWER, MODULO, the trig/log/abs/sqrt functions,
//! COND_IF, the relational and bit operators.
//!
//! The port's stack accessor raised TypeMismatch on any string operand, which a
//! record turns into CALC_ALARM/INVALID — so an scalcout reading a numeric
//! string from a device (the ordinary case) went into alarm instead of
//! computing. Every expectation below is the compiled sCalcPostfix/sCalcPerform.

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

/// `expr` with AA (and optionally BB) bound to strings.
fn s(expr: &str, aa: &str, bb: &str) -> Result<StackValue, CalcError> {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = aa.to_string();
    inputs.str_vars[1] = bb.to_string();
    scalc(expr, &mut inputs)
}

fn d(expr: &str, aa: &str) -> f64 {
    match s(expr, aa, "").unwrap() {
        StackValue::Double(v) => v,
        StackValue::Str(x) => panic!("{expr} must produce a double, got {x:?}"),
    }
}

/// atof, not a full parse: it takes the leading numeric prefix and answers 0
/// when there is none. Compiled C, `AA+1`: "12" -> 13, "12abc" -> 13,
/// "abc" -> 1, " 3.5" -> 4.5, "1e3" -> 1001.
#[test]
fn a_string_operand_is_atof_ed() {
    assert_eq!(d("AA+1", "12"), 13.0);
    assert_eq!(d("AA+1", "12abc"), 13.0, "atof takes the numeric PREFIX");
    assert_eq!(d("AA+1", "abc"), 1.0, "no numeric prefix at all is 0");
    assert_eq!(d("AA+1", " 3.5"), 4.5, "strtod skips leading whitespace");
    assert_eq!(d("AA+1", "1e3"), 1001.0);
    assert_eq!(d("AA+1", "-2"), -1.0);
    assert_eq!(d("AA+1", ""), 1.0, "the empty string is 0");
}

/// The whole numeric surface coerces, not just `+`. Compiled C with AA="9" /
/// "-5" / "7" / "21" / "2" as noted.
#[test]
fn every_numeric_position_coerces() {
    assert_eq!(d("AA*2", "21"), 42.0);
    assert_eq!(d("AA%3", "7"), 1.0);
    assert_eq!(d("-AA", "5"), -5.0);
    assert_eq!(d("SQRT(AA)", "9"), 3.0);
    assert_eq!(d("ABS(AA)", "-5"), 5.0);
    assert_eq!(d("AA&&1", "2"), 1.0);
    assert_eq!(d("AA?1:2", "0"), 2.0, "a string condition is atof-ed too");
    assert_eq!(d("AA?1:2", "x"), 2.0, "...and \"x\" is 0, so the else arm");
}

/// Both operands coerce, not merely one. C's MULT/DIV call `toDouble` on each
/// (sCalcPerform.c:1015-1030) with no string branch at all.
#[test]
fn both_operands_of_mult_and_div_coerce() {
    assert_eq!(
        match s("AA*BB", "3", "4").unwrap() {
            StackValue::Double(v) => v,
            other => panic!("{other:?}"),
        },
        12.0
    );
    assert_eq!(
        match s("AA/BB", "10", "4").unwrap() {
            StackValue::Double(v) => v,
            other => panic!("{other:?}"),
        },
        2.5
    );
}

/// The point of the finding: none of this may raise a calc error. A TypeMismatch
/// here is what a record turns into CALC_ALARM/INVALID.
#[test]
fn no_numeric_operand_errors_on_a_string() {
    for expr in [
        "AA+1", "AA-1", "AA*2", "AA/2", "AA%2", "AA^2", "-AA", "SQRT(AA)", "ABS(AA)", "EXP(AA)",
        "SIN(AA)", "NINT(AA)", "AA&&1", "AA||1", "!AA", "~AA", "AA&1", "AA|1", "AA?1:2", "AA<<1",
    ] {
        assert!(
            s(expr, "2", "").is_ok(),
            "{expr} is legal in C on a string operand and must not raise a calc error"
        );
    }
}

/// ADD keeps its string branch: two strings CONCATENATE, they do not add as 0+0.
/// C only coerces when one side is already a double (sCalcPerform.c:964-978).
#[test]
fn add_still_concatenates_two_strings() {
    assert_eq!(
        s("AA+BB", "ab", "cd").unwrap(),
        StackValue::Str("abcd".into())
    );
    // SUB's string branch is "remove the first occurrence", not subtraction.
    assert_eq!(
        s("AA-BB", "abcd", "bc").unwrap(),
        StackValue::Str("ad".into())
    );
}

/// A comparison with ONE string side coerces and compares numerically; a
/// comparison with TWO string sides is strcmp. The discriminator is `"10" > "9"`:
/// numerically that is true, but compiled C answers 0, because strcmp puts "10"
/// before "9". The port rejected the mixed case outright.
#[test]
fn comparisons_coerce_when_mixed_and_strcmp_when_both_strings() {
    assert_eq!(d("AA>2", "10"), 1.0, "mixed: numeric, 10 > 2");
    assert_eq!(
        d("AA>2", "abc"),
        0.0,
        "mixed: atof(\"abc\") is 0, 0 > 2 is false"
    );
    assert_eq!(d("AA==5", "5"), 1.0);
    assert_eq!(d("AA!=5", "5"), 0.0);
    assert_eq!(d("AA<2", "1"), 1.0);
    assert_eq!(d("2>AA", "1"), 1.0, "the double may be on either side");

    let both_strings = match s("AA>BB", "10", "9").unwrap() {
        StackValue::Double(v) => v,
        other => panic!("a comparison always yields a double, got {other:?}"),
    };
    assert_eq!(
        both_strings, 0.0,
        "two strings compare with strcmp: \"10\" sorts BEFORE \"9\""
    );
}
