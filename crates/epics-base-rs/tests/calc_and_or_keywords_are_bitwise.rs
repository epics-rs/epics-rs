//! R13-1 — the `AND` / `OR` keywords are the BITWISE operators, in all three
//! engines.
//!
//! All three C element tables give the words the same `code` column the symbols
//! `&` and `|` carry:
//!
//! | C table | line | `AND` | `OR` | `XOR` |
//! |---|---|---|---|---|
//! | `postfix.c` (base) | 174-176 | `BIT_AND` | `BIT_OR` | `BIT_EXCL_OR` |
//! | `sCalcPostfix.c` | 237-239 | `BIT_AND` | `BIT_OR` | `BIT_EXCL_OR` |
//! | `aCalcPostfix.c` | 234-236 | `BIT_AND` | `BIT_OR` | `BIT_EXCL_OR` |
//!
//! There is no `REL_AND` / `REL_OR` spelling as a word — those codes belong to
//! `&&` and `||` alone. The port compiled the keywords to the LOGICAL opcodes,
//! so every expression using the word form answered a boolean 0/1 instead of the
//! bitwise result, silently, in all three engines.
//!
//! Ground truth is the compiled upstream: base `postfix.c`+`calcPerform.c`,
//! `sCalcPostfix.c`+`sCalcPerform.c` and `aCalcPostfix.c`+`aCalcPerform.c` built
//! against a stub libCom and asked directly.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, NumericInputs, StackValue, StringInputs, acalc, calc, scalc,
};

fn base(expr: &str, a: f64) -> f64 {
    let mut inputs = NumericInputs::new();
    inputs.vars[0] = a;
    calc(expr, &mut inputs).unwrap()
}

fn string(expr: &str, a: f64) -> f64 {
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = a;
    match scalc(expr, &mut inputs).unwrap() {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

fn array(expr: &str, a: f64) -> f64 {
    let mut inputs = ArrayInputs::new(8);
    inputs.num_vars[0] = a;
    match acalc(expr, &mut inputs).unwrap() {
        ArrayStackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

/// `A` is unused in these; every engine gets the same expression and must give
/// the same answer, because all three C tables hold the same three rows.
fn all_engines(expr: &str, a: f64, expected: f64) {
    assert_eq!(base(expr, a), expected, "base postfix.c: {expr}");
    assert_eq!(string(expr, a), expected, "sCalcPostfix.c: {expr}");
    assert_eq!(array(expr, a), expected, "aCalcPostfix.c: {expr}");
}

/// Compiled C, all three engines: `5 OR 3` = 7. The port used to answer 1.
#[test]
fn or_keyword_is_bitwise_or() {
    all_engines("5 OR 3", 0.0, 7.0);
    all_engines("8 OR 4 OR 2", 0.0, 14.0);
    // The symbol form has always been right; the word must agree with it.
    all_engines("5|3", 0.0, 7.0);
}

/// Compiled C, all three engines: `12 AND 10` = 8. The port used to answer 1.
#[test]
fn and_keyword_is_bitwise_and() {
    all_engines("12 AND 10", 0.0, 8.0);
    all_engines("5&3", 0.0, 1.0);
}

/// Compiled C, all three engines: `A AND 255` with A=511 is 255 — the low byte,
/// not the boolean 1 the port answered.
#[test]
fn and_keyword_masks_a_variable() {
    all_engines("A AND 255", 511.0, 255.0);
    all_engines("A OR 256", 1.0, 257.0);
}

/// `XOR` was already bitwise, which is exactly what hid the other two: the word
/// forms are one family and all three take the `BIT_*` codes.
#[test]
fn xor_keyword_stays_bitwise() {
    all_engines("12 XOR 10", 0.0, 6.0);
}

/// `AND` / `OR` keep the PRECEDENCE of the symbols they alias — `AND` binds at 3
/// like `&`, `OR` at 2 like `|` (sCalcPostfix.c:237-238) — so `AND` still binds
/// tighter. Compiled C: `1 OR 6 AND 4` = 5.
#[test]
fn keyword_precedence_matches_the_symbols() {
    all_engines("1 OR 6 AND 4", 0.0, 5.0);
    all_engines("1|6&4", 0.0, 5.0);
}

/// The word forms are NOT `&&` / `||`: those stay logical. Compiled C:
/// `5 && 3` = 1 while `5 AND 3` = 1 by coincidence, but `12 && 10` = 1 where
/// `12 AND 10` = 8.
#[test]
fn symbol_forms_of_the_logical_operators_stay_logical() {
    all_engines("12 && 10", 0.0, 1.0);
    all_engines("5 || 3", 0.0, 1.0);
    all_engines("0 || 0", 0.0, 0.0);
}
