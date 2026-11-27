//! CBUG-AJ1 — an assignment leaves no value, so an operator whose operand is
//! one pops whatever lies below it. C's compilers only check that the depth
//! stays >= 0 after the operator (`sCalcPostfix.c:820`, `aCalcPostfix.c:750`),
//! which lets the operator take the previous statement's value, or the
//! sentinel element under the stack: 0 in sCalc's no-string evaluator
//! (`sCalcPerform.c:408-409`), 1.23456 in its string evaluator (`:845-847`)
//! and in aCalc (`aCalcPerform.c:418-420`). Compiled upstream, all values 0:
//!
//! ```text
//! sCalc  UNTIL(K:=K+1);K        -> 1001, K=1001  (sentinel 0: runs to sCalcLoopMax)
//! sCalc  UNTIL(K:=K+1);"x"      -> "x",  K=1     (sentinel 1.23456: one pass)
//! aCalc  UNTIL(K:=K+1);K        -> 1,    K=1
//! sCalc  FINITE(K:=1);UNTIL(L:=L+1);L  -> 1     (FINITE wrote 1 under the stack)
//! sCalc  ISNAN(K:=1);UNTIL(L:=L+1);L   -> 1001  (ISNAN wrote 0 there)
//! both   3;MAX(K:=1,2)          -> 3             (MAX took the 3)
//! both   4;A?1:K:=2             -> 4             (the else branch took the 4)
//! both   3;ATAN2(K:=1,2)        -> atan2(3,2)
//! both   3;A?K:=1:2             -> 3 when A is 1 (the result took the 3)
//! ```
//!
//! The port refuses these at compile time with CALC_ERR_UNDERFLOW, after every
//! check of C's, so a source C refuses keeps C's error. Only a read of the
//! missing value is refused: by an operator, or by the result when a
//! conditional's missing value is on top, and only when no path through the
//! conditionals ends with one value read. A value left buried is never read —
//! `A?1:K:=1;2` gives 2 when A is 0, and when A is 1 the uncounted 1 fails
//! C's end-of-run depth check (`sCalcPerform.c:816`), as it fails the port's.
//! A bare `(3,K:=1)` is one operand, the 3: C gives `(3,K:=1)+2` = 5 and
//! `SIN(3,K:=1)` = sin 3.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, CalcError, StackValue, StringInputs, acalc, acalc_compile,
    compile, scalc, scalc_compile,
};

#[test]
fn an_assignment_operand_is_refused() {
    for expr in [
        "UNTIL(K:=1);1",
        "UNTIL(K:=K+1);K",
        "FINITE(K:=1);1",
        "ISNAN(K:=1);1",
        "SIN(K:=1);1",
        "-(K:=1);1",
        "MAX(K:=1,5);1",
        "MAX(2,K:=1);1",
        "(K:=1)+2;1",
        "2+K:=1;5",
        "3;MAX(K:=1,2)",
        "3;UNTIL(K:=K+1)",
        "3;-(K:=1)",
        "5;MAX(K:=1,(L:=1;3))",
        "4;A?1:K:=2",
        "4;A?C:=3.3:B",
        "3;A?K:=1:2",
        "5;3;A?K:=1:L:=2",
        "5;6;MAX(A?K:=1:L:=1,3)",
        "5;A?B?1:K:=1:3",
        "3;ATAN2(K:=1,2)",
        "ATAN2(3,K:=1);1",
        "(K:=1,L:=2)+2;1",
        "5;ATAN2(A?K:=1:(1,2))",
        // The parenthesis's value is the conditional's missing value.
        "7;(5;A?K:=1:L:=1)",
        "7;(5;A?K:=1:L:=1)+2",
    ] {
        assert_eq!(
            scalc_compile(expr).err(),
            Some(CalcError::Underflow),
            "scalc {expr}"
        );
        assert_eq!(
            acalc_compile(expr).err(),
            Some(CalcError::Underflow),
            "acalc {expr}"
        );
    }
    assert_eq!(
        scalc_compile("UNTIL(K:=K+1);\"x\"").err(),
        Some(CalcError::Underflow)
    );
    assert_eq!(
        scalc_compile("AA[K:=1,2];1").err(),
        Some(CalcError::Underflow)
    );
}

/// An operand whose statements leave a value has its stores as statements, and
/// a value left buried is not read.
#[test]
fn an_operand_that_leaves_a_value_still_compiles() {
    for expr in [
        "K:=1;1",
        "1;K:=2",
        "(K:=1);K",
        "(K:=1,2)",
        "(0;K:=1)+2",
        "MAX(K:=1;K,2)",
        "MAX((K:=1;2),(L:=1;3))",
        "UNTIL(K:=K+1;K>3)",
        "UNTIL(A:=A+1;MAX(A,B)>3)",
        "A?(0;C:=3.3):B",
        "A?0;C:=3.3:B",
        "(3,K:=1)+2",
        "SIN(3,K:=1)",
        "UNTIL(3,K:=1)",
        "ATAN2((3,K:=1),2)",
        "A?1:K:=1;2",
        "3;A?K:=1:L:=2;5",
        "MAX(A?1:K:=1;2,7)",
        "3+(A?1:K:=1;2)",
        "6;5+(A?K:=1:L:=1;2)",
        "A?B?1:K:=1;2:3",
        "5;J:=1;A?L:=1:K:=2+3;6",
    ] {
        assert!(scalc_compile(expr).is_ok(), "scalc {expr}");
        assert!(acalc_compile(expr).is_ok(), "acalc {expr}");
    }
    assert!(compile("A>1.5*H && E>50;H:=A").is_ok());
}

/// A source is kept when one path through its conditionals defines the
/// result, though their branches leave different numbers of values or another
/// path reads a missing value. C gives `ATAN2(A?(1,2):3)` = atan2(1,2) when A
/// is 1, `ATAN2(A?3:(1,2))` the same when A is 0, `MAX(A?1,2:3)` = 2 when A
/// is 1, `A?K:=1;(5:6)` = 5, and `A?B:(2*K:=1;5)` = B when A is not 0.
#[test]
fn one_defined_path_is_enough() {
    for expr in [
        "ATAN2(A?(1,2):3)",
        "ATAN2(A?3:(1,2))",
        "MAX(A?1,2:3)",
        "A?K:=1;(5:6)",
        "A?K:=1;(5:6)+1",
        "A?MAX(K:=1;2,(5:6))",
        "A?B:(2*K:=1;5)",
        "0?D+L:=C;3:-D",
        "A?-C;-0:3>A?D:L:=7",
        "B?UNTIL(1):A>1?C>1?K:=-2;7:5;K:=5:-B",
    ] {
        assert!(scalc_compile(expr).is_ok(), "scalc {expr}");
        assert!(acalc_compile(expr).is_ok(), "acalc {expr}");
    }
}

/// A `;`, `)` or `,` read before a later `?`'s `:` emits the COND_END of an
/// earlier `:`, and C's `cond_search` counts that one against the later `?`:
/// the earlier true branch lands after the last COND_END, past the `;3` or
/// `,3`. Compiled C, A=1 and all else 0.
#[test]
fn a_true_branch_lands_where_cond_search_lands() {
    for (expr, v) in [
        ("A?5:B?C;D:L:=7", 5.0),
        ("1?5:B?-A;3:K:=7", 5.0),
        ("(1?5:B?-A;3):K:=7", 5.0),
        ("(1?5:B?-A);3:K:=7", 5.0),
        ("1?5:0?6:B?-A;3:K:=7", 5.0),
        ("1?5:B?-A;3:K:=7;L:=1", 5.0),
        ("9;MAX(1?5:B?-A,3:K:=7)", 9.0),
    ] {
        let mut s = StringInputs::new();
        s.num_vars[0] = 1.0;
        assert_eq!(
            scalc(expr, &mut s),
            Ok(StackValue::Double(v)),
            "scalc {expr}"
        );
        let mut a = ArrayInputs::new(8);
        a.num_vars[0] = 1.0;
        assert_eq!(
            acalc(expr, &mut a),
            Ok(ArrayStackValue::Double(v)),
            "acalc {expr}"
        );
    }
}

/// A branch or operand that left a value has its stores as statements, also
/// where it ends inside a conditional, or a conditional ends inside it. C lets
/// `ATAN2(ATAN2(1))` take the two statements before it, values that are
/// there: 9 and 2 when A is 1 in the first two, 9 and 6 when A is 0 in the
/// last. The other paths read a missing value.
#[test]
fn a_value_left_below_is_there() {
    for expr in [
        "9;A?K:=1;2:(3;(L:=1)+0);ATAN2(ATAN2(1))",
        "9;A?K:=1;B?2:(3;(L:=1)+0):(4;(L:=1)+0);ATAN2(ATAN2(1))",
        "9;A?(5:L:=1;6);ATAN2(ATAN2(1))",
    ] {
        assert!(scalc_compile(expr).is_ok(), "scalc {expr}");
        assert!(acalc_compile(expr).is_ok(), "acalc {expr}");
    }
}

/// Conditionals that each leave none or two values are followed while their
/// paths are few: eight leave no path with one value, and some path reads a
/// missing value. Past 256 paths the source is kept, as C keeps it.
#[test]
fn many_paths_are_not_followed() {
    let many = |n| format!("MAX(5,{})", vec!["A?K:=1:(1,2)"; n].join(","));
    assert_eq!(scalc_compile(&many(8)).err(), Some(CalcError::Underflow));
    assert_eq!(acalc_compile(&many(8)).err(), Some(CalcError::Underflow));
    assert!(scalc_compile(&many(12)).is_ok());
    assert!(acalc_compile(&many(12)).is_ok());
}

/// A source C already refuses keeps C's error.
#[test]
fn c_errors_come_first() {
    assert_eq!(scalc_compile("A:=B:=5").err(), Some(CalcError::Incomplete));
    assert_eq!(scalc_compile("A:=B:=5;1").err(), Some(CalcError::Underflow));
    assert_eq!(scalc_compile("A?K:=1:2").err(), Some(CalcError::Underflow));
    assert_eq!(
        scalc_compile("A?1:(K:=1)").err(),
        Some(CalcError::Incomplete)
    );
}
