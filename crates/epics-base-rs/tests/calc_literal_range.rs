//! R10-12 — base's compiler reads a double literal with `epicsParseDouble`
//! (`postfix.c:263`), which FAILS on `errno == ERANGE` (`epicsStdlib.c:164`).
//! A literal naming a number the format cannot hold is CALC_ERR_BAD_LITERAL, not
//! an infinity or a zero. sCalc and aCalc call bare `epicsStrtod`
//! (`sCalcPostfix.c:492`, `aCalcPostfix.c:462`) and never look at errno, so they
//! DO take the infinity and the zero.
//!
//! Every case below is the answer of the compiled C compilers on this host.

use epics_base_rs::calc::{CalcError, NumericInputs, acalc_compile, calc, compile, scalc_compile};

/// Compiled base postfix: BAD_LITERAL for each of these.
#[test]
fn base_rejects_a_literal_that_ranges_out() {
    for expr in [
        "1e400",      // overflow -> inf
        "1.8e308",    // overflow, just past DBL_MAX
        "1e-400",     // underflow -> 0
        "1e-320",     // underflow -> subnormal
        "2.2e-308",   // underflow -> subnormal, just under DBL_MIN
        "5e-324",     // the smallest subnormal is still ERANGE
        "1+1e400",    // and it is the LITERAL that is bad, wherever it sits
        "-1e400",     // `-` is the unary operator; `1e400` is the literal
        "1e400;A:=1", // ...including in a statement that would otherwise compile
    ] {
        assert_eq!(
            compile(expr).unwrap_err(),
            CalcError::BadLiteral,
            "base must reject {expr:?}"
        );
    }
}

/// Negative control — the boundary on the other side. Both are representable, so
/// no ERANGE, so no error. `0e999` is the one that separates "the value came out
/// zero" from "the TEXT names zero": C raises ERANGE only for a nonzero
/// significand, so a zero significand with any exponent is exact and fine.
#[test]
fn base_accepts_the_representable_neighbours() {
    for expr in [
        "1e308",
        "1.7976931348623157e308", // DBL_MAX
        "2.3e-308",               // the smallest normal decade
        "0e999",
        "0.0e-400",
        "0",
        "INF", // a SPELLED infinity is exact: strtod sets no ERANGE
        "NAN",
    ] {
        assert!(compile(expr).is_ok(), "base must accept {expr:?}");
    }
    // ...and the value survives.
    let mut inp = NumericInputs::new();
    assert_eq!(calc("1e308", &mut inp).unwrap(), 1e308);
    assert!(calc("INF", &mut inp).unwrap().is_infinite());
}

/// The other half of the finding, and the reason this is a per-table property
/// rather than a global one: synApps reads the same literal with bare
/// `epicsStrtod` and keeps whatever it returned.
#[test]
fn scalc_and_acalc_take_the_infinity_and_the_zero() {
    for expr in ["1e400", "1e-400", "2.2e-308"] {
        assert!(scalc_compile(expr).is_ok(), "sCalc must accept {expr:?}");
        assert!(acalc_compile(expr).is_ok(), "aCalc must accept {expr:?}");
    }
}
