use epics_base_rs::calc::{
    CalcError, NumericInputs, StackValue, StringInputs, calc, compile, eval, scalc, scalc_compile,
};

fn make_inputs(vals: &[(u8, f64)]) -> NumericInputs {
    let mut inputs = NumericInputs::new();
    for &(idx, val) in vals {
        inputs.vars[idx as usize] = val;
    }
    inputs
}

/// Evaluate through the **sCalc** engine. `>?`, `<?` and `NRNDM` are in
/// `sCalcPostfix.c`'s element table but not in epics-base `postfix.c`'s, so the
/// numeric engine refuses them at compile time (`CALC_ERR_SYNTAX`).
fn assert_scalc(expr: &str, inputs: &[(u8, f64)], expected: f64) {
    let mut inp = StringInputs::new();
    for &(idx, val) in inputs {
        inp.num_vars[idx as usize] = val;
    }
    match scalc(expr, &mut inp).unwrap() {
        StackValue::Double(result) => assert!(
            (result - expected).abs() < 1e-9,
            "Expression '{expr}': expected {expected}, got {result}"
        ),
        other => panic!("Expression '{expr}': expected a double, got {other:?}"),
    }
}

fn assert_calc(expr: &str, inputs: &[(u8, f64)], expected: f64) {
    let mut inp = make_inputs(inputs);
    let result = calc(expr, &mut inp).unwrap();
    assert!(
        (result - expected).abs() < 1e-9,
        "Expression '{}': expected {}, got {}",
        expr,
        expected,
        result
    );
}

fn assert_calc_nan(expr: &str, inputs: &[(u8, f64)]) {
    let mut inp = make_inputs(inputs);
    let result = calc(expr, &mut inp).unwrap();
    assert!(
        result.is_nan(),
        "Expression '{}': expected NaN, got {}",
        expr,
        result
    );
}

// ===== Basic Arithmetic =====

#[test]
fn test_literal_add() {
    assert_calc("1+2", &[], 3.0);
}

#[test]
fn test_precedence() {
    assert_calc("1+2*3", &[], 7.0);
}

#[test]
fn test_parentheses() {
    assert_calc("(1+2)*3", &[], 9.0);
}

#[test]
fn test_subtraction() {
    assert_calc("10-3", &[], 7.0);
}

#[test]
fn test_division() {
    assert_calc("10/4", &[], 2.5);
}

#[test]
fn test_modulo() {
    assert_calc("7%3", &[], 1.0);
}

#[test]
fn test_power() {
    assert_calc("2^10", &[], 1024.0);
    assert_calc("2**3", &[], 8.0);
}

// ===== Variables =====

#[test]
fn test_variable_add() {
    assert_calc("A+B", &[(0, 3.0), (1, 4.0)], 7.0);
}

#[test]
fn test_unary_neg() {
    assert_calc("-A", &[(0, 5.0)], -5.0);
}

#[test]
fn test_double_neg() {
    assert_calc("--A", &[(0, 5.0)], 5.0);
}

// ===== Comparison =====

#[test]
fn test_gt_true() {
    assert_calc("A>B", &[(0, 5.0), (1, 3.0)], 1.0);
}

#[test]
fn test_gt_false() {
    assert_calc("A>B", &[(0, 3.0), (1, 4.0)], 0.0);
}

#[test]
fn test_lt() {
    assert_calc("A<B", &[(0, 3.0), (1, 4.0)], 1.0);
}

#[test]
fn test_eq() {
    assert_calc("A==B", &[(0, 5.0), (1, 5.0)], 1.0);
    assert_calc("A==B", &[(0, 5.0), (1, 6.0)], 0.0);
}

#[test]
fn test_ne() {
    assert_calc("A!=B", &[(0, 5.0), (1, 6.0)], 1.0);
    assert_calc("A#B", &[(0, 5.0), (1, 5.0)], 0.0);
}

#[test]
fn test_le() {
    assert_calc("A<=B", &[(0, 3.0), (1, 4.0)], 1.0);
    assert_calc("A<=B", &[(0, 4.0), (1, 4.0)], 1.0);
    assert_calc("A<=B", &[(0, 5.0), (1, 4.0)], 0.0);
}

#[test]
fn test_ge() {
    assert_calc("A>=B", &[(0, 5.0), (1, 4.0)], 1.0);
    assert_calc("A>=B", &[(0, 4.0), (1, 4.0)], 1.0);
    assert_calc("A>=B", &[(0, 3.0), (1, 4.0)], 0.0);
}

// ===== Logical =====

#[test]
fn test_and() {
    assert_calc("A&&B", &[(0, 1.0), (1, 1.0)], 1.0);
    assert_calc("A&&B", &[(0, 1.0), (1, 0.0)], 0.0);
}

#[test]
fn test_or() {
    assert_calc("A||B", &[(0, 0.0), (1, 1.0)], 1.0);
    assert_calc("A||B", &[(0, 0.0), (1, 0.0)], 0.0);
}

#[test]
fn test_not() {
    assert_calc("!A", &[(0, 0.0)], 1.0);
    assert_calc("!A", &[(0, 5.0)], 0.0);
}

// ===== Bitwise =====

#[test]
fn test_bit_and() {
    assert_calc("A&B", &[(0, 12.0), (1, 10.0)], 8.0);
}

#[test]
fn test_bit_or() {
    assert_calc("A|B", &[(0, 12.0), (1, 10.0)], 14.0);
}

#[test]
fn test_bit_xor() {
    // Use XOR keyword
    assert_calc("12 XOR 10", &[], 6.0);
}

#[test]
fn test_bit_not() {
    assert_calc("~0", &[], -1.0);
}

#[test]
fn test_shift() {
    assert_calc("A>>B", &[(0, 16.0), (1, 2.0)], 4.0);
    assert_calc("A<<B", &[(0, 1.0), (1, 4.0)], 16.0);
}

// ===== Functions =====

#[test]
fn test_sin() {
    let mut inputs = make_inputs(&[(0, std::f64::consts::FRAC_PI_2)]);
    let result = calc("SIN(A)", &mut inputs).unwrap();
    assert!((result - 1.0).abs() < 1e-9);
}

#[test]
fn test_cos() {
    assert_calc("COS(0)", &[], 1.0);
}

#[test]
fn test_tan() {
    assert_calc("TAN(0)", &[], 0.0);
}

#[test]
fn test_asin() {
    let mut inputs = make_inputs(&[(0, 1.0)]);
    let result = calc("ASIN(A)", &mut inputs).unwrap();
    assert!((result - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
}

#[test]
fn test_abs() {
    assert_calc("ABS(-5)", &[], 5.0);
    assert_calc("ABS(5)", &[], 5.0);
}

#[test]
fn test_sqrt() {
    assert_calc("SQRT(9)", &[], 3.0);
    assert_calc("SQR(16)", &[], 4.0);
}

#[test]
fn test_exp_log() {
    assert_calc("LN(EXP(1))", &[], 1.0);
    assert_calc("LOG(100)", &[], 2.0);
    assert_calc("LOGE(EXP(2))", &[], 2.0);
}

#[test]
fn test_ceil_floor_nint() {
    assert_calc("CEIL(1.2)", &[], 2.0);
    assert_calc("FLOOR(1.8)", &[], 1.0);
    assert_calc("NINT(1.6)", &[], 2.0);
    assert_calc("NINT(1.4)", &[], 1.0);
    assert_calc("NINT(-1.6)", &[], -2.0);
}

#[test]
fn test_sinh_cosh_tanh() {
    assert_calc("SINH(0)", &[], 0.0);
    assert_calc("COSH(0)", &[], 1.0);
    assert_calc("TANH(0)", &[], 0.0);
}

// ===== Vararg functions =====

#[test]
fn test_min_vararg() {
    assert_calc("MIN(A,B,C)", &[(0, 3.0), (1, 1.0), (2, 2.0)], 1.0);
    assert_calc("MIN(A,B)", &[(0, 5.0), (1, 3.0)], 3.0);
}

#[test]
fn test_max_vararg() {
    assert_calc("MAX(A,B,C)", &[(0, 3.0), (1, 7.0), (2, 2.0)], 7.0);
    assert_calc("MAX(A,B)", &[(0, 5.0), (1, 3.0)], 5.0);
}

// ===== Max/Min operators =====

#[test]
fn test_max_op() {
    assert_scalc("A>?B", &[(0, 3.0), (1, 5.0)], 5.0);
    assert_scalc("A>?B", &[(0, 7.0), (1, 5.0)], 7.0);
    assert_eq!(compile("A>?B").unwrap_err(), CalcError::Syntax);
}

#[test]
fn test_min_op() {
    assert_scalc("A<?B", &[(0, 3.0), (1, 5.0)], 3.0);
    assert_scalc("A<?B", &[(0, 7.0), (1, 5.0)], 5.0);
    assert_eq!(compile("A<?B").unwrap_err(), CalcError::Syntax);
}

// ===== Ternary =====

#[test]
fn test_ternary_true() {
    assert_calc("A?B:C", &[(0, 1.0), (1, 10.0), (2, 20.0)], 10.0);
}

#[test]
fn test_ternary_false() {
    assert_calc("A?B:C", &[(0, 0.0), (1, 10.0), (2, 20.0)], 20.0);
}

#[test]
fn test_nested_ternary() {
    // A ? (B ? 1 : 2) : 3
    assert_calc("A?(B?1:2):3", &[(0, 1.0), (1, 1.0)], 1.0);
    assert_calc("A?(B?1:2):3", &[(0, 1.0), (1, 0.0)], 2.0);
    assert_calc("A?(B?1:2):3", &[(0, 0.0), (1, 1.0)], 3.0);
}

// ===== Assignment =====

#[test]
fn test_assign() {
    let compiled = compile("A:=5;A+1").unwrap();
    let mut inputs = NumericInputs::new();
    let result = eval(&compiled, &mut inputs).unwrap();
    assert!((result - 6.0).abs() < 1e-9);
    assert!((inputs.vars[0] - 5.0).abs() < 1e-9);
}

#[test]
fn test_assign_expression() {
    let compiled = compile("A:=2+3;A*2").unwrap();
    let mut inputs = NumericInputs::new();
    let result = eval(&compiled, &mut inputs).unwrap();
    assert!((result - 10.0).abs() < 1e-9);
    assert!((inputs.vars[0] - 5.0).abs() < 1e-9);
}

// ===== Constants =====

#[test]
fn test_pi() {
    let mut inputs = NumericInputs::new();
    let result = calc("PI", &mut inputs).unwrap();
    assert!((result - std::f64::consts::PI).abs() < 1e-9);
}

#[test]
fn test_d2r() {
    let mut inputs = NumericInputs::new();
    let result = calc("D2R", &mut inputs).unwrap();
    assert!((result - std::f64::consts::PI / 180.0).abs() < 1e-15);
}

#[test]
fn test_r2d() {
    let mut inputs = NumericInputs::new();
    let result = calc("R2D", &mut inputs).unwrap();
    assert!((result - 180.0 / std::f64::consts::PI).abs() < 1e-9);
}

// ===== Special =====

#[test]
fn test_rndm_range() {
    // base `calcRandom` is `seed / 65535.0` (calcPerform.c:521-526), so its
    // range is the CLOSED [0, 1]: zero is a legal draw. sCalc/aCalc exclude it
    // ((seed+1)/65536, because NRNDM takes a log); base has no NRNDM.
    let mut inputs = NumericInputs::new();
    for _ in 0..100 {
        let r = calc("RNDM", &mut inputs).unwrap();
        assert!((0.0..=1.0).contains(&r), "RNDM out of range: {}", r);
    }
}

#[test]
fn test_nrndm() {
    // NRNDM is sCalc/aCalc only; the numeric engine refuses it at compile.
    let mut inputs = StringInputs::new();
    let r = scalc("NRNDM", &mut inputs).unwrap();
    match r {
        StackValue::Double(v) => assert!(v.is_finite(), "NRNDM not finite: {v}"),
        other => panic!("NRNDM must produce a double, got {other:?}"),
    }
    assert_eq!(
        calc("NRNDM", &mut NumericInputs::new()),
        Err(CalcError::Syntax)
    );
}

// ===== Division by zero =====

#[test]
fn test_div_by_zero() {
    let mut inp = make_inputs(&[]);
    let result = calc("1/0", &mut inp).unwrap();
    assert!(result.is_infinite(), "Expected Inf, got {result}");
}

#[test]
fn test_mod_by_zero() {
    assert_calc_nan("1%0", &[]);
}

// ===== ISNAN, ISINF, FINITE =====

#[test]
fn test_isnan() {
    assert_calc("ISNAN(0)", &[], 0.0);
    // NaN literal
    let mut inputs = NumericInputs::new();
    inputs.vars[0] = f64::NAN;
    let result = calc("ISNAN(A)", &mut inputs).unwrap();
    assert!((result - 1.0).abs() < 1e-9);
}

#[test]
fn test_isinf() {
    assert_calc("ISINF(0)", &[], 0.0);
    let mut inputs = NumericInputs::new();
    inputs.vars[0] = f64::INFINITY;
    let result = calc("ISINF(A)", &mut inputs).unwrap();
    assert!((result - 1.0).abs() < 1e-9);
}

#[test]
fn test_finite() {
    assert_calc("FINITE(1)", &[], 1.0);
    let mut inputs = NumericInputs::new();
    inputs.vars[0] = f64::INFINITY;
    let result = calc("FINITE(A)", &mut inputs).unwrap();
    assert!((result - 0.0).abs() < 1e-9);
}

// ===== Case insensitivity =====

#[test]
fn test_case_insensitive() {
    assert_calc("sin(a)+cos(b)", &[(0, 0.0), (1, 0.0)], 1.0);
}

// ===== Complex expressions =====

#[test]
fn test_complex_1() {
    // (A + B) * (C - D) / E
    assert_calc(
        "(A+B)*(C-D)/E",
        &[(0, 2.0), (1, 3.0), (2, 10.0), (3, 4.0), (4, 2.0)],
        15.0,
    );
}

#[test]
fn test_complex_2() {
    // SIN(PI/6) should be ~0.5
    let mut inputs = NumericInputs::new();
    let result = calc("SIN(PI/6)", &mut inputs).unwrap();
    assert!((result - 0.5).abs() < 1e-9);
}

#[test]
fn test_atan2() {
    // ATAN2(y, x) computes atan2(x, y) per EPICS convention
    // atan2(1, 0) = PI/2
    let mut inputs = NumericInputs::new();
    let result = calc("ATAN2(0,1)", &mut inputs).unwrap();
    assert!((result - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
}

#[test]
fn test_keyword_and_or() {
    assert_calc("1 AND 1", &[], 1.0);
    assert_calc("1 OR 0", &[], 1.0);
}

/// This asserted `compile("")` succeeds and evaluates to 0.0. Base C does
/// neither: `postfix("")` is `CALC_ERR_NULL_ARG` (-1, `postfix.c:235-241`), and
/// the empty program it leaves in RPCL makes `calcPerform` return -1 — which is
/// how a calc record with an empty CALC alarms on every process instead of
/// quietly reading 0. tests/calc_empty_program.rs covers the rule in full.
#[test]
fn test_empty_expression() {
    assert_eq!(compile("").err(), Some(CalcError::NullArg));
}

/// `LOG2` is in no C element table — base, sCalc and aCalc all lex it as `LOG`
/// applied to the literal `2`, which makes `LOG2(8)` an operand followed by
/// `(`: CALC_ERR_SYNTAX. It is not a base-2 logarithm in any dialect.
/// See tests/calc_grammar_tables.rs (R7-3).
#[test]
fn test_log2() {
    assert!(matches!(compile("LOG2(8)"), Err(CalcError::Syntax)));
    assert_calc("LOG2", &[], 2.0f64.log10());
}

/// `INT` is a synApps sCalc/aCalc symbol (an alias of NINT). base's `postfix()`
/// has no such element, so `calc`/`calcout` reject it at compile time.
#[test]
fn test_int() {
    assert!(matches!(compile("INT(3.7)"), Err(CalcError::Syntax)));
    assert!(matches!(compile("INT(-3.7)"), Err(CalcError::Syntax)));
}

#[test]
fn test_multiple_assignments() {
    let compiled = compile("A:=3;B:=4;A+B").unwrap();
    let mut inputs = NumericInputs::new();
    let result = eval(&compiled, &mut inputs).unwrap();
    assert!((result - 7.0).abs() < 1e-9);
    assert!((inputs.vars[0] - 3.0).abs() < 1e-9);
    assert!((inputs.vars[1] - 4.0).abs() < 1e-9);
}

#[test]
fn test_hex_literal() {
    assert_calc("0xFF", &[], 255.0);
}

#[test]
fn test_float_with_exponent() {
    assert_calc("1e3", &[], 1000.0);
    assert_calc("1.5e2", &[], 150.0);
}

#[test]
fn test_nan_literal() {
    let mut inputs = NumericInputs::new();
    let result = calc("NAN", &mut inputs).unwrap();
    assert!(result.is_nan());
}

#[test]
fn test_inf_literal() {
    let mut inputs = NumericInputs::new();
    let result = calc("INF", &mut inputs).unwrap();
    assert!(result.is_infinite() && result > 0.0);
}

// ===== A..U input range (epics-base PR #655: 12 → 21 inputs) =====

#[test]
fn test_extended_inputs_m_through_u() {
    // M..U single-letter variables must be parsed and evaluated.
    // Pre-#655 the parser only accepted A..P; now A..U is the contract.
    let cases = [
        ("M+1", 12, 1.0, 2.0),
        ("N*2", 13, 3.0, 6.0),
        ("O-O", 14, 5.0, 0.0),
        ("P+0", 15, 7.0, 7.0),
        ("Q*Q", 16, 4.0, 16.0),
        ("R/2", 17, 8.0, 4.0),
        ("S+1", 18, 9.0, 10.0),
        ("T-1", 19, 11.0, 10.0),
        ("U+U", 20, 6.0, 12.0),
    ];
    for (expr, idx, val, expected) in cases {
        assert_calc(expr, &[(idx, val)], expected);
    }
}

#[test]
fn test_full_a_to_u_sum() {
    // A..U all set to 1; expression is the sum of all 21 inputs.
    // Verifies the engine accepts all 21 letters in a single expression.
    let inputs: Vec<(u8, f64)> = (0..21).map(|i| (i, 1.0)).collect();
    assert_calc("A+B+C+D+E+F+G+H+I+J+K+L+M+N+O+P+Q+R+S+T+U", &inputs, 21.0);
}

#[test]
fn test_double_letter_uu_parses() {
    // Doubled letters are sCalc string args / aCalc array args, and BOTH C
    // tables stop at LL: `FETCH_AA`..`FETCH_LL` (sCalcPostfix.c:118-186,
    // aCalcPostfix.c:108-190). base #655 widened the SINGLE letters to A..U;
    // it added no doubled-letter operand, and it did not touch synApps. The
    // compiled sCalcPostfix.c answers CALC_ERR_SYNTAX (11) for MM and UU —
    // this case used to assert they compile, which no C table supports.
    assert!(scalc_compile("LL").is_ok(), "sCalc has FETCH_LL");
    assert!(scalc_compile("MM").is_err(), "sCalc's table stops at LL");
    assert!(scalc_compile("UU").is_err(), "sCalc's table stops at LL");
    assert!(scalc_compile("VV").is_err());

    assert_eq!(compile("UU").unwrap_err(), CalcError::Syntax);
    assert!(compile("VV").is_err());
}
