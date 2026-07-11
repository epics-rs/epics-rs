//! R8-6 — each calc engine accepts exactly the symbols of its own C `ELEMENT`
//! table, and no others.
//!
//! C has three separate compilers, each with its own table, and the table IS
//! the lexer (`get_element`): a symbol that is not in it is never lexed and
//! `postfix()` stops with `CALC_ERR_SYNTAX` — a compile error (`CLCV != 0`),
//! not a runtime one.
//!
//! Every expectation below was taken from the C compilers themselves —
//! `sCalcPostfix.c` + `sCalcPerform.c` and `aCalcPostfix.c` + `aCalcPerform.c`
//! built standalone from `epics-modules/calc/calcApp/src` and asked, the same
//! method R7-3 used for base `postfix.c`. `COMPILE_ERR 11` (CALC_ERR_SYNTAX) is
//! `Err`, a `PERFORM` line is `Ok`.

use epics_base_rs::calc::{
    ArrayInputs, ArrayStackValue, StackValue, StringInputs, acalc, acalc_compile, compile, scalc,
    scalc_compile,
};

fn scalc_ok(expr: &str) {
    assert!(
        scalc_compile(expr).is_ok(),
        "sCalcPostfix.c compiles {expr:?}; port rejected it"
    );
}
fn scalc_err(expr: &str) {
    assert!(
        scalc_compile(expr).is_err(),
        "sCalcPostfix.c answers CALC_ERR_SYNTAX for {expr:?}; port accepted it"
    );
}
fn acalc_ok(expr: &str) {
    assert!(
        acalc_compile(expr).is_ok(),
        "aCalcPostfix.c compiles {expr:?}; port rejected it"
    );
}
fn acalc_err(expr: &str) {
    assert!(
        acalc_compile(expr).is_err(),
        "aCalcPostfix.c answers CALC_ERR_SYNTAX for {expr:?}; port accepted it"
    );
}

/// Single-letter operands: base `FETCH_A`..`FETCH_U` (postfix.c:96-141),
/// sCalc/aCalc `FETCH_A`..`FETCH_P` (sCalcPostfix.c:117-186,
/// aCalcPostfix.c:107-190).
#[test]
fn single_letter_operands_stop_at_p_in_scalc_and_acalc() {
    assert!(compile("U").is_ok(), "base has FETCH_U");
    scalc_ok("P");
    acalc_ok("P");
    for v in ["Q", "R", "S", "T", "U"] {
        scalc_err(v);
        acalc_err(v);
    }
}

/// Double-letter operands: `FETCH_AA`..`FETCH_LL` in both synApps tables, and
/// none at all in base — where `AA` is the operand `A` twice.
#[test]
fn double_letter_operands_stop_at_ll() {
    scalc_ok("LL");
    acalc_ok("LL");
    for v in ["MM", "NN", "OO", "PP", "UU"] {
        scalc_err(v);
        acalc_err(v);
    }
    assert!(compile("AA").is_err(), "base has no double-letter operand");
}

/// `FMOD` and `>>>` are in base's operator/operand tables only
/// (postfix.c:107,172).
#[test]
fn fmod_and_logical_shift_are_base_only() {
    assert!(compile("FMOD(A,B)").is_ok());
    assert!(compile("A>>>1").is_ok());
    scalc_err("FMOD(A,B)");
    acalc_err("FMOD(A,B)");
    scalc_err("A>>>1");
    acalc_err("A>>>1");
}

/// `INF`/`NAN` are LITERAL_OPERANDs of base (postfix.c:111,125) and sCalc
/// (sCalcPostfix.c:149,167). aCalc's table has neither — only `ISINF`/`ISNAN`.
#[test]
fn inf_and_nan_literals_are_not_in_the_acalc_table() {
    assert!(compile("INF").is_ok());
    assert!(compile("NAN").is_ok());
    scalc_ok("INF");
    scalc_ok("NAN");
    acalc_err("INF");
    acalc_err("NAN");
}

/// `SVAL` is sCalcPostfix.c:188 alone.
#[test]
fn sval_is_scalc_only() {
    scalc_ok("SVAL");
    acalc_err("SVAL");
    assert!(compile("SVAL").is_err());
}

/// Symbols the port had invented: no C table spells `ATOD`, `BIN_READ`,
/// `BIN_WRITE` or `NORMAL_RNDM`. The ops behind them are named `DBL`, `READ`,
/// `WRITE` and `NRNDM` (sCalcPostfix.c:133,180,199,169; aCalcPostfix.c:133).
///
/// (`READ`/`WRITE` are 2-operand in C — `runtime_effect -1`,
/// sCalcPostfix.c:180,199, so `READ(AA)` is CALC_ERR_INCOMPLETE there — while
/// this port's `BIN_READ` op is 1-operand. Renaming the symbol does not change
/// that; the arity/semantics gap is a separate defect and is reported, not
/// pinned here.)
#[test]
fn invented_symbols_are_refused() {
    for expr in ["ATOD(AA)", "BIN_READ(AA)", "BIN_WRITE(AA)", "NORMAL_RNDM"] {
        scalc_err(expr);
        acalc_err(expr);
    }
    scalc_ok("NRNDM");
    acalc_ok("NRNDM");
}

/// `DBL` is `TO_DOUBLE` in both synApps tables, but a different opcode in each:
/// string->double in sCalc, array->double in aCalc.
#[test]
fn dbl_is_the_to_double_symbol_in_both_synapps_engines() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "12.5".to_string();
    assert_eq!(
        scalc("DBL(AA)", &mut inputs).unwrap(),
        StackValue::Double(12.5)
    );

    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![7.0, 8.0, 9.0, 10.0];
    assert_eq!(
        acalc("DBL(AA)", &mut inputs).unwrap(),
        ArrayStackValue::Double(7.0),
        "aCalc DBL is to_double: the array's first element"
    );
}

/// Hex literals. base has a `{"0X", …, LITERAL_INT}` element parsed with
/// `epicsParseUInt32` (postfix.c:79,283) — 32-bit, wider is
/// CALC_ERR_BAD_LITERAL. sCalc/aCalc have no `0X` element: `0x1F` matches
/// `{"0"}` and `epicsStrtod` re-scans it (sCalcPostfix.c:491), so hex is
/// accepted there too, at full double width.
///
/// (The R8-6 finding claimed the C sCalc/aCalc tables reject hex. The compiled
/// C says otherwise: `0x1F` is 31 in all three engines.)
#[test]
fn hex_literals_follow_each_engines_literal_parser() {
    let mut n = epics_base_rs::calc::NumericInputs::new();
    assert_eq!(epics_base_rs::calc::calc("0x1F", &mut n).unwrap(), 31.0);
    assert!(
        compile("0x1FFFFFFFFF").is_err(),
        "base epicsParseUInt32 overflows past 32 bits"
    );

    let mut s = StringInputs::new();
    assert_eq!(scalc("0x1F", &mut s).unwrap(), StackValue::Double(31.0));
    assert_eq!(
        scalc("0x1FFFFFFFFF", &mut s).unwrap(),
        StackValue::Double(137438953471.0),
        "sCalc's epicsStrtod parses hex at double width"
    );

    let mut a = ArrayInputs::new(2);
    assert_eq!(
        acalc("0x1F", &mut a).unwrap(),
        ArrayStackValue::Double(31.0)
    );
}

/// The synApps-only symbols still compile in the engine that owns them.
#[test]
fn synapps_symbols_still_compile_where_c_has_them() {
    for expr in ["INT(1.6)", "UNTIL A; A:=A+1", "A>?B", "A<?B", "AA|-BB"] {
        scalc_ok(expr);
    }
    for expr in ["INT(1.6)", "A>?B", "IX", "ARR(A)", "AVG(AA)", "CAT(AA,BB)"] {
        acalc_ok(expr);
    }
}
