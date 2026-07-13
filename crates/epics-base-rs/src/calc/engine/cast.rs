//! The double→integer casts the three calc dialects perform, in one place.
//!
//! Every integer operation in an EPICS calc engine starts by casting a `double`
//! stack cell to an integer, and the three dialects do **not** use the same
//! cast. Keeping one module as the owner is what stops a cast from being copied
//! between dialects, which is how the port ended up running base's `d2i` inside
//! sCalc and aCalc, neither of which has that macro at all:
//!
//! | dialect | bit / shift ops | MODULO |
//! |---|---|---|
//! | base `calcPerform.c` | [`d2i`] / [`d2ui`] (:324-325, :329-366) | plain `(epicsInt32)` (:162-164) |
//! | sCalc `sCalcPerform.c` | plain `(long)` (:578-631, :725) | `(int)` no-string path (:562), `(long)` string path (:1109) |
//! | aCalc `aCalcPerform.c` | plain `(int)` (:907, :1355-1357, :1424-1427) | plain `(int)` (:650, :674, :701) |
//!
//! [`d2i`]/[`d2ui`] are *not* general truncating casts: they route a
//! non-negative double through `epicsUInt32` first, so `3e9` becomes
//! `-1294967296` (bit pattern preserved) instead of overflowing. That
//! reinterpretation is exactly what base wants for a bitwise operand and
//! exactly what C's plain cast does *not* do.

/// C `d2i` (`calcPerform.c:324`):
/// `((x)<0 ? (epicsInt32)(x) : (epicsInt32)(epicsUInt32)(x))`.
///
/// A non-negative double goes through `epicsUInt32`, so the full 32-bit
/// pattern survives and bit 31 lands as the sign bit — `d2i(3e9)` is
/// `-1294967296`, not an overflow. Base uses this for BIT_OR/AND/XOR/NOT and
/// the shifts, and for nothing else.
#[inline]
pub(crate) fn d2i(x: f64) -> i32 {
    if x < 0.0 {
        f64_to_i32_wrap(x)
    } else {
        f64_to_u32_wrap(x) as i32
    }
}

/// C `d2ui` (`calcPerform.c:325`):
/// `((x)<0 ? (epicsUInt32)(epicsInt32)(x) : (epicsUInt32)(x))`.
/// Base's logical right shift (`>>>`, RIGHT_SHIFT_LOGIC) is its only user.
#[inline]
pub(crate) fn d2ui(x: f64) -> u32 {
    if x < 0.0 {
        f64_to_i32_wrap(x) as u32
    } else {
        f64_to_u32_wrap(x)
    }
}

/// C's plain `(int)` / `(epicsInt32)` cast of a double.
///
/// In range it truncates toward zero. Out of range (and for NaN/±Inf) the C
/// standard calls it undefined, so there is no portable answer — what an EPICS
/// IOC actually does is whatever its ISA's convert instruction does. On
/// **x86-64**, the reference platform, that is `cvttsd2si` with a 32-bit
/// destination: the "integer indefinite" value `INT32_MIN`. (On aarch64
/// `fcvtzs` saturates instead, so a C IOC there answers `INT32_MAX`.) Either
/// way it differs from [`d2i`]'s wrap, which is the bug this module's split
/// exists to prevent.
///
/// The cast itself is not calc's — it is the same bare C cast `dbConvert.c`
/// performs on a `DBF_DOUBLE` → `DBF_LONG` put — so it is owned by
/// [`crate::types::c_cast`] and merely named for calc here.
#[inline]
pub(crate) fn c_int(x: f64) -> i32 {
    crate::types::c_cast::f64_to_i32(x)
}

/// C `myNINT` — `sCalcPerform.c:40` and `aCalcPerform.c:50`, byte-identical:
///
/// ```c
/// #define myNINT(a) ((int)((a) >= 0 ? (a)+0.5 : (a)-0.5))
/// ```
///
/// Round half away from zero, **and then cast to `int`** — the narrowing is
/// INSIDE the macro, which is the whole point of it living here. A caller that
/// takes `myNINT`'s value into a `long` gets an already-narrowed `int`
/// sign-extended, not a fresh 64-bit conversion of the double.
///
/// The port used to have two copies of this and neither narrowed like C: sCalc's
/// returned a `double` (so each of its call sites invented its own narrowing —
/// `as i64` wrapped, `as i32` saturated, and the two disagreed *bitwise* on the
/// same input), and aCalc's used Rust's `as i32`, which saturates where C's
/// `cvttsd2si` yields the indefinite value. One function, one narrowing.
///
/// Compiled (`gcc -O0`, x86-64, runtime operand — a *constant* operand is folded
/// by gcc to `INT32_MAX` instead, which is why this must be probed at runtime):
/// `myNINT(3e9)` = `myNINT(-3e9)` = `myNINT(1e18)` = `myNINT(NaN)` =
/// `-2147483648`; `myNINT(2.5)` = 3; `myNINT(-2.5)` = -3.
#[inline]
pub(crate) fn my_nint(a: f64) -> i32 {
    c_int(if a >= 0.0 { a + 0.5 } else { a - 0.5 })
}

/// C's plain `(long)` cast of a double on LP64 (what sCalc's operators use).
/// Same story as [`c_int`], one width up: x86-64 `cvttsd2si` with a 64-bit
/// destination yields `INT64_MIN` for NaN and for anything out of range.
#[inline]
pub(crate) fn c_long(x: f64) -> i64 {
    crate::types::c_cast::f64_to_i64(x)
}

/// `NINT` — round half away from zero, and **do not narrow**.
///
/// **DEVIATION from C, deliberate — CBUG-A2.** All three engines narrow the
/// rounded double with a bare cast before pushing it back onto a `double` stack:
/// base `(epicsInt32)(top >= 0 ? top+0.5 : top-0.5)` (`calcPerform.c:290-293`),
/// sCalc `(long)` (`sCalcPerform.c:716-719`), aCalc `(long)` (`aCalcPerform.c:827`).
/// Out of the destination's range that cast is undefined; on x86-64 it yields
/// `cvttsd2si`'s indefinite value, so C answers `NINT(3e9) = -2147483648` and
/// `NINT(NaN) = -2147483648`.
///
/// The narrowing serves no purpose here — the operand and the result are both
/// `double`, the documented contract is "nearest integer"
/// (`calcRecord.dbd.pod`), and C's own `d2i` comment (`:313-322`) says
/// out-of-range double→int conversions "give very different results on different
/// systems", which is why every sibling bitwise op was routed through a guard
/// that NINT and MODULO never got. We round and stop: `NINT(3e9)` is `3e9`,
/// `NINT(NaN)` is `NaN`, `NINT(inf)` is `inf`. Inside `[i32::MIN, i32::MAX]` this
/// is bit-identical to C, which is every value a calc record actually rounds.
///
/// C's `(x >= 0 ? x+0.5 : x-0.5)` then truncate-toward-zero IS the rounding rule;
/// only the width is dropped.
#[inline]
pub(crate) fn nint(x: f64) -> f64 {
    if x.is_nan() {
        x
    } else if x >= 0.0 {
        (x + 0.5).floor()
    } else {
        (x - 0.5).ceil()
    }
}

/// `MODULO` — truncate both operands toward zero, then take the remainder,
/// **without narrowing to an integer type**.
///
/// **DEVIATION from C, deliberate — CBUG-A2.** C narrows the dividend and the
/// divisor with a bare cast first (base `(epicsInt32)`, `calcPerform.c:162-164`;
/// sCalc `(int)`/`(long)`; aCalc `(int)`), so `3e9 % 7` is `INT_MIN % 7 = -2`
/// instead of `4`, and the answer changes with the CPU. `f64`'s remainder is the
/// same truncated remainder C's integer `%` computes, exactly, for every operand
/// pair inside the cast's range — and it stays correct outside it.
///
/// This also removes the UB dividend that made CBUG-A1 (`INT_MIN % -1` SIGFPE)
/// reachable at all.
///
/// The caller keeps its own engine's zero-divisor rule (base NaN, sCalc error,
/// aCalc `myMAXFLOAT`) — that part of C is not a bug. The divisor is zero
/// exactly when it truncates to zero, as in C.
#[inline]
pub(crate) fn imod(a: f64, b: f64) -> f64 {
    a.trunc() % b.trunc()
}

/// `(epicsInt32)x` where the value is already known to be in `epicsUInt32`
/// range — the tail of `d2i`/`d2ui`, i.e. a modular reduction, NOT a C cast.
/// Private on purpose: an operator that wants a C cast wants [`c_int`].
#[inline]
fn f64_to_i32_wrap(x: f64) -> i32 {
    if x.is_nan() {
        return 0;
    }
    let m = x.trunc().rem_euclid(4294967296.0);
    m as u64 as u32 as i32
}

#[inline]
fn f64_to_u32_wrap(x: f64) -> u32 {
    if x.is_nan() {
        return 0;
    }
    let m = x.trunc().rem_euclid(4294967296.0);
    m as u64 as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the split: on the values where they disagree, `d2i`
    /// and a C cast are different numbers. They agree everywhere inside
    /// `[i32::MIN, i32::MAX]`, which is why the bug hid — `A % 7` only
    /// diverges once A climbs past 2^31.
    #[test]
    fn d2i_and_c_int_agree_in_range_and_diverge_outside_it() {
        for x in [0.0, 1.0, -1.0, 7.9, -7.9, 2147483647.0, -2147483648.0] {
            assert_eq!(d2i(x), c_int(x), "for {x}");
        }
        // 3e9 fits in epicsUInt32, so d2i reinterprets it as a negative int32.
        assert_eq!(d2i(3e9), -1_294_967_296);
        // The plain cast has no such route: 3e9 > INT32_MAX is out of range.
        assert_eq!(c_int(3e9), i32::MIN);
        assert_eq!(c_long(3e9), 3_000_000_000);
    }

    /// Compiled sCalc/aCalc (`gcc -O0`, x86-64, RUNTIME operand). The narrowing
    /// is inside C's macro, so an out-of-range magnitude is `INT32_MIN` — the
    /// same value for either sign — and not a saturation or a wrap.
    #[test]
    fn my_nint_rounds_then_casts_at_c_width() {
        assert_eq!(my_nint(2.5), 3);
        assert_eq!(my_nint(-2.5), -3);
        assert_eq!(my_nint(2.4), 2);
        assert_eq!(my_nint(-2.4), -2);
        assert_eq!(my_nint(0.0), 0);
        // Out of int32 range, either sign, and NaN: `cvttsd2si`'s indefinite value.
        assert_eq!(my_nint(3e9), i32::MIN);
        assert_eq!(my_nint(-3e9), i32::MIN);
        assert_eq!(my_nint(1e18), i32::MIN);
        assert_eq!(my_nint(f64::NAN), i32::MIN);
        // The rounding happens BEFORE the cast, so INT32_MAX still fits.
        assert_eq!(my_nint(2147483647.0), i32::MAX);
        // Not d2i: the wrap that base's bitwise ops use would say -1294967296.
        assert_ne!(my_nint(3e9), d2i(3e9));
    }

    #[test]
    fn d2i_and_d2ui_are_the_base_macros() {
        assert_eq!(d2ui(3e9), 3_000_000_000);
        assert_eq!(d2i(-1.0), -1);
        assert_eq!(d2ui(-1.0), 0xFFFF_FFFF);
    }

    /// x86-64 `cvttsd2si` answers with the indefinite value for NaN and ±Inf.
    #[test]
    fn c_casts_are_x86_64_cvttsd2si() {
        assert_eq!(c_int(f64::NAN), i32::MIN);
        assert_eq!(c_int(f64::INFINITY), i32::MIN);
        assert_eq!(c_int(f64::NEG_INFINITY), i32::MIN);
        assert_eq!(c_long(f64::NAN), i64::MIN);
        assert_eq!(c_long(1e300), i64::MIN);
        // The 2^63 boundary: representable as f64 and NOT representable as i64.
        assert_eq!(c_long(9223372036854775808.0), i64::MIN);
        assert_eq!(c_long(-9223372036854775808.0), i64::MIN);
    }
}
