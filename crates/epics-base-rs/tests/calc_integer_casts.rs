//! R8-5 — every calc dialect's integer operators cast their `double` operands
//! the way THAT dialect's C does, and the three do not agree.
//!
//! | dialect | bit / shift | MODULO | NINT |
//! |---|---|---|---|
//! | base `calcPerform.c` | `d2i`/`d2ui` (:324-325 at R7.0.10) | `d2i` (:176-190 at unmerged PR #925) — NaN on zero | `d2i` (:313-317, same PR) |
//! | sCalc `sCalcPerform.c` | plain `(long)` (:578-631) | `(long)` (:1109) — error on zero | `(long)` (:718) |
//! | aCalc `aCalcPerform.c` | plain `(int)` (:1355-1357) | `(int)` (:650,:674,:701) — myMAXFLOAT on zero | `(long)` (:828,:1085) |
//!
//! **CBUG-A2 — each engine narrows MODULO and NINT with its OWN dialect's cast**,
//! mirroring that engine's pristine/fixed C. They genuinely disagree because the
//! C widths do: `3e9` is `d2i(3e9) = -1294967296` in base, `(long)3e9 = 3e9` in
//! sCalc, and (for MODULO) `(int)3e9` out-of-range in aCalc. So `3e9 % 7` is `0`
//! (base), `4` (sCalc), `1` (aCalc — its `(int)` saturates to i32::MAX under
//! CBUG-E2, then `% 7`). The structure is a single `cast::nint`/`cast::imod`
//! parameterised by the narrowing fn — one logic, per-engine narrowing.
//!
//! Note aCalc is internally inconsistent: NINT is `(long)` (64-bit) but MODULO
//! is `(int)` (32-bit) — verified in aCalcPerform.c. base is the only engine on
//! `d2i`, because PR #925 is a base-only commit; sCalc/aCalc keep their pristine
//! synApps `(long)`/`(int)`, which the differential oracle also runs.
//!
//! d2i and the plain casts alike stay UNDEFINED in C outside their range
//! (negatives, `>= 2^32`, NaN); on those the port mirrors its own cast owner
//! (base d2i's modular wrap, sCalc/aCalc's saturating `c_long`/`c_int`), never
//! reproducing x86 `cvttsd2si`. Those values carry no claim of C-correctness.
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
/// * base — `d2i(3e9) = -1294967296`, `% 7 == 0` (fixed `calcPerform.c`, PR #925)
/// * sCalc — `(long)3e9 = 3000000000`, `% 7 == 4` (64-bit, no loss)
/// * aCalc — `(int)3e9` is out of int32 range, so the port's saturating cast
///   (CBUG-E2) gives `i32::MAX = 2147483647`, `% 7 == 1`
///
/// Each equals its own pristine/fixed C. This used to pin the clean no-narrow
/// deviation (`4` everywhere).
#[test]
fn modulo_above_2_31_mirrors_each_dialect_width() {
    assert_eq!(n("A%7", 3e9), 0.0); // base d2i
    assert_eq!(s("A%7", 3e9), 4.0); // sCalc (long)
    assert_eq!(a("A%7", 3e9), 1.0); // aCalc (int), saturating (CBUG-E2)
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

/// In the BASE (numeric) engine a divisor of exactly 2^32 is the UB boundary:
/// `d2i` is defined only for `[2^31, 2^32)`, and `2^32` is one past it. The
/// port's `d2i` wraps modulo 2^32, so `d2i(2^32) = 0` — a zero divisor — and
/// base's rule makes it NaN. This is NOT a C-defined value; it is the port
/// mirroring its own d2i on a still-UB input. sCalc's `(long)` and aCalc's
/// `(int)` do NOT wrap here — this boundary is base-specific. Under the earlier
/// clean no-narrow deviation this answered `5`.
#[test]
fn base_divisor_at_the_2_32_boundary_narrows_to_zero() {
    assert!(nc("5 % 4294967296").is_nan());
    assert!(n("A % 4294967296", 5.0).is_nan());
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
/// UB on an out-of-range double, so compiled C answers INT32_MIN on x86-64 and
/// INT32_MAX on aarch64. The port saturates.
#[test]
fn acalc_bitwise_is_a_plain_int_cast() {
    assert_ne!(a("A|0", 3e9), -1_294_967_296.0);
    assert_eq!(a("A|0", 3e9), i32::MAX as f64);
}

/// CBUG-A2 — NINT rounds, then narrows with EACH dialect's own cast:
/// * base — `d2i` (fixed `calcPerform.c:316`, PR #925): `NINT(3e9) = -1294967296`
/// * sCalc — `(long)` (`sCalcPerform.c:730`): `NINT(3e9) = 3e9` (64-bit, no loss)
/// * aCalc — `(long)` (`aCalcPerform.c:839,1096`): `NINT(3e9) = 3e9` — aCalc
///   narrows NINT at 64-bit even though its MODULO is 32-bit `(int)` (an internal
///   synApps inconsistency)
///
/// This used to pin the clean no-narrow deviation (`3e9` in every dialect); note
/// sCalc/aCalc happen to still be `3e9` because `(long)` does not lose 3e9.
#[test]
fn nint_mirrors_each_dialect_width() {
    assert_eq!(n("NINT(A)", 3e9), -1_294_967_296.0); // base d2i
    assert_eq!(s("NINT(A)", 3e9), 3e9); // sCalc (long)
    assert_eq!(a("NINT(A)", 3e9), 3e9); // aCalc (long)

    // Past each width's range: base d2i wraps 1e300 (a multiple of 2^32) to 0;
    // sCalc/aCalc `(long)` saturate to i64::MAX (CBUG-E2). Not C-defined values.
    assert_eq!(n("NINT(A)", 1e300), 0.0);
    assert_eq!(s("NINT(A)", 1e300), i64::MAX as f64);
    assert_eq!(a("NINT(A)", 1e300), i64::MAX as f64);

    // In range, all three agree and round half away from zero — same as C.
    for expr_val in [(2.5, 3.0), (-2.5, -3.0), (2.4, 2.0)] {
        let (input, want) = expr_val;
        assert_eq!(n("NINT(A)", input), want);
        assert_eq!(s("NINT(A)", input), want);
        assert_eq!(a("NINT(A)", input), want);
    }
}
