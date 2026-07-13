//! R14-2 — the three string operators C's DOUBLE-ONLY evaluator has no case for.
//!
//! `sCalcPostfix` marks a program USES_STRING only for the elements in its
//! allowlist (`sCalcPostfix.c:449-471`), and `SUBLAST` (`|-`), `TO_DOUBLE`
//! (`DBL`) and `BYTE` are NOT in it. An expression whose only "string" element
//! is one of those three therefore runs C's double-only evaluator
//! (`sCalcPerform.c:399-823`) — which has no case for any of them. They fall to
//!
//! ```c
//! default:
//!     break;
//! ```
//!
//! and the stack is not touched: no pop, no push. Two consequences, both
//! checked here against compiled C:
//!
//! - `A|-B` leaves both operands behind, so C's closing `if (pd != topd)
//!   return(-1)` fails the WHOLE expression. scalcout then sets stat -1 and
//!   raises CALC_ALARM with VAL/SVAL unchanged — it does NOT compute `A-B`,
//!   which is what the port used to do.
//! - `DBL(A)` and `BYTE(A)` leave their single operand in place, which is the
//!   identity: the operand was already a double.
//!
//! The moment ANY allowlisted element joins the expression (a string literal, a
//! string variable, SVAL…), C switches to the string evaluator, where all three
//! opcodes have real cases — so the same `|-` that fails here subtracts there.

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn ev(expr: &str) -> Result<StackValue, CalcError> {
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = 7.0; // A
    inputs.num_vars[1] = 2.0; // B
    inputs.str_vars[0] = "hi".into();
    scalc(expr, &mut inputs)
}

fn num(expr: &str) -> f64 {
    match ev(expr).unwrap() {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

/// The boundary R14-2 names: `|-` with no string element anywhere fails the
/// expression, because the operands it never consumed are still on the stack.
#[test]
fn sublast_on_the_no_string_path_fails_the_whole_expression() {
    assert_eq!(ev("A|-B"), Err(CalcError::StackLeak));
    assert_eq!(ev("4|-2"), Err(CalcError::StackLeak));
    // Same when the leaked operands are the whole program's value: the error is
    // the expression's, not the operator's — nothing partial is returned.
    assert_eq!(ev("(A|-B)+1"), Err(CalcError::StackLeak));
}

/// The other side of the same boundary: one allowlisted element (here a string
/// literal, and a string variable) switches C to the string evaluator, where
/// SUBLAST is `case SUB` and a double operand makes it subtraction.
#[test]
fn sublast_with_any_string_element_runs_the_string_evaluator() {
    assert_eq!(num(r#"A|-"2""#), 5.0);
    assert_eq!(num(r#""7"|-B"#), 5.0);
    assert_eq!(num("A|-B+0*LEN(AA)"), 5.0); // LEN is allowlisted
}

/// `DBL` and `BYTE` on the no-string path are the C `default: break` too — but
/// their operand count is one, so leaving it untouched IS the identity and the
/// program still ends at depth 1. Compiled C: `DBL(A)` = 7, `BYTE(A)` = 7.
#[test]
fn dbl_and_byte_are_the_identity_on_the_no_string_path() {
    assert_eq!(num("DBL(A)"), 7.0);
    assert_eq!(num("BYTE(A)"), 7.0);
    assert_eq!(num("BYTE(65)"), 65.0);
    assert_eq!(num("DBL(A)+B"), 9.0);
}

/// And on the string path they do their real work — `BYTE("hi")` is 'h' = 104,
/// `DBL("12x")` hunts the number out of the text.
#[test]
fn dbl_and_byte_do_their_real_work_on_the_string_path() {
    assert_eq!(num("BYTE(AA)"), 104.0);
    assert_eq!(num(r#"DBL("12x")"#), 12.0);
}
