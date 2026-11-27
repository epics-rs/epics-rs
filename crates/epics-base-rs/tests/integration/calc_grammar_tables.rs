//! R7-3 — each calc engine compiles against ITS C dialect's ELEMENT table.
//!
//! In C the table *is* the lexer: `get_element` looks a symbol up in the table
//! of the compiler being run, and a symbol that is not in that table is never
//! lexed. The port shares one tokenizer across the three engines, so the table
//! has to be reapplied on the token stream — and it has to be an ALLOWLIST,
//! because an exception-list accepts every symbol nobody thought to name. The
//! two that slipped through were:
//!
//! * `INT` — sCalc/aCalc only (sCalcPostfix.c:150, aCalcPostfix.c:152, where it
//!   is an alias of `NINT` and therefore ROUNDS). Base's lexer has no `INT`, so
//!   it splits `INT(A)` into the operands I, N, T and fails with
//!   CALC_ERR_SYNTAX.
//! * `LOG2` — in NO C table (postfix.c, sCalcPostfix.c, aCalcPostfix.c). All
//!   three C lexers therefore split it into `LOG` and the literal `2`. The port
//!   had it as a token mapped to a `log2()` opcode, so every engine answered
//!   1.0 where C answers log10(2).
//!
//! Every expectation below was taken from the real C compiler: postfix.c +
//! calcPerform.c built standalone out of epics-base and asked directly.
//!
//! ```text
//! LOG2          ok  ->  0.3010299956639812
//! LOG2(A)       COMPILE-ERR 11 (Syntax error, unknown operator/operand)
//! INT           COMPILE-ERR 11 (Syntax error, unknown operator/operand)
//! INT(A)        COMPILE-ERR 11 (Syntax error, unknown operator/operand)
//! NRNDM         COMPILE-ERR 11    A>?B  COMPILE-ERR 11    SVAL  COMPILE-ERR 11
//! AA            COMPILE-ERR 11    UNTIL(A) COMPILE-ERR 11
//! ```

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, CalcError, NumericInputs, StackValue, StringInputs, acalc, calc,
    scalc,
};

fn n(expr: &str) -> Result<f64, CalcError> {
    let mut inp = NumericInputs::new();
    inp.vars[0] = 1.7;
    calc(expr, &mut inp)
}

fn s(expr: &str) -> Result<f64, CalcError> {
    let mut inp = StringInputs::new();
    inp.num_vars[0] = 1.7;
    scalc(expr, &mut inp).map(|v| match v {
        StackValue::Double(d) => d,
        other => panic!("expected a double, got {other:?}"),
    })
}

fn a(expr: &str) -> Result<f64, CalcError> {
    let mut inp = ArrayInputs::new(1);
    inp.num_vars[0] = 1.7;
    acalc(expr, &mut inp).map(|v| match v {
        ArrayStackValue::Double(d) => d,
        other => panic!("expected a scalar, got {other:?}"),
    })
}

/// `LOG2` is not a symbol. It is `LOG` applied to `2`, in all three dialects,
/// because none of the three tables contains it — so C accepts the expression
/// and answers log10(2). The port used to answer log2(2) = 1.
#[test]
fn log2_is_log_of_two_not_a_base_2_logarithm() {
    let want = 2.0f64.log10(); // 0.3010299956639812, the value C printed
    assert_eq!(n("LOG2").unwrap(), want);
    assert_eq!(s("LOG2").unwrap(), want);
    assert_eq!(a("LOG2").unwrap(), want);

    // Same lexing, so the same arithmetic composes: LOG(2)+1.
    assert_eq!(n("LOG2+1").unwrap(), want + 1.0);
}

/// Once `LOG2` lexes as `LOG` `2`, a following `(` is an operand after an
/// operand — CALC_ERR_SYNTAX in C, and a compile error here.
#[test]
fn log2_of_something_is_a_syntax_error() {
    for expr in ["LOG2(A)", "LOG2(4)"] {
        assert!(matches!(n(expr), Err(CalcError::Syntax)), "numeric {expr}");
        assert!(matches!(s(expr), Err(CalcError::Syntax)), "string {expr}");
        assert!(matches!(a(expr), Err(CalcError::Syntax)), "array {expr}");
    }
}

/// `INT` is a synApps extension. The numeric engine must refuse it at COMPILE
/// time — `calc.CALC` and `calcout.CALC` are `postfix()`, which has no `INT`.
#[test]
fn int_is_not_in_the_base_table() {
    for expr in ["INT(A)", "INT(1.7)", "INT"] {
        assert!(
            matches!(n(expr), Err(CalcError::Syntax)),
            "base postfix.c has no INT: {expr}"
        );
    }
}

/// …and sCalc/aCalc must still ACCEPT it, as the alias of NINT that C makes it:
/// `INT(1.7)` is 2, not 1. (The name misleads; the C table does not.)
#[test]
fn int_is_nint_in_scalc_and_acalc() {
    assert_eq!(s("INT(A)").unwrap(), 2.0);
    assert_eq!(a("INT(A)").unwrap(), 2.0);
    assert_eq!(s("INT(-1.7)").unwrap(), -2.0);
    assert_eq!(a("INT(-1.7)").unwrap(), -2.0);
}

/// The allowlist must not have narrowed the numeric engine: everything base's
/// table DOES have still compiles. Each of these was confirmed `ok` by the C
/// driver.
#[test]
fn the_base_table_itself_still_compiles() {
    assert_eq!(n("NINT(A)").unwrap(), 2.0);
    assert_eq!(n("FMOD(A,3)").unwrap(), 1.7);
    assert_eq!(n("A>>>1").unwrap(), 0.0);
    assert_eq!(n("0xFF").unwrap(), 255.0);
    assert_eq!(n("MAX(A,3)").unwrap(), 3.0);
    assert_eq!(n("LOGE(1)").unwrap(), 0.0);
    assert!(n("Q").is_ok(), "base has FETCH_A..FETCH_U (NARGS 21)");
    assert!(n("U").is_ok());
    assert!(n("INF").unwrap().is_infinite());
    assert!(n("VAL").is_ok());
    assert!(n("PI+D2R+R2D").is_ok());
    assert!(n("A AND 1").is_ok());
    assert!(n("A XOR 1").is_ok());
    assert!(n("ISINF(A)").is_ok());
    assert!(n("FINITE(A)").is_ok());
}

/// The symbols the old exception-list named are still refused — the allowlist
/// has to hold the ground the denylist held, or this fix trades one hole for
/// another.
#[test]
fn synapps_extensions_stay_out_of_the_numeric_engine() {
    for expr in ["NRNDM", "A>?B", "A<?B", "SVAL", "AA", "UNTIL(A);", "AA:=1"] {
        assert!(
            matches!(n(expr), Err(CalcError::Syntax)),
            "not in postfix.c's table: {expr}"
        );
    }

    // String and array functions, likewise.
    for expr in ["LEN('a')", "DBL(A)", "STR(A)", "PRINTF('%d',A)"] {
        assert!(
            matches!(n(expr), Err(CalcError::Syntax)),
            "sCalc-only {expr}"
        );
    }
    for expr in ["AVG(AA)", "SUM(AA)", "IX", "ARR(A)"] {
        assert!(
            matches!(n(expr), Err(CalcError::Syntax)),
            "aCalc-only {expr}"
        );
    }
}
