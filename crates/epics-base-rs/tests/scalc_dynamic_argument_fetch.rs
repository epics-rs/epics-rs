//! R13-5 — sCalc has the dynamic-argument fetches `@` and `@@`.
//!
//! ```c
//! {"@",   9, 10,  0,  UNARY_OPERATOR,  A_FETCH},     /* fetch numeric argument */
//! {"@@",  9, 10,  0,  UNARY_OPERATOR,  A_SFETCH},    /* fetch string argument */
//! ```
//! (`sCalcPostfix.c:99-100`.)
//!
//! ```c
//! case A_FETCH:
//!     if (isDouble(ps)) d = ps->d; else { d = atof(ps->s); ps->s = NULL; }
//!     i = myNINT(d);
//!     if (i >= numArgs || i < 0) { printf(...); ps->d = 0; }
//!     else                        ps->d = parg[i];
//!     break;
//!
//! case A_SFETCH:
//!     if (isDouble(ps)) d = ps->d; else d = atof(ps->s);
//!     ps->s = &(ps->local_string[0]);  ps->s[0] = '\0';
//!     i = myNINT(d);
//!     if (i >= numSArgs || i < 0) { printf(...); }
//!     else                        strNcpy(ps->s, psarg[i], SCALC_STRING_SIZE);
//!     break;
//! ```
//! (`sCalcPerform.c:1446-1476`.)
//!
//! The operand is the INDEX, not the value: `@0` is A and `@@0` is AA. The port's
//! SCALC_TABLE had neither symbol, so both were a syntax error — `@` and `@@`
//! existed only in the aCalc table.
//!
//! `@@` is NOT aCalc's `@@`: aCalc's is `A_AFETCH`, the ARRAY argument
//! (`aCalcPostfix.c:94`). sCalc's fetches the STRING argument, which is why C
//! lists `A_SFETCH` — and only `A_SFETCH`, not `A_FETCH` — in its `USES_STRING`
//! set (`sCalcPostfix.c:461`).
//!
//! Every expected value below is an output of the compiled upstream
//! `sCalcPostfix.c` + `sCalcPerform.c`, run with `A=7 B=8 C=9 AA="zz" BB="yy"`.

use epics_base_rs::calc::{ScalcString, StackValue, StringInputs, scalc};

fn inputs() -> StringInputs {
    let mut i = StringInputs::new();
    i.num_vars[0] = 7.0; // A
    i.num_vars[1] = 8.0; // B
    i.num_vars[2] = 9.0; // C
    i.str_vars[0] = ScalcString::from_c(b"zz"); // AA
    i.str_vars[1] = ScalcString::from_c(b"yy"); // BB
    i
}

fn num(expr: &str) -> f64 {
    match scalc(expr, &mut inputs()).unwrap() {
        StackValue::Double(v) => v,
        other => panic!("{expr}: expected a double, got {other:?}"),
    }
}

fn text(expr: &str) -> String {
    match scalc(expr, &mut inputs()).unwrap() {
        StackValue::Str(s) => s.as_str_lossy().to_string(),
        other => panic!("{expr}: expected a string, got {other:?}"),
    }
}

/// The two cases the audit pinned. Compiled C: `@0` = 7 (A), `@@0` = "zz" (AA).
#[test]
fn the_operand_indexes_the_argument_list() {
    assert_eq!(num("@0"), 7.0);
    assert_eq!(text("@@0"), "zz");
    assert_eq!(num("@1"), 8.0);
    assert_eq!(text("@@1"), "yy");
}

/// The operand is an expression, and a string operand goes through `atof` first.
/// Compiled C: `@(1+1)` = 9 (C), `@"1"` = 8 (B).
#[test]
fn the_index_is_any_expression_and_a_string_index_is_atofd() {
    assert_eq!(num("@(1+1)"), 9.0);
    assert_eq!(num(r#"@"1""#), 8.0);
}

/// The results carry their types onward: `@` is a double and `@@` is a string, so
/// `+` concatenates on one and adds on the other. Compiled C: `@0+1` = 8,
/// `@@0+"!"` = "zz!", `LEN(@@0)` = 2.
#[test]
fn the_fetches_keep_their_types() {
    assert_eq!(num("@0+1"), 8.0);
    assert_eq!(text(r#"@@0+"!""#), "zz!");
    assert_eq!(num("LEN(@@0)"), 2.0);
}

/// Out of range is not an error: C prints a message and answers 0 for `@`, and the
/// EMPTY STRING for `@@` — it points the cell at its local buffer and empties it
/// BEFORE the range test, so the result is a string either way.
///
/// (The bound is the caller's argument count. C's records pass 16/12; the port
/// passes `CALC_NARGS` = 21 everywhere, its own documented model, so these use an
/// index past 21 to test the bound itself rather than C's narrower one.)
#[test]
fn an_out_of_range_index_is_zero_and_the_empty_string() {
    assert_eq!(num("@21"), 0.0);
    assert_eq!(num("@-1"), 0.0);
    assert_eq!(text("@@21"), "");
    assert_eq!(text("@@-1"), "");
}
