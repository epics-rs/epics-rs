//! W10-A2 — sCalc `|-` (SUBLAST) is plain subtraction when either operand is a
//! double.
//!
//! C gives SUBLAST no case of its own — it falls into `case SUB`:
//!
//! ```c
//! case SUB:
//! case SUBLAST:
//!     ps1 = ps;  DEC(ps);
//!     if (isDouble(ps))       { toDouble(ps1);  ps->d = ps->d - ps1->d; }
//!     else if (isDouble(ps1)) { to_double(ps);  ps->d = ps->d - ps1->d; }
//!     else {
//!         /* subtract ps1->s from ps->s */
//!         if (ps1->s[0]) {
//!             if (op == SUB) { /* first occurrence */ }
//!             else           { /* last occurrence  */ }
//!             ...
//!         }
//!     }
//! ```
//! (`sCalcPerform.c:979-1012`.)
//!
//! So `|-` and `-` are the same operator with the same mixed-type rule, and they
//! part company only inside the both-strings branch — first occurrence vs last.
//! The port popped both operands with `as_bytes()?`, which raised `TypeMismatch`
//! as soon as either side was a double.
//!
//! Every expected value below is an output of the compiled upstream
//! `sCalcPostfix.c` + `sCalcPerform.c`.

use epics_base_rs::calc::{CalcError, StackValue, StringInputs, scalc};

fn ev(expr: &str) -> Result<StackValue, CalcError> {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs)
}

fn num(expr: &str) -> f64 {
    match ev(expr).unwrap() {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

fn text(expr: &str) -> String {
    match ev(expr).unwrap() {
        StackValue::Str(s) => s.as_str_lossy().to_string(),
        other => panic!("{expr}: expected a string, got {other:?}"),
    }
}

/// The two cases the audit pinned. A double on the left coerces the right with
/// `atof` (`atof(".")` is 0), a double on the right coerces the left the same way
/// (`atof("a.b")` is 0). Both were `TypeMismatch` in the port.
#[test]
fn one_double_operand_makes_it_subtraction() {
    assert_eq!(num(r#"4|-".""#), 4.0);
    assert_eq!(num(r#""a.b"|-4"#), -4.0);
}

/// The coercion is `atof`, not a parse failure — a numeric-looking string on
/// either side subtracts as its value. Compiled C: `"12"|-2` = 10, `12|-"2"` = 10.
#[test]
fn a_numeric_string_operand_subtracts_as_its_value() {
    assert_eq!(num(r#""12"|-2"#), 10.0);
    assert_eq!(num(r#"12|-"2""#), 10.0);
}

/// Two strings still take the substring branch, and it is still the LAST
/// occurrence — that is the only thing `|-` does differently from `-`.
/// Compiled C: `"a.b.c"|-"."` = `"a.bc"`, whereas `"a.b.c"-"."` = `"ab.c"`.
#[test]
fn two_strings_still_remove_the_last_occurrence() {
    assert_eq!(text(r#""a.b.c"|-".""#), "a.bc");
    assert_eq!(text(r#""a.b.c"-".""#), "ab.c");
    assert_eq!(text(r#""aXbXc"|-"X""#), "aXbc");
}

/// C's `if (ps1->s[0])` guard: an empty pattern removes nothing, and a pattern
/// that does not occur leaves the subject alone.
#[test]
fn an_empty_or_absent_pattern_leaves_the_subject_alone() {
    assert_eq!(text(r#""abc"|-"""#), "abc");
    assert_eq!(text(r#""abc"|-"z""#), "abc");
}
