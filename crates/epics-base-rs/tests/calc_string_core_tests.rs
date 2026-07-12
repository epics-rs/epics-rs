#![allow(clippy::approx_constant)]

use epics_base_rs::calc::{StackValue, StringInputs, scalc, scalc_compile, scalc_eval};

fn eval_str(expr: &str) -> StackValue {
    let mut inputs = StringInputs::new();
    scalc(expr, &mut inputs).unwrap()
}

fn eval_str_with(expr: &str, inputs: &mut StringInputs) -> StackValue {
    scalc(expr, inputs).unwrap()
}

// --- String literal tests ---

#[test]
fn test_string_literal_double_quote() {
    assert_eq!(eval_str(r#""hello""#), StackValue::Str("hello".into()));
}

#[test]
fn test_string_literal_single_quote() {
    assert_eq!(eval_str("'world'"), StackValue::Str("world".into()));
}

// R13-3 — a string literal is RAW BYTES. C's `LITERAL_STRING`
// (`sCalcPostfix.c:803-812`) is `while (*psrc != c && *psrc) *pout++ = *psrc++;`
// — a byte-for-byte copy that interprets no backslash at all. `$T` / `TR_ESC` is
// the only translator, which is why sCalc has that operator.
//
// These three used to assert the opposite (that the lexer translates), which is
// the defect: it made `$T` a double translation and changed the bytes on every
// path that does not translate.

#[test]
fn test_string_literal_keeps_backslash_n_raw() {
    // Compiled C: `LEN("a\nb")` = 4 — the bytes are `a`, `\`, `n`, `b`.
    assert_eq!(eval_str(r#""a\nb""#), StackValue::Str("a\\nb".into()));
}

#[test]
fn test_string_literal_keeps_backslash_t_raw() {
    // Compiled C: `BYTE("\t")` = 92, the backslash — not 9, a TAB.
    assert_eq!(eval_str(r#""a\tb""#), StackValue::Str("a\\tb".into()));
}

#[test]
fn test_string_literal_keeps_both_backslashes_raw() {
    // Compiled C: `LEN("\\\\")` = 4. The lexer collapses nothing; `$T` is what
    // turns `\\` into one backslash.
    assert_eq!(eval_str(r#""a\\b""#), StackValue::Str("a\\\\b".into()));
}

#[test]
fn test_string_literal_empty() {
    assert_eq!(eval_str(r#""""#), StackValue::Str("".into()));
}

// --- String variable tests ---

#[test]
fn test_string_var_push() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[0] = "hello".into(); // AA
    let result = eval_str_with("AA", &mut inputs);
    assert_eq!(result, StackValue::Str("hello".into()));
}

#[test]
fn test_string_var_store() {
    let mut inputs = StringInputs::new();
    // R10-9: bare `AA:="test"` is CALC_ERR_INCOMPLETE in sCalcPostfix — a store
    // leaves nothing on the stack, and the program must end with exactly one value.
    let compiled = scalc_compile(r#"AA:="test";AA"#).unwrap();
    scalc_eval(&compiled, &mut inputs).unwrap();
    assert_eq!(inputs.str_vars[0], "test");
}

#[test]
fn test_string_var_bb() {
    let mut inputs = StringInputs::new();
    inputs.str_vars[1] = "world".into(); // BB
    let result = eval_str_with("BB", &mut inputs);
    assert_eq!(result, StackValue::Str("world".into()));
}

// --- String concat (+) ---

#[test]
fn test_string_concat() {
    assert_eq!(
        eval_str(r#""hello" + "world""#),
        StackValue::Str("helloworld".into())
    );
}

#[test]
fn test_string_concat_empty() {
    assert_eq!(eval_str(r#""hello" + """#), StackValue::Str("hello".into()));
}

// --- String subtract (-) ---

#[test]
fn test_string_subtract_first_match() {
    assert_eq!(
        eval_str(r#""abcabc" - "b""#),
        StackValue::Str("acabc".into())
    );
}

#[test]
fn test_string_subtract_no_match() {
    assert_eq!(
        eval_str(r#""hello" - "xyz""#),
        StackValue::Str("hello".into())
    );
}

#[test]
fn test_string_subtract_full() {
    assert_eq!(eval_str(r#""abc" - "abc""#), StackValue::Str("".into()));
}

// --- String comparison ---

#[test]
fn test_string_eq() {
    assert_eq!(eval_str(r#""abc" == "abc""#), StackValue::Double(1.0));
    assert_eq!(eval_str(r#""abc" == "def""#), StackValue::Double(0.0));
}

#[test]
fn test_string_ne() {
    assert_eq!(eval_str(r#""abc" != "def""#), StackValue::Double(1.0));
    assert_eq!(eval_str(r#""abc" != "abc""#), StackValue::Double(0.0));
}

#[test]
fn test_string_lt() {
    assert_eq!(eval_str(r#""abc" < "def""#), StackValue::Double(1.0));
    assert_eq!(eval_str(r#""def" < "abc""#), StackValue::Double(0.0));
}

#[test]
fn test_string_le() {
    assert_eq!(eval_str(r#""abc" <= "abc""#), StackValue::Double(1.0));
    assert_eq!(eval_str(r#""abc" <= "def""#), StackValue::Double(1.0));
}

#[test]
fn test_string_gt() {
    assert_eq!(eval_str(r#""def" > "abc""#), StackValue::Double(1.0));
}

#[test]
fn test_string_ge() {
    assert_eq!(eval_str(r#""abc" >= "abc""#), StackValue::Double(1.0));
}

// --- Mixed string/double operands ---
//
// These three cases asserted TypeMismatch. C raises no such error: a numeric
// position COERCES a string with atof (`toDouble`, sCalcPerform.c:80-83), and a
// binary operator with a string branch takes it only when BOTH sides are
// strings. Compiled sCalcPerform answers each of them, so the assertions now
// carry C's value instead of an error the C engine cannot produce.
// tests/calc_string_coercion.rs covers the rule in full.

#[test]
fn a_string_plus_a_double_coerces_the_string() {
    let mut inputs = StringInputs::new();
    // atof("abc") is 0, so this is 0 + 1.
    assert_eq!(
        scalc(r#""abc" + 1"#, &mut inputs).unwrap(),
        StackValue::Double(1.0)
    );
}

#[test]
fn a_double_plus_a_string_coerces_the_string() {
    let mut inputs = StringInputs::new();
    assert_eq!(
        scalc(r#"3 + "12""#, &mut inputs).unwrap(),
        StackValue::Double(15.0)
    );
}

#[test]
fn a_mixed_comparison_compares_numerically() {
    let mut inputs = StringInputs::new();
    assert_eq!(
        scalc(r#""3" < 20"#, &mut inputs).unwrap(),
        StackValue::Double(1.0)
    );
}

// --- STR/DBL conversion ---

// C `to_string` is `cvtDoubleToString(d, s, 8)` — a FIXED-POINT rendering with 8
// fractional digits, not a shortest-round-trip one (R11-1). See
// `tests/scalc_double_to_string.rs` for the compiled-C table.

#[test]
fn test_str_function() {
    assert_eq!(eval_str("STR(3.14)"), StackValue::Str("3.14000000".into()));
}

#[test]
fn test_str_integer() {
    assert_eq!(eval_str("STR(42)"), StackValue::Str("42.00000000".into()));
}

#[test]
fn test_dbl_function() {
    assert_eq!(eval_str(r#"DBL("42")"#), StackValue::Double(42.0));
}

#[test]
fn test_dbl_float_string() {
    assert_eq!(eval_str(r#"DBL("3.14")"#), StackValue::Double(3.14));
}

#[test]
fn test_dbl_invalid_string() {
    assert_eq!(eval_str(r#"DBL("abc")"#), StackValue::Double(0.0));
}

#[test]
fn test_dbl_plus_number() {
    assert_eq!(eval_str(r#"DBL("12") + 3"#), StackValue::Double(15.0));
}

// --- LEN / BYTE ---

#[test]
fn test_len_function() {
    assert_eq!(eval_str(r#"LEN("hello")"#), StackValue::Double(5.0));
}

#[test]
fn test_len_empty() {
    assert_eq!(eval_str(r#"LEN("")"#), StackValue::Double(0.0));
}

#[test]
fn test_byte_function() {
    assert_eq!(eval_str(r#"BYTE("A")"#), StackValue::Double(65.0));
}

#[test]
fn test_byte_empty() {
    assert_eq!(eval_str(r#"BYTE("")"#), StackValue::Double(0.0));
}

#[test]
fn test_byte_lowercase() {
    assert_eq!(eval_str(r#"BYTE("a")"#), StackValue::Double(97.0));
}

// --- Numeric expressions still work in string evaluator ---

#[test]
fn test_numeric_add() {
    assert_eq!(eval_str("1+2"), StackValue::Double(3.0));
}

#[test]
fn test_numeric_sin() {
    let result = eval_str("SIN(0)");
    match result {
        StackValue::Double(v) => assert!((v - 0.0).abs() < 1e-10),
        _ => panic!("expected Double"),
    }
}

#[test]
fn test_numeric_ternary() {
    assert_eq!(eval_str("1?2:3"), StackValue::Double(2.0));
    assert_eq!(eval_str("0?2:3"), StackValue::Double(3.0));
}

#[test]
fn test_numeric_variables() {
    let mut inputs = StringInputs::new();
    inputs.num_vars[0] = 10.0; // A
    inputs.num_vars[1] = 20.0; // B
    let result = eval_str_with("A+B", &mut inputs);
    assert_eq!(result, StackValue::Double(30.0));
}

#[test]
fn test_max_string() {
    assert_eq!(
        eval_str(r#"MAX("apple","banana")"#),
        StackValue::Str("banana".into())
    );
}

#[test]
fn test_min_string() {
    assert_eq!(
        eval_str(r#"MIN("apple","banana")"#),
        StackValue::Str("apple".into())
    );
}

#[test]
fn test_numeric_assign_and_use() {
    let mut inputs = StringInputs::new();
    let result = scalc("A:=5;A+1", &mut inputs).unwrap();
    assert_eq!(result, StackValue::Double(6.0));
    assert_eq!(inputs.num_vars[0], 5.0);
}
