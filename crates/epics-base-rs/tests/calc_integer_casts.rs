//! R8-5 — every calc dialect's integer operators cast their `double` operands
//! the way THAT dialect's C does, and the three do not agree.
//!
//! | dialect | bit / shift | MODULO |
//! |---|---|---|
//! | base `calcPerform.c` | `d2i`/`d2ui` macros (:324-325) | plain `(epicsInt32)` (:162-164), NaN on a zero divisor |
//! | sCalc `sCalcPerform.c` | plain `(long)` (:578-631) | `(int)` no-string (:562) / `(long)` string (:1109), `return -1` on a zero divisor |
//! | aCalc `aCalcPerform.c` | plain `(int)` (:1355-1357) | plain `(int)` (:650), `myMAXFLOAT` on a zero divisor |
//!
//! The port ran base's `d2i` in all three, so every operand ≥ 2^31 came out
//! wrong in every engine: `d2i` reinterprets a non-negative double through
//! `epicsUInt32` (3e9 → -1294967296), which is what base wants for a BITWISE
//! operand and is not a C cast at all.
//!
//! Every expected value below was verified by compiling the same C expression
//! with gcc -O2 on x86-64, the platform an EPICS IOC is built for:
//!
//! ```text
//! (epicsInt32)3e9 % 7  = -2      (long)3e9 % 7 = 4      (epicsInt32)3e9 = -2147483648
//! (epicsInt32)4294967296.0 = -2147483648    5 % that = 5
//! ```

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

/// The finding's fingerprint. `A % 7` with `A = 3e9`:
/// * base — `(epicsInt32)3e9` is out of range → x86-64 gives INT32_MIN → `-2`
/// * sCalc — no string op in the expression, so C's no-string evaluator runs
///   and casts with `(int)` → also `-2`
/// * aCalc — `(int)` → `-2`
///
/// Pre-fix all three answered `0`, because `d2i(3e9) = -1294967296` happens to
/// be an exact multiple of 7.
#[test]
fn modulo_of_a_value_above_2_31() {
    assert_eq!(n("A%7", 3e9), -2.0);
    assert_eq!(s("A%7", 3e9), -2.0);
    assert_eq!(a("A%7", 3e9), -2.0);
}

/// sCalc's C picks its evaluator from the compiled `USES_STRING` marker
/// (`sCalcPostfix.c:447-475`, `sCalcPerform.c:399`), and the two evaluators
/// cast MODULO differently: `(int)` vs `(long)`. Adding a string op to the
/// SAME arithmetic therefore changes the answer — `(long)3e9 % 7 == 4`, since
/// 3e9 fits in a long and needs no out-of-range cast at all.
#[test]
fn scalc_modulo_width_follows_the_uses_string_marker() {
    assert_eq!(s("A%7", 3e9), -2.0, "no string opcode → (int) path");
    // LEN("") is in C's USES_STRING opcode list, so the whole program switches
    // to the string evaluator; the arithmetic is otherwise identical.
    assert_eq!(s("A%7+LEN('')", 3e9), 4.0, "string opcode → (long) path");
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

/// A denominator that overflows the cast is NOT zero: C's cast yields
/// INT32_MIN, so the modulo branch runs. Pinning the old behaviour (NaN) meant
/// pinning the wrap model.
#[test]
fn out_of_range_denominator_is_int32_min_not_zero() {
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

/// aCalc's bitwise ops are `(int)`, so an out-of-range operand becomes
/// INT32_MIN — not `d2i`'s -1294967296.
#[test]
fn acalc_bitwise_is_a_plain_int_cast() {
    assert_eq!(a("A|0", 3e9), i32::MIN as f64);
}

/// NINT is a plain cast in every dialect, at each dialect's width:
/// base `(epicsInt32)` (calcPerform.c:292), sCalc/aCalc `(long)`
/// (sCalcPerform.c:718, aCalcPerform.c:1085).
#[test]
fn nint_casts_at_each_dialects_width() {
    assert_eq!(n("NINT(A)", 3e9), i32::MIN as f64);
    assert_eq!(s("NINT(A)", 3e9), 3e9);
    assert_eq!(a("NINT(A)", 3e9), 3e9);

    // In range, all three agree and round half away from zero.
    for expr_val in [(2.5, 3.0), (-2.5, -3.0), (2.4, 2.0)] {
        let (input, want) = expr_val;
        assert_eq!(n("NINT(A)", input), want);
        assert_eq!(s("NINT(A)", input), want);
        assert_eq!(a("NINT(A)", input), want);
    }
}
