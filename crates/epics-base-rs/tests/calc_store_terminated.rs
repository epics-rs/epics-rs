//! R10-9 — an assignment is not a program. All three C compilers end with the
//! same check (`postfix.c:499-502`, `sCalcPostfix.c:862-870`,
//! `aCalcPostfix.c:790-799`): `runtime_depth != 1` is CALC_ERR_INCOMPLETE, and
//! `:=` has runtime_effect -1 (`postfix.c:162`, `sCalcPostfix.c:226`). So a
//! source that ENDS in a store leaves nothing on the stack and does not compile.
//!
//! The port exempted depth 0 when the last opcode was a store, so every one of
//! these compiled and then evaluated to a stale value. Compiled base `postfix`
//! and synApps `sCalcPostfix` on this host both answer err=8 for `A:=5`,
//! `AA:="x"`, `A:=5;B:=6`, `A:=B:=5`, and 0 for `A:=5;A`.

use epics_base_rs::calc::{CalcError, acalc_compile, compile, scalc_compile};

/// The boundary: the LAST thing the program does is a store.
#[test]
fn a_store_terminated_source_is_incomplete() {
    for expr in ["A:=5", "A:=5;B:=6", "A:=B:=5"] {
        assert!(
            matches!(compile(expr), Err(CalcError::Incomplete)),
            "calc {expr}"
        );
        assert!(
            matches!(scalc_compile(expr), Err(CalcError::Incomplete)),
            "scalc {expr}"
        );
        assert!(
            matches!(acalc_compile(expr), Err(CalcError::Incomplete)),
            "acalc {expr}"
        );
    }
    // sCalc's string store is the same shape and the same answer.
    assert!(matches!(
        scalc_compile("AA:=\"x\""),
        Err(CalcError::Incomplete)
    ));
}

/// Negative control: a store is still legal — the PROGRAM just has to end at
/// depth 1. Naming a value after the store does it.
#[test]
fn a_store_followed_by_a_value_still_compiles() {
    assert!(compile("A:=5;A").is_ok());
    assert!(compile("A:=5;B:=6;A+B").is_ok());
    assert!(scalc_compile("A:=5;A").is_ok());
    assert!(scalc_compile("AA:=\"x\";AA").is_ok());
    assert!(acalc_compile("A:=5;A").is_ok());
}

/// The other way round, and the boundary the rule must not over-reject: `;` does
/// NOT reset the depth (C's EXPR_TERMINATOR has runtime_effect 0 and only
/// flushes the operator stack, `postfix.c:...`/`sCalcPostfix.c:785-799`). So a
/// leading VALUE plus a trailing store nets back to 1 and compiles — this is
/// upstream areaDetector's own NDPluginCircularBuff trigger expression, and
/// compiled base `postfix` accepts it (and rejects it once a third value is
/// appended).
#[test]
fn a_value_followed_by_a_store_compiles() {
    assert!(compile("A>1.5*H && E>50;H:=A").is_ok());
    assert!(matches!(
        compile("A>1.5*H && E>50;H:=A;A"),
        Err(CalcError::Incomplete)
    ));
}
