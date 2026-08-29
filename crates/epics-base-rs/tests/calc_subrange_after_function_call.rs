//! R13-2 — a `[…]` or `{…}` following a function call applies the function to
//! the SUBRANGE / REPLACE result, not to the function's own argument.
//!
//! C never emits a function at the `)`. `CLOSE_PAREN` (`sCalcPostfix.c:634-663`)
//! pops only down to the `(`, removes it, and — for a `VARARG_OPERATOR` below —
//! merely copies the accumulated argument count onto it. The function itself
//! stays on the operator stack at `in_stack_pri` 9.
//!
//! `[` and `{` are `BINARY_OPERATOR`s with `in_coming_pri` **11**
//! (`sCalcPostfix.c:215-216`, `aCalcPostfix.c:212-213`) — the only value above 10
//! in any of the three tables. So the pop-on-arrival (`>= in_coming_pri`) does
//! not reach the function at 9, and the function is emitted only later, AFTER the
//! subrange. C's own postfix dump for `ABS(1)[0,1]` is
//! `1, 0, 1, SUBRANGE, ABS_VAL`.
//!
//! The port used to flush the function at the `)`, emitting
//! `1, ABS, 0, 1, SUBRANGE` — the function bound to its own argument, and the
//! subrange applied to the result. Silently different numbers, no error.
//!
//! Every expected value below is an output of the compiled upstream engines
//! (`sCalcPostfix.c`+`sCalcPerform.c`, `aCalcPostfix.c`+`aCalcPerform.c`).

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, StackValue, StringInputs, acalc, scalc, scalc_compile,
};

fn s(expr: &str) -> StackValue {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs).unwrap()
}

fn s_num(expr: &str) -> f64 {
    match s(expr) {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

fn s_str(expr: &str) -> String {
    match s(expr) {
        StackValue::Str(v) => v.as_str_lossy().to_string(),
        other => panic!("{expr}: expected a string, got {other:?}"),
    }
}

/// Compiled-C harness shape: arraySize 8, AA = the given values, zero-padded.
fn a(expr: &str, aa: &[f64]) -> ArrayStackValue {
    let mut inputs = ArrayInputs::new(8);
    let mut buf = aa.to_vec();
    buf.resize(8, 0.0);
    inputs.arrays[0] = buf;
    acalc(expr, &mut inputs).unwrap()
}

/// The four cases the audit pinned, each an output of the compiled C engine.
///
/// `LEN("abcd")[0,1]`: C takes the subrange `"ab"` first, then `LEN` of it = 2.
/// Binding `LEN` to `"abcd"` first would give 4 — what the port used to answer.
#[test]
fn len_applies_to_the_subrange_not_to_its_argument() {
    assert_eq!(s_num(r#"LEN("abcd")[0,1]"#), 2.0);
}

/// `SQRT(4){"2","9"}`: C replaces `"2"` with `"9"` in `"4"`... which does not
/// match, so the subject `"4"` survives, and `SQRT` of it is 2. Binding `SQRT`
/// first gives `"2"` -> replace -> `"9"` -> 9, the old port's answer.
#[test]
fn sqrt_applies_to_the_replace_result() {
    assert_eq!(s_num(r#"SQRT(4){"2","9"}"#), 2.0);
}

/// `INT("12.9")[0,1]`: subrange `"12"` first, then `INT` = 12. `INT` first would
/// round 12.9 to 13.
#[test]
fn int_applies_to_the_subrange() {
    assert_eq!(s_num(r#"INT("12.9")[0,1]"#), 12.0);
}

/// aCalc, AA = [3,1,2,9]: `AMAX(AA)[0,1]` is the max of the SUBRANGE [3,1] = 3.
/// Binding `AMAX` first gives the max of the whole array, 9.
#[test]
fn acalc_amax_applies_to_the_array_subrange() {
    assert_eq!(
        a("AMAX(AA)[0,1]", &[3.0, 1.0, 2.0, 9.0]),
        ArrayStackValue::Double(3.0)
    );
}

/// The emitted program itself, against C's own postfix dump. C compiles
/// `ABS(1)[0,1]` to `1, 0, 1, SUBRANGE, ABS_VAL` — the function LAST. The port
/// used to emit `1, ABS, 0, 1, SUBRANGE`.
#[test]
fn the_function_is_emitted_after_the_subrange() {
    let code = format!("{:?}", scalc_compile("ABS(1)[0,1]").unwrap().code);
    assert_eq!(
        code,
        "[Core(PushConst(1.0)), Core(PushConst(0.0)), Core(PushConst(1.0)), \
         String(Subrange), Core(Abs), Core(End)]",
        "C dumps `1, 0, 1, SUBRANGE, ABS_VAL` for ABS(1)[0,1]"
    );
}

/// A vararg function defers the same way, and the `,` inside the BRACKET must NOT
/// be charged to it — C's `SEPARATOR` (`sCalcPostfix.c:612-631`) stops at `(`,
/// `[` and `{` alike, but only `CLOSE_PAREN` (`:647-658`) hands the accumulated
/// count to a vararg below the `(`. This is why `[` / `{` are a stack entry of
/// their own rather than a second `LParen`: sharing one variant let the bracket's
/// `,` bump `MAX`'s argument count from 2 to 3.
///
/// Pinned on the emitted program, because `MAX` keeps its `nargs` in the opcode:
/// `Max(2)`, never `Max(3)`. (Note that C's runtime here feeds `MAX` the SUBRANGE
/// of its own second argument — `MAX(4, "7")` — which needs the vararg
/// string-coercion rule of W10-A1 to evaluate; the compile-stage property is what
/// R13-2 owns and is what this pins.)
#[test]
fn a_bracket_comma_is_not_charged_to_the_vararg() {
    let code = format!("{:?}", scalc_compile("MAX(4,7)[0,0]").unwrap().code);
    assert!(
        code.contains("Max(2)"),
        "MAX still takes exactly 2 arguments; the bracket's `,` is the SUBRANGE's. \
         Got: {code}"
    );
    assert!(!code.contains("Max(3)"), "got: {code}");
}

/// Every other operator has `in_coming_pri` <= 10, so it DOES pop the function on
/// arrival and the emitted order is unchanged. These pin that the deferral did
/// not disturb the ordinary case. Compiled C: `SQRT(4)+1` = 3, `SQRT(4)*3` = 6,
/// `ABS(0-2)^2` = 4.
#[test]
fn an_ordinary_operator_after_a_call_still_pops_the_function() {
    assert_eq!(s_num("SQRT(4)+1"), 3.0);
    assert_eq!(s_num("SQRT(4)*3"), 6.0);
    assert_eq!(s_num("ABS(0-2)^2"), 4.0);
    assert_eq!(s_num("SQRT(4)>2"), 0.0);
    // A `)` closing an OUTER call pops the inner function normally.
    assert_eq!(s_num("SQRT(ABS(0-16))"), 4.0);
}

/// Subrange and replace on a plain (non-call) subject are untouched by the
/// restructure. Compiled C: `"abcdef"[1,3]` = `"bcd"`, `"abcdef"{"cd","X"}` =
/// `"abXef"`.
#[test]
fn subrange_and_replace_on_a_plain_subject_are_unchanged() {
    assert_eq!(s_str(r#""abcdef"[1,3]"#), "bcd");
    assert_eq!(s_str(r#""abcdef"{"cd","X"}"#), "abXef");
}

/// The deferral is per-DELIMITER, not per-function: `{` defers exactly as `[`
/// does, and in aCalc `{` is `SUBRANGE_IP` (`aCalcPostfix.c:213`), a different
/// opcode from sCalc's `REPLACE`. Compiled C, AA=[3,1,2,9]:
/// `AMAX(AA){1,2}` = 2 (the max over the in-place window [1,2], i.e. of [1,2]),
/// not 9. The other reductions defer the same way.
#[test]
fn acalc_reductions_defer_past_both_delimiters() {
    let aa = [3.0, 1.0, 2.0, 9.0];
    assert_eq!(a("AMAX(AA){1,2}", &aa), ArrayStackValue::Double(2.0));
    assert_eq!(a("AMIN(AA)[0,2]", &aa), ArrayStackValue::Double(1.0));
    assert_eq!(a("SUM(AA)[0,1]", &aa), ArrayStackValue::Double(4.0));
    assert_eq!(a("AVG(AA)[2,3]", &aa), ArrayStackValue::Double(5.5));
}

/// Writing the subrange INSIDE the parentheses and writing it AFTER them now
/// mean the same thing, which is C's point: the subrange runs first either way.
/// Compiled C: `LEN("abcdef")` = 6, `LEN("abcdef"[0,3])` = 4, and
/// `LEN("abcdef")[0,3]` = 4 — both spellings take `"abcd"` and measure it.
/// Before the fix the second spelling answered 6, because `LEN` ran first and the
/// subrange then sliced the digit string `"6"`.
#[test]
fn the_subrange_runs_first_whichever_side_of_the_paren_it_is_on() {
    assert_eq!(s_num(r#"LEN("abcdef")"#), 6.0);
    assert_eq!(s_num(r#"LEN("abcdef"[0,3])"#), 4.0);
    assert_eq!(s_num(r#"LEN("abcdef")[0,3]"#), 4.0);
}
