//! R13-4 — sCalc `>?` / `<?` have C's three-branch mixed-type rule, and two
//! string operands compare with `strcmp` and answer a STRING.
//!
//! ```c
//! case MAX_VAL:
//!     ps1 = ps;  DEC(ps);                       /* ps = left, ps1 = right */
//!     if (isDouble(ps))       { toDouble(ps1); if (ps->d < ps1->d) ps->d = ps1->d; }
//!     else if (isDouble(ps1)) { to_double(ps); if (ps->d < ps1->d) ps->d = ps1->d; }
//!     else {
//!         /* compare ps->s to ps1->s */
//!         if (strcmp(ps->s, ps1->s) < 0) strcpy(ps->s, ps1->s);
//!     }
//! ```
//! (`sCalcPerform.c:1296-1311`; `MIN_VAL` at `:1313-1328` reverses the
//! comparison.)
//!
//! This is the same shape ADD, SUB and the six comparisons use — either operand
//! being a double makes the operator numeric; only an all-string pair reaches
//! `strcmp`, and only that branch answers a string. The port evaluated both
//! operators through `pop2_f64`, so they were always numeric and always answered a
//! double.
//!
//! These are a DIFFERENT opcode from `MAX` / `MIN` (W10-A1): those are varargs
//! that pre-scan n arguments. Neither is covered by `cca653c6`, which fixed the
//! *aCalc* array shape of the same two operators.
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

/// The two cases the audit pinned. Both operands are strings, so C compares them
/// with `strcmp` and the result IS the winning string. The port answered the
/// double 0 for both, because `atof("abc")` and `atof("abd")` are both 0.
#[test]
fn two_strings_compare_with_strcmp_and_answer_a_string() {
    assert_eq!(text(r#""abc">?"abd""#), "abd");
    assert_eq!(text(r#""b"<?"a""#), "a");
    assert_eq!(text(r#""abd"<?"abc""#), "abc");
    assert_eq!(text(r#""b">?"a""#), "b");
}

/// One double on EITHER side makes the operator numeric, and `atof("a")` is 0.
/// Compiled C: `4>?"a"` = 4, `"a">?4` = 4, `4<?"a"` = 0, `"a"<?4` = 0.
#[test]
fn one_double_operand_makes_it_numeric() {
    assert_eq!(num(r#"4>?"a""#), 4.0);
    assert_eq!(num(r#""a">?4"#), 4.0);
    assert_eq!(num(r#"4<?"a""#), 0.0);
    assert_eq!(num(r#""a"<?4"#), 0.0);
}

/// The plain numeric case is unchanged.
#[test]
fn two_doubles_stay_numeric() {
    assert_eq!(num("5>?3"), 5.0);
    assert_eq!(num("5<?3"), 3.0);
}

/// `MAX_VAL` / `MIN_VAL` carry NO `isnan` clause — unlike the n-ary `MAX` / `MIN`
/// (W10-A1). C's `if (ps->d < ps1->d) ps->d = ps1->d;` simply never fires when the
/// left is NaN, so the NaN survives; but a NaN on the RIGHT loses and is dropped.
/// Compiled C: `NAN>?5` is a perform error (-1), while `5>?NAN` is 5.
#[test]
fn the_nan_rule_is_asymmetric_and_has_no_isnan_clause() {
    assert_eq!(ev("NAN>?5"), Err(CalcError::NonFiniteResult));
    assert_eq!(num("5>?NAN"), 5.0);
}
