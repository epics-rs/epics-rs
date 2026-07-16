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
//! **The BITWISE half of that table is still C's and is pinned here. The MODULO
//! column is NOT — CBUG-A2.** C's MODULO (and NINT) narrows its operands with a
//! bare cast that is undefined out of range, so `3e9 % 7` is `-2` on a C IOC
//! (`INT_MIN % 7`) and would be something else on another CPU. The port
//! truncates the operands and takes the remainder without narrowing, so `3e9 %
//! 7` is `4` — the true remainder — in all three dialects, and NINT rounds
//! without narrowing. The C answers are named beside each case below.
//!
//! Every expected C value below was verified by compiling the same C expression
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

/// CBUG-A2 — `A % 7` with `A = 3e9` is the TRUE remainder, `4`, in every
/// dialect. This test used to pin C's answers, which were:
/// * base — `(epicsInt32)3e9` overflows → x86-64 gives INT32_MIN → `-2`
/// * sCalc — no string op, so C's no-string evaluator casts `(int)` → `-2`
///   (its string evaluator casts `(long)` and answers `4`, on the same input)
/// * aCalc — `(int)` → `-2`
///
/// The port truncates the operands and takes the f64 remainder, which IS C's
/// truncated integer remainder for every operand pair the cast could represent,
/// and stays correct past it.
#[test]
fn modulo_of_a_value_above_2_31_is_the_true_remainder() {
    assert_eq!(n("A%7", 3e9), 4.0); // C: -2
    assert_eq!(s("A%7", 3e9), 4.0); // C: -2
    assert_eq!(a("A%7", 3e9), 4.0); // C: -2
}

/// CBUG-A2, the second face of it. sCalc's C picks its evaluator from the
/// compiled `USES_STRING` marker (`sCalcPostfix.c:447-475`,
/// `sCalcPerform.c:399`) and the two evaluators cast MODULO at different widths
/// — `(int)` vs `(long)` — so in C the SAME arithmetic answers differently
/// depending on whether an unrelated string opcode appears anywhere in the
/// expression (`A%7` → `-2`, `A%7+LEN('')` → `4`).
///
/// The port narrows neither, so the marker no longer perturbs the arithmetic:
/// both are `4`. This test used to pin the split.
#[test]
fn scalc_modulo_no_longer_depends_on_the_uses_string_marker() {
    assert_eq!(s("A%7", 3e9), 4.0, "C's no-string (int) evaluator says -2");
    // LEN("") is in C's USES_STRING opcode list, so the whole program switches
    // to C's string evaluator; the arithmetic is otherwise identical.
    assert_eq!(
        s("A%7+LEN('')", 3e9),
        4.0,
        "C's string (long) evaluator agrees"
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

/// A denominator larger than the cast's range is NOT zero, and never was: every
/// candidate rule for the out-of-range cast — C-on-x86's INT32_MIN, CBUG-E2's
/// saturating INT32_MAX, or not narrowing at all — leaves a divisor whose
/// magnitude exceeds 5, so `5 % divisor == 5` in all of them. Neither CBUG-A2
/// nor CBUG-E2 moves this. The case is kept because pinning NaN here (the
/// pre-R8-5 wrap model, where 2^32 truncated to 0) would be wrong under every
/// one of those rules.
#[test]
fn a_denominator_past_the_cast_range_is_not_a_zero_divisor() {
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
/// UB on an out-of-range double, so compiled C answers INT32_MIN on x86-64 and
/// INT32_MAX on aarch64. The port saturates.
#[test]
fn acalc_bitwise_is_a_plain_int_cast() {
    assert_ne!(a("A|0", 3e9), -1_294_967_296.0);
    assert_eq!(a("A|0", 3e9), i32::MAX as f64);
}

/// CBUG-A2 — NINT rounds and does not narrow, in every dialect. C narrows at
/// each dialect's own width — base `(epicsInt32)` (`calcPerform.c:292`),
/// sCalc/aCalc `(long)` (`sCalcPerform.c:718`, `aCalcPerform.c:1085`) — so on a
/// C IOC `NINT(3e9)` is `-2147483648` in base and `3e9` in the other two, purely
/// because of the cast width. This test used to pin that split.
#[test]
fn nint_does_not_narrow_in_any_dialect() {
    assert_eq!(n("NINT(A)", 3e9), 3e9); // C base: i32::MIN
    assert_eq!(s("NINT(A)", 3e9), 3e9);
    assert_eq!(a("NINT(A)", 3e9), 3e9);

    // Past int64 too, where all three C dialects give their indefinite value.
    assert_eq!(n("NINT(A)", 1e300), 1e300);
    assert_eq!(s("NINT(A)", 1e300), 1e300);
    assert_eq!(a("NINT(A)", 1e300), 1e300);

    // In range, all three agree and round half away from zero — same as C.
    for expr_val in [(2.5, 3.0), (-2.5, -3.0), (2.4, 2.0)] {
        let (input, want) = expr_val;
        assert_eq!(n("NINT(A)", input), want);
        assert_eq!(s("NINT(A)", input), want);
        assert_eq!(a("NINT(A)", input), want);
    }
}
