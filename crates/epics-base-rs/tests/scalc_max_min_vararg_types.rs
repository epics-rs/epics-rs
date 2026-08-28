//! W10-A1 — sCalc `MAX` / `MIN` settle their argument types by a PRE-SCAN over
//! every argument, not by the first one popped.
//!
//! ```c
//! case MAX:
//!     nargs = *post++;
//!     for(i=0, j=0; i<nargs; j |= isDouble(ps-i), i++);
//!     if (j) {
//!         /* an arg is double.  coerce all to double */
//!         toDouble(ps);
//!         while (--nargs) {
//!             d = ps->d;  DEC(ps);  toDouble(ps);
//!             if (ps->d < d || isnan(d)) ps->d = d;
//!         }
//!     } else {
//!         /* all args are string */
//!         while (--nargs) {
//!             ps1 = ps;  DEC(ps);
//!             if (strcmp(ps->s, ps1->s) < 0) strcpy(ps->s, ps1->s);
//!         }
//!     }
//! ```
//! (`sCalcPerform.c:1927-1962`; `MIN` at `:1952` is the same with the comparison
//! reversed.)
//!
//! One double anywhere makes every argument a double — `toDouble` is `atof`, so a
//! non-numeric string becomes 0. Only an all-string call takes `strcmp`, and only
//! that call can answer a string. The port branched on the type of the FIRST
//! argument it popped and raised `TypeMismatch` on a mixed call.
//!
//! Every expected value below is an output of the compiled upstream
//! `sCalcPostfix.c` + `sCalcPerform.c`.

use epics_base_rs::calc::{
    CalcError, StackValue, StringInputs, scalc, scalc_compile, scalc_perform,
};

fn ev(expr: &str) -> Result<StackValue, CalcError> {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs)
}

/// C `sCalcPerform`'s return code paired with the `*presult` it wrote: a
/// non-finite result is `st=-1` WITH the value in hand (`sCalcPerform.c:2034-2056`
/// writes the cell, then `:2055` returns -1).
fn perform(expr: &str) -> (f64, bool) {
    let mut inputs = StringInputs::new();
    let r = scalc_perform(&scalc_compile(expr).unwrap(), &mut inputs, 6).unwrap();
    (r.val, r.non_finite)
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

/// The two cases the audit pinned. One double makes both arguments doubles, and
/// `atof("a")` is 0 — so `MAX(4,"a")` is 4 and `MIN(4,"a")` is 0. The port raised
/// TypeMismatch for both.
#[test]
fn one_double_argument_coerces_them_all() {
    assert_eq!(num(r#"MAX(4,"a")"#), 4.0);
    assert_eq!(num(r#"MIN(4,"a")"#), 0.0);
}

/// The pre-scan looks at EVERY argument, so the order does not matter. Putting
/// the string first gives the same answers — which is the whole point: the port's
/// "type of the first operand popped" rule could not.
#[test]
fn the_prescan_does_not_depend_on_argument_order() {
    assert_eq!(num(r#"MAX("a",4)"#), 4.0);
    assert_eq!(num(r#"MIN("a",4)"#), 0.0);
}

/// An ALL-string call takes `strcmp` and answers the winning STRING.
/// Compiled C: `MAX("abc","abd")` = `"abd"`, `MIN("abc","abd")` = `"abc"`,
/// `MAX("a","b","c")` = `"c"`.
#[test]
fn an_all_string_call_compares_with_strcmp_and_answers_a_string() {
    assert_eq!(text(r#"MAX("abc","abd")"#), "abd");
    assert_eq!(text(r#"MIN("abc","abd")"#), "abc");
    assert_eq!(text(r#"MAX("a","b","c")"#), "c");
    assert_eq!(text(r#"MIN("a","b","c")"#), "a");
}

/// The two paths genuinely disagree, and the pre-scan is what picks between them.
/// Compiled C: `MAX("10","9")` = `"9"` — strcmp, because `'9' > '1'`. Add ONE
/// double and the same comparison becomes numeric: `MAX("10",9)` = 10.
#[test]
fn strcmp_and_numeric_give_different_answers_and_the_prescan_chooses() {
    assert_eq!(text(r#"MAX("10","9")"#), "9");
    assert_eq!(num(r#"MAX("10",9)"#), 10.0);
    assert_eq!(num("MAX(10,9)"), 10.0);
}

/// `isnan(d)` in C tests the RUNNING value, so a NaN entering the fold stays and
/// the whole perform fails on a non-finite result. Compiled C returns -1 (the
/// record raises a CALC alarm) for all four; the port used to DROP the NaN and
/// answer 5.
#[test]
fn a_nan_argument_propagates_and_fails_the_perform() {
    // C writes the NaN into `*presult` and THEN returns -1 (st=-1 d=nan), so
    // both halves are checked: the value survives the failing status.
    for expr in ["MAX(NAN,5)", "MAX(5,NAN)", "MIN(NAN,5)", "MIN(5,NAN)"] {
        let (val, non_finite) = perform(expr);
        assert!(val.is_nan(), "{expr}: C writes d=nan into *presult");
        assert!(non_finite, "{expr}: C PERFORM st=-1");
    }
}
