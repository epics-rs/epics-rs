//! R18-3: each calc flavour compiles against the stack ITS `*Perform`
//! allocates, not one shared literal.
//!
//! C rejects an over-deep expression at COMPILE time, each compiler against its
//! own evaluator's stack:
//!
//! ```c
//!     if (runtime_depth >= CALCPERFORM_STACK) { … CALC_ERR_OVERFLOW }   // postfix.c:469,      80
//!     if (runtime_depth >= SCALC_STACKSIZE)   { … CALC_ERR_OVERFLOW }   // sCalcPostfix.c:825, 30
//!     if (runtime_depth >= ACALC_STACKSIZE)   { … CALC_ERR_OVERFLOW }   // aCalcPostfix.c:755, 20
//! ```
//!
//! The port had ONE hardcoded 30 in the shared `compile()` — correct for sCalc,
//! and wrong in OPPOSITE directions for the other two: a depth-35 numeric CALC
//! that every C IOC accepts was a database-load failure, while aCalc accepted
//! depths 20..29 that C refuses.
//!
//! **Depth probe: `MAX(1,1,…,1)`.** Every argument is pushed before `MAX`'s
//! runtime effect of `-(n-1)` applies, so `n` arguments peak at runtime depth
//! `n` — while the OPERATOR stack holds only `LPAREN` + `MAX`. (Nested parens
//! reach the same depth but cost two operator-stack entries per level, and C's
//! own `postfix()` smashes its `ELEMENT stack[80]` at 40 levels — a construction
//! no C boundary can be read from.)
//!
//! Compiled C, all three compilers, this exact construction:
//!
//! ```text
//!   MAX-args 19:  base OK   sCalc OK    aCalc OK
//!   MAX-args 20:  base OK   sCalc OK    aCalc ERR(10)   <- ACALC_STACKSIZE
//!   MAX-args 29:  base OK   sCalc OK    aCalc ERR(10)
//!   MAX-args 30:  base OK   sCalc ERR(10)  aCalc ERR(10) <- SCALC_STACKSIZE
//!   MAX-args 79:  base OK
//!   MAX-args 80:  base ERR(10) "Runtime stack overflow"  <- CALCPERFORM_STACK
//! ```
//!
//! (`postfix()` from the built libCom; `sCalcPostfix`/`aCalcPostfix` compiled
//! standalone from the synApps sources. Error 10 is `CALC_ERR_OVERFLOW`.)

use epics_base_rs::calc::{CalcError, NumericInputs, acalc_compile, compile, eval, scalc_compile};

/// `MAX(1,1,…,1)` with `n` arguments — peak runtime depth `n`.
fn depth(n: usize) -> String {
    let args = vec!["1"; n].join(",");
    format!("MAX({args})")
}

/// Each flavour rejects AT its own limit (C's `>=`) and accepts one below it.
/// Six cases, one per side of each of the three C boundaries.
#[test]
fn each_flavour_rejects_at_its_own_c_limit() {
    assert!(acalc_compile(&depth(19)).is_ok(), "aCalc: 19 < 20");
    assert!(
        matches!(acalc_compile(&depth(20)), Err(CalcError::Overflow)),
        "aCalc: runtime_depth >= ACALC_STACKSIZE (20) is CALC_ERR_OVERFLOW"
    );

    assert!(scalc_compile(&depth(29)).is_ok(), "sCalc: 29 < 30");
    assert!(
        matches!(scalc_compile(&depth(30)), Err(CalcError::Overflow)),
        "sCalc: runtime_depth >= SCALC_STACKSIZE (30) is CALC_ERR_OVERFLOW"
    );

    assert!(compile(&depth(79)).is_ok(), "base: 79 < 80");
    assert!(
        matches!(compile(&depth(80)), Err(CalcError::Overflow)),
        "base: runtime_depth >= CALCPERFORM_STACK (80) is CALC_ERR_OVERFLOW"
    );
}

/// The band the shared literal got wrong in BOTH directions. Depth 25 is
/// accepted by base and sCalc and refused by aCalc; the port accepted all three.
/// Depth 35 is accepted by base alone; the port refused all three.
#[test]
fn the_bands_the_shared_limit_of_30_got_wrong() {
    // 20..29: aCalc alone refuses. The port accepted it into an ACALC-20 stack.
    assert!(compile(&depth(25)).is_ok());
    assert!(scalc_compile(&depth(25)).is_ok());
    assert!(
        matches!(acalc_compile(&depth(25)), Err(CalcError::Overflow)),
        "aCalc refuses 25; the shared limit of 30 waved it through"
    );

    // 30..79: base alone accepts. The port made it a database-load failure.
    assert!(
        compile(&depth(35)).is_ok(),
        "base accepts depth 35; the shared limit of 30 rejected an expression \
         every C IOC loads"
    );
    assert!(matches!(
        scalc_compile(&depth(35)),
        Err(CalcError::Overflow)
    ));
    assert!(matches!(
        acalc_compile(&depth(35)),
        Err(CalcError::Overflow)
    ));
}

/// The depth-35 base expression does not just compile — it RUNS, on a stack the
/// evaluator sizes for it. Compiled softIoc, `record(calc)` with a depth-35
/// `1+(1+(1+…))` CALC: `VAL=35`.
#[test]
fn a_depth_35_base_expression_compiles_and_evaluates() {
    let mut nest = String::from("1");
    for _ in 1..35 {
        nest = format!("1+({nest})");
    }
    let program = compile(&nest).expect("CALCPERFORM_STACK is 80: depth 35 compiles");
    let mut inputs = NumericInputs::new();
    assert_eq!(
        eval(&program, &mut inputs).unwrap(),
        35.0,
        "compiled softIoc prints VAL=35 for this CALC"
    );
}
