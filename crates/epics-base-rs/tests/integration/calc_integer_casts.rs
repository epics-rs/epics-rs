//! R8-5 — every calc dialect's integer operators cast their `double` operands
//! the way THAT dialect's C does, and the three do not agree.
//!
//! | dialect | bit / shift | MODULO | NINT |
//! |---|---|---|---|
//! | base `calcPerform.c` | `d2i`/`d2ui` (:324-325 at R7.0.10) | `(epicsInt32)` (:162-164) — NaN on zero | `(epicsInt32)` (:292) |
//! | sCalc `sCalcPerform.c` | plain `(long)` (:578-631) | `(long)` (:1109) — error on zero | `(long)` (:718) |
//! | aCalc `aCalcPerform.c` | plain `(int)` (:1355-1357) | `(int)` (:650,:674,:701) — myMAXFLOAT on zero | `(long)` (:828,:1085) |
//!
//! **CBUG-A2 — each engine narrows MODULO and NINT with its OWN dialect's cast**,
//! mirroring that engine's pristine/fixed C. They genuinely disagree because the
//! C widths do: `3e9` is `(epicsInt32)3e9`, out of range, in base, `(long)3e9 =
//! 3e9` in sCalc, and (for MODULO) `(int)3e9` out of range in aCalc. So `3e9 % 7`
//! is `-2` (base and aCalc, `INT32_MIN % 7`) and `4` (sCalc). The structure is a
//! single `cast::nint`/`cast::imod` parameterised by the narrowing fn — one
//! logic, per-engine narrowing.
//!
//! Note aCalc is internally inconsistent: NINT is `(long)` (64-bit) but MODULO
//! is `(int)` (32-bit) — verified in aCalcPerform.c. Base narrows MODULO and
//! NINT as released base does, not through the `d2i` of unmerged PR #925
//! (CBUG-A5).
//!
//! The plain casts are UNDEFINED in C outside their range (and for NaN); the
//! port takes the x86-64 `cvttsd2si` value through `c_cast`. Expected base
//! values are R7.0.10's `calcPerform.c` compiled with gcc on x86-64.
//!
//! This test used to pin the earlier clean no-narrow deviation (`3e9 % 7 == 4`,
//! `NINT(3e9) == 3e9` in every dialect).

use epics_base_rs::calc::{ArrayInputs, NumericInputs, StringInputs, acalc, calc, scalc};

fn n(expr: &str, a: f64) -> f64 {
    let mut inp = NumericInputs::new();
    inp.vars[0] = a;
    calc(expr, &mut inp).expect("compiles and evaluates")
}

/// A constant-only expression (no `A`).
fn nc(expr: &str) -> f64 {
    n(expr, 0.0)
}

fn s(expr: &str, a: f64) -> f64 {
    let mut inp = StringInputs::new();
    inp.num_vars[0] = a;
    match scalc(expr, &mut inp).expect("compiles and evaluates") {
        epics_base_rs::calc::StackValue::Double(d) => d,
        other => panic!("expected a double, got {other:?}"),
    }
}

fn a(expr: &str, a0: f64) -> f64 {
    let mut inp = ArrayInputs::new(1);
    inp.num_vars[0] = a0;
    match acalc(expr, &mut inp).expect("compiles and evaluates") {
        epics_base_rs::calc::ArrayStackValue::Double(d) => d,
        other => panic!("expected a scalar, got {other:?}"),
    }
}

/// CBUG-A2 — `A % 7` with `A = 3e9` narrows through EACH dialect's own cast, and
/// the three genuinely disagree because their C widths do:
/// * base — `(epicsInt32)3e9` is out of int32 range: `INT32_MIN`, `% 7 == -2`
/// * sCalc — `(long)3e9 = 3000000000`, `% 7 == 4` (64-bit, no loss)
/// * aCalc — `(int)3e9` is out of int32 range: x86-64 `INT32_MIN`, `% 7 == -2`
///
/// Each equals its own pristine/fixed C. This used to pin the clean no-narrow
/// deviation (`4` everywhere).
#[test]
fn modulo_above_2_31_mirrors_each_dialect_width() {
    assert_eq!(n("A%7", 3e9), -2.0); // base (epicsInt32), INT32_MIN
    assert_eq!(s("A%7", 3e9), 4.0); // sCalc (long)
    assert_eq!(a("A%7", 3e9), -2.0); // aCalc (int), INT32_MIN
}

/// sCalc's C picks its evaluator from the `USES_STRING` marker
/// (`sCalcPostfix.c:447-475`) and its two evaluators cast MODULO at different
/// widths — `(int)` no-string (`sCalcPerform.c:562`) vs `(long)` string
/// (`:1108`). The port models the wider `(long)` path uniformly, so the marker
/// no longer perturbs the arithmetic: both are `4`. This test used to pin the
/// split.
#[test]
fn scalc_modulo_no_longer_depends_on_the_uses_string_marker() {
    assert_eq!(s("A%7", 3e9), 4.0, "(long)3e9 % 7, no-string program");
    // LEN("") is in C's USES_STRING opcode list, so C's string evaluator would
    // run; the port's single `(long)` path is width-independent.
    assert_eq!(
        s("A%7+LEN('')", 3e9),
        4.0,
        "same (long) narrowing, string program"
    );
}

/// A zero divisor is a THREE-way split, and the port had base's answer in all
/// three engines.
#[test]
fn zero_divisor_disposition_is_per_dialect() {
    // base: epicsNAN (calcPerform.c:166).
    assert!(n("A%0", 5.0).is_nan());

    // sCalc: `return(-1)` — the evaluation FAILS (sCalcPerform.c:560-561).
    let mut inp = StringInputs::new();
    inp.num_vars[0] = 5.0;
    assert!(
        scalc("A%0", &mut inp).is_err(),
        "sCalcPerform returns -1 for a zero MODULO divisor"
    );

    // aCalc: myMAXFLOAT == (float)1e35 (aCalcPerform.c:49,701).
    assert_eq!(a("A%0", 5.0), 1e35f32 as f64);
}

/// CBUG-A5 — a base divisor of 2^32 narrows to `INT32_MIN`, not to zero: released
/// base's `(epicsInt32)` does not wrap the way unmerged PR #925's `d2i` does, so
/// `5 % 2^32` is `5`, not NaN.
#[test]
fn base_divisor_at_2_32_is_not_a_zero_divisor() {
    assert_eq!(nc("5 % 4294967296"), 5.0);
    assert_eq!(n("A % 4294967296", 5.0), 5.0);
}

/// Base's bitwise ops REALLY do use `d2i` — this must not change. It is the
/// one place the uint32 reinterpretation is C.
#[test]
fn base_bitwise_still_uses_d2i() {
    assert_eq!(nc("3000000000 & 4294967295"), -1_294_967_296.0);
    assert_eq!(n("A | 0", 3e9), -1_294_967_296.0);
}

/// sCalc's bitwise ops are `(long)` — 64-bit, so 3e9 survives intact instead
/// of being reinterpreted as a negative int32.
#[test]
fn scalc_bitwise_is_long_wide() {
    assert_eq!(s("A|0", 3e9), 3e9);
    assert_eq!(s("A&0xFFFFFFFF", 3e9), 3e9);
}

/// aCalc's bitwise ops are a plain `(int)` cast, NOT `d2i`'s uint32
/// reinterpretation — so an out-of-range operand does not come back as
/// -1294967296. Where it DOES come back is CBUG-E2's call: the `(int)` cast is
/// UB on an out-of-range double; x86-64 answers INT32_MIN, as the port does.
#[test]
fn acalc_bitwise_is_a_plain_int_cast() {
    assert_ne!(a("A|0", 3e9), -1_294_967_296.0);
    assert_eq!(a("A|0", 3e9), i32::MIN as f64);
}

/// CBUG-A2 — NINT rounds, then narrows with EACH dialect's own cast:
/// * base — `(epicsInt32)` (`calcPerform.c:292`): `NINT(3e9) = INT32_MIN`
/// * sCalc — `(long)` (`sCalcPerform.c:730`): `NINT(3e9) = 3e9` (64-bit, no loss)
/// * aCalc — `(long)` (`aCalcPerform.c:839,1096`): `NINT(3e9) = 3e9` — aCalc
///   narrows NINT at 64-bit even though its MODULO is 32-bit `(int)` (an internal
///   synApps inconsistency)
///
/// This used to pin the clean no-narrow deviation (`3e9` in every dialect); note
/// sCalc/aCalc happen to still be `3e9` because `(long)` does not lose 3e9.
#[test]
fn nint_mirrors_each_dialect_width() {
    assert_eq!(n("NINT(A)", 3e9), -2_147_483_648.0); // base (epicsInt32)
    assert_eq!(s("NINT(A)", 3e9), 3e9); // sCalc (long)
    assert_eq!(a("NINT(A)", 3e9), 3e9); // aCalc (long)

    // Past each width's range: x86-64's i32::MIN for base, i64::MIN for
    // sCalc/aCalc `(long)`.
    assert_eq!(n("NINT(A)", 1e300), i32::MIN as f64);
    assert_eq!(s("NINT(A)", 1e300), i64::MIN as f64);
    assert_eq!(a("NINT(A)", 1e300), i64::MIN as f64);

    // In range, all three agree and round half away from zero — same as C.
    for expr_val in [(2.5, 3.0), (-2.5, -3.0), (2.4, 2.0)] {
        let (input, want) = expr_val;
        assert_eq!(n("NINT(A)", input), want);
        assert_eq!(s("NINT(A)", input), want);
        assert_eq!(a("NINT(A)", input), want);
    }
}
