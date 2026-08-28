//! R10-14 — a SUBRANGE bound is not a number in C, it is a TYPE branch
//! (`sCalcPerform.c:1876-1888`): a double is an index, a STRING is searched for
//! with `strstr`. The port rejected a string bound outright.
//!
//! Every expectation is the output of that C block, compiled and run.

use epics_base_rs::calc::{StackValue, StringInputs, scalc};

fn sub(expr: &str) -> String {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "hello world".into();
    match scalc(expr, &mut inp).expect("st=0") {
        StackValue::Str(s) => s.as_str_lossy().into_owned(),
        StackValue::Double(d) => panic!("SUBRANGE is a string, got {d}"),
    }
}

/// A string START bound puts the range just AFTER the match; a string END bound
/// just BEFORE it. (`i = (s - ps->s) + strlen(ps1->s)`, `j = (s - ps->s) - 1`.)
#[test]
fn a_string_bound_is_a_strstr_search() {
    // Port before R10-14: TypeMismatch for every one of these.
    assert_eq!(sub("AA[\"hello\", 99]"), " world");
    assert_eq!(sub("AA[\"o\", \"d\"]"), " worl");
    assert_eq!(sub("AA[\"o w\", 99]"), "orld");
}

/// The search's fall-backs: a start that is not found selects from 0, an end
/// that is not found selects to the end, and an EMPTY end bound means the end
/// (C special-cases it — `strstr` would match at 0 and give j = -1).
#[test]
fn a_failed_search_falls_back_to_the_whole_string() {
    assert_eq!(sub("AA[\"zz\", 4]"), "hello");
    assert_eq!(sub("AA[0, \"zz\"]"), "hello world");
    assert_eq!(sub("AA[\"hello\", \"\"]"), " world");
    // An empty START needle matches at 0 and adds nothing: i = 0.
    assert_eq!(sub("AA[\"\", \"world\"]"), "hello ");
}

/// C clamps `i` from below but NOT `j`, so a match at position 0 as the END
/// bound gives j = -1 and selects nothing. This is the boundary the wrap must
/// not touch: were the search's -1 wrapped like a negative numeric bound, it
/// would become k-1 and select almost everything.
#[test]
fn an_end_bound_matching_at_zero_selects_nothing() {
    assert_eq!(sub("AA[\"h\", \"h\"]"), "");
}

/// Negative control: the numeric branch is unchanged, negative indices still
/// count back from the end, and both bounds are still inclusive.
#[test]
fn the_numeric_branch_is_untouched() {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "hello".into();
    let ev = |e: &str, inp: &mut StringInputs| match scalc(e, inp).expect("st=0") {
        StackValue::Str(s) => s.as_str_lossy().into_owned(),
        StackValue::Double(d) => panic!("expected a string, got {d}"),
    };
    assert_eq!(ev("AA[1,3]", &mut inp), "ell");
    assert_eq!(ev("AA[-3,-1]", &mut inp), "llo");
    assert_eq!(ev("AA[2,2]", &mut inp), "l");
    assert_eq!(
        ev("AA[3,1]", &mut inp),
        "",
        "an inverted range selects nothing"
    );
}

/// C `toString(ps)` on the subject (`:1873`): SUBRANGE of a double is the
/// subrange of its text, not a type error.
#[test]
fn a_double_subject_is_converted_first() {
    let mut inp = StringInputs::new();
    // "3.14159265"[0,3]
    match scalc("PI[0,3]", &mut inp).expect("st=0") {
        StackValue::Str(s) => assert_eq!(s, "3.14"),
        StackValue::Double(d) => panic!("expected a string, got {d}"),
    }
}
