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
/// IOC actually does is whatever its ISA's convert instruction does. This ports
/// **x86-64** (`cvttsd2si` with a 32-bit destination), the reference platform:
/// the result is the "integer indefinite" value `INT32_MIN`. On aarch64 `fcvtzs`
/// saturates instead, so a C IOC there answers `INT32_MAX`; both differ from
/// [`d2i`]'s wrap, which is the bug this owner exists to prevent.
#[inline]
pub(crate) fn c_int(x: f64) -> i32 {
    let t = x.trunc();
    if t.is_nan() || t < i32::MIN as f64 || t > i32::MAX as f64 {
        i32::MIN
    } else {
        t as i32
    }
}

/// C's plain `(long)` cast of a double on LP64 (what sCalc's operators use).
/// Same story as [`c_int`], one width up: x86-64 `cvttsd2si` with a 64-bit
/// destination yields `INT64_MIN` for NaN and for anything out of range.
#[inline]
pub(crate) fn c_long(x: f64) -> i64 {
    let t = x.trunc();
    if t.is_nan() || t < i64::MIN as f64 || t >= 9223372036854775808.0 {
        i64::MIN
    } else {
        t as i64
    }
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
