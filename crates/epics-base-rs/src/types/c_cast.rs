//! C's narrowing `double -> T` conversions — the single owner of each.
//!
//! `dbConvert.c` converts a `DBF_DOUBLE` value into an integer field with a
//! bare C cast — the PUT/GET macros are literally `*pdst = (typeb) *psrc`
//! (`dbConvert.c:96-113` and the GET twin at `:63-80`), instantiated per
//! destination width:
//!
//! ```c
//! static long putDoubleChar   PUT(epicsFloat64, char)
//! static long putDoubleShort  PUT(epicsFloat64, epicsInt16)
//! static long putDoubleLong   PUT(epicsFloat64, epicsInt32)
//! static long putDoubleUlong  PUT(epicsFloat64, epicsUInt32)
//! ...                                        /* dbConvert.c:1631-1638 */
//! ```
//!
//! # The policy: C's x86-64 value, exactly
//!
//! An out-of-range `double -> integer` cast is undefined behaviour in C
//! (C17 6.3.1.4p1), and the port takes the value the shipped x86-64 softIoc
//! produces. That is a Tier 2 decision (the user's, 2026-10): bit-identical
//! stored values and snapshots against `softIoc`, so a port and a compiled IOC
//! on the same box agree on every landing. It is not the aarch64 `fcvtzs`
//! saturation and it is not Rust's `as`.
//!
//! x86-64 has two hardware conversions and every C cast shape is one of them
//! plus integer truncation. There are exactly two primitives:
//!
//! ```text
//! cvttsd2si r32   out of range or NaN -> 0x80000000 (INT_MIN)
//! cvttsd2si r64   out of range or NaN -> 0x8000000000000000 (LONG_MIN)
//! ```
//!
//! and the shapes compose them the way gcc does:
//!
//! | C cast                              | compiled as                    |
//! |-------------------------------------|--------------------------------|
//! | `(epicsInt32)`                      | `cvttsd2si r32`                |
//! | `(char)` `(epicsInt8/UInt8/Int16/UInt16/Enum16)` | `cvttsd2si r32`, low bits |
//! | `(long)` `(epicsInt64)`             | `cvttsd2si r64`                |
//! | `(epicsUInt32)`                     | `cvttsd2si r64`, low 32 bits   |
//! | `(epicsUInt64)`                     | below 2^63 `cvttsd2si r64`; from 2^63 `cvttsd2si r64 (d - 2^63) ^ 2^63`; NaN takes the first arm |
//!
//! So `3e9 -> DBF_LONG` is `-2147483648`, `1e19 -> DBF_INT64` is `i64::MIN`,
//! `1e19 -> DBF_ULONG` is `0` (the low half of `i64::MIN`), `inf -> DBF_UINT64`
//! is `0`, and `NaN -> DBF_UINT64` is `2^63`. The narrow widths store 0 for
//! every out-of-`i32` input, `INT_MIN`'s low bits.
//!
//! No alarm is raised: `dbConvert`'s routines run on the put path, outside the
//! record's own process cycle, so an alarm raised here would be erased by the
//! next `recGblResetAlarms`.
//!
//! This module is the SINGLE OWNER of the cast. Every `double -> integer`
//! conversion that models a `dbConvert` cast calls it; no such site may use a
//! bare `as`.
//!
//! # `double -> float` is the opposite case
//!
//! [`f64_to_f32`] is not part of the story above. C narrows a `double` to a
//! `DBF_FLOAT` through a named helper, `epicsConvertDoubleToFloat`, so the
//! behaviour is specified rather than undefined and the port reproduces it
//! exactly — Tier 1, because the clamped magnitude is what goes on the wire in
//! a `DBR_FLOAT` reply. It shares this module only because the ownership rule
//! is the same: a field, link, or wire conversion may not narrow with a bare
//! `as f32`.

/// x86-64 `cvttsd2si r32`: truncated toward zero, `INT_MIN` for NaN and for
/// anything outside `i32`.
#[inline]
fn cvttsd2si32(v: f64) -> i32 {
    if v > -2_147_483_649.0 && v < 2_147_483_648.0 {
        v as i32
    } else {
        i32::MIN
    }
}

/// x86-64 `cvttsd2si r64`: truncated toward zero, `i64::MIN` for NaN and for
/// anything outside `i64`.
#[inline]
fn cvttsd2si64(v: f64) -> i64 {
    if v >= -9_223_372_036_854_775_808.0 && v < 9_223_372_036_854_775_808.0 {
        v as i64
    } else {
        i64::MIN
    }
}

/// C `(epicsInt32) d`.
pub fn f64_to_i32(v: f64) -> i32 {
    cvttsd2si32(v)
}

/// C `(long) d` on LP64.
pub fn f64_to_c_long(v: f64) -> i64 {
    cvttsd2si64(v)
}

/// C `(epicsInt64) d`: `(long) d` on LP64.
pub fn f64_to_i64(v: f64) -> i64 {
    cvttsd2si64(v)
}

/// C `(char) d` / `(epicsInt8) d`: the 32-bit convert, then the low byte.
pub fn f64_to_i8(v: f64) -> i8 {
    cvttsd2si32(v) as i8
}

/// C `(epicsUInt8) d`: the 32-bit convert, then the low byte.
pub fn f64_to_u8(v: f64) -> u8 {
    cvttsd2si32(v) as u8
}

/// C `(epicsInt16) d`: the 32-bit convert, then the low 16 bits.
pub fn f64_to_i16(v: f64) -> i16 {
    cvttsd2si32(v) as i16
}

/// C `(epicsUInt16) d` / `(epicsEnum16) d`: the 32-bit convert, then the low
/// 16 bits.
pub fn f64_to_u16(v: f64) -> u16 {
    cvttsd2si32(v) as u16
}

/// C `(epicsUInt32) d`: the 64-bit convert, then the low 32 bits, so -1.0 is
/// 4294967295, 5e9 is 705032704 and 1e19 is 0.
pub fn f64_to_u32(v: f64) -> u32 {
    cvttsd2si64(v) as u32
}

/// C `(epicsUInt64) d`: below 2^63 (and NaN) the 64-bit convert reinterpreted,
/// so -1.0 is `u64::MAX`; from 2^63 the convert of `d - 2^63` with the top bit
/// set, so inf and everything from 2^64 up is 0.
pub fn f64_to_u64(v: f64) -> u64 {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if v >= TWO_63 {
        (cvttsd2si64(v - TWO_63) as u64) ^ (1 << 63)
    } else {
        cvttsd2si64(v) as u64
    }
}

/// C `epicsConvertDoubleToFloat` (`epicsConvert.c:19-34`), verbatim.
///
/// ```c
/// if (value == 0 || !finite(value))  return (float) value;
/// abs = fabs(value);
/// if (abs >= FLT_MAX)  return (value > 0) ?  FLT_MAX : -FLT_MAX;
/// if (abs <= FLT_MIN)  return (value > 0) ?  FLT_MIN : -FLT_MIN;
/// return (float) value;
/// ```
///
/// Not saturation: zero and non-finite pass straight through, so a NaN stays a
/// NaN and a genuine infinity stays an infinity — only a finite magnitude is
/// clamped. Both bounds are inclusive, and the lower one clamps to `FLT_MIN`,
/// the smallest NORMAL float, keeping the sign: a tiny non-zero double
/// (`1e-300`) reaches a `DBF_FLOAT` field as `1.17549e-38`, never as `0.0`.
///
/// Every C `double -> float` conversion goes through this helper —
/// `getDoubleFloat`/`putDoubleFloat` (`dbConvert.c:815,822,1646,1651`),
/// `cvt_d_f` (`dbFastLinkConv.c:1426`), and the `DBR_GR_FLOAT`/`DBR_CTRL_FLOAT`
/// limit fill (`db_access.c:480-485,629-636`) — which is why the port has one
/// owner here rather than a clamp at each site.
pub fn f64_to_f32(v: f64) -> f32 {
    if v == 0.0 || !v.is_finite() {
        return v as f32;
    }
    let abs = v.abs();
    if abs >= f32::MAX as f64 {
        return if v > 0.0 { f32::MAX } else { -f32::MAX };
    }
    if abs <= f32::MIN_POSITIVE as f64 {
        return if v > 0.0 {
            f32::MIN_POSITIVE
        } else {
            -f32::MIN_POSITIVE
        };
    }
    v as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is what `softIoc` R7.0.10-146-g8f5015b66 (x86-64) stored
    /// when a `DBR_DOUBLE` of that value was put to a waveform of each FTVL
    /// (`dbConvert.c` `putDouble{Char,Uchar,Short,Ushort,Long,Ulong,Int64,
    /// Uint64}`), in the order i8 u8 i16 u16 i32 u32 i64 u64. The
    /// boundaries — +-2^31, +-2^63, 2^64, inf, -inf, NaN, -0.0 and 0.5 — are
    /// among them.
    #[test]
    fn every_cast_matches_the_measured_softioc_value() {
        #[allow(clippy::type_complexity)]
        let rows: &[(f64, [i128; 8])] = &[
            (0.5, [0, 0, 0, 0, 0, 0, 0, 0]),
            (-0.5, [0, 0, 0, 0, 0, 0, 0, 0]),
            (1.5, [1, 1, 1, 1, 1, 1, 1, 1]),
            (
                -1.5,
                [-1, 255, -1, 65535, -1, 4294967295, -1, 18446744073709551615],
            ),
            (-0.0, [0, 0, 0, 0, 0, 0, 0, 0]),
            (2.7, [2, 2, 2, 2, 2, 2, 2, 2]),
            (
                -2.7,
                [-2, 254, -2, 65534, -2, 4294967294, -2, 18446744073709551614],
            ),
            (127.9, [127, 127, 127, 127, 127, 127, 127, 127]),
            (128.0, [-128, 128, 128, 128, 128, 128, 128, 128]),
            (255.9, [-1, 255, 255, 255, 255, 255, 255, 255]),
            (256.0, [0, 0, 256, 256, 256, 256, 256, 256]),
            (300.0, [44, 44, 300, 300, 300, 300, 300, 300]),
            (32767.5, [-1, 255, 32767, 32767, 32767, 32767, 32767, 32767]),
            (32768.0, [0, 0, -32768, 32768, 32768, 32768, 32768, 32768]),
            (65535.7, [-1, 255, -1, 65535, 65535, 65535, 65535, 65535]),
            (65536.0, [0, 0, 0, 0, 65536, 65536, 65536, 65536]),
            (70000.9, [112, 112, 4464, 4464, 70000, 70000, 70000, 70000]),
            (
                -32769.0,
                [
                    -1,
                    255,
                    32767,
                    32767,
                    -32769,
                    4294934527,
                    -32769,
                    18446744073709518847,
                ],
            ),
            (
                -40000.0,
                [
                    -64,
                    192,
                    25536,
                    25536,
                    -40000,
                    4294927296,
                    -40000,
                    18446744073709511616,
                ],
            ),
            (
                2147483647.0,
                [
                    -1, 255, -1, 65535, 2147483647, 2147483647, 2147483647, 2147483647,
                ],
            ),
            (
                2147483647.5,
                [
                    -1, 255, -1, 65535, 2147483647, 2147483647, 2147483647, 2147483647,
                ],
            ),
            (
                2147483648.0,
                [0, 0, 0, 0, -2147483648, 2147483648, 2147483648, 2147483648],
            ),
            (
                -2147483648.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    2147483648,
                    -2147483648,
                    18446744071562067968,
                ],
            ),
            (
                -2147483648.5,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    2147483648,
                    -2147483648,
                    18446744071562067968,
                ],
            ),
            (
                -2147483649.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    2147483647,
                    -2147483649,
                    18446744071562067967,
                ],
            ),
            (
                3e9,
                [0, 0, 0, 0, -2147483648, 3000000000, 3000000000, 3000000000],
            ),
            (
                -3e9,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    1294967296,
                    -3000000000,
                    18446744070709551616,
                ],
            ),
            (
                4294967295.5,
                [0, 0, 0, 0, -2147483648, 4294967295, 4294967295, 4294967295],
            ),
            (
                4294967296.0,
                [0, 0, 0, 0, -2147483648, 0, 4294967296, 4294967296],
            ),
            (
                5e9,
                [0, 0, 0, 0, -2147483648, 705032704, 5000000000, 5000000000],
            ),
            (
                -5e9,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    3589934592,
                    -5000000000,
                    18446744068709551616,
                ],
            ),
            (
                1e10,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    1410065408,
                    10000000000,
                    10000000000,
                ],
            ),
            (
                9.2e18,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    2643984384,
                    9200000000000000000,
                    9200000000000000000,
                ],
            ),
            (
                9223372036854775807.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                9223372036854775808.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                -9223372036854775808.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                -9.3e18,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                1e19,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    10000000000000000000,
                ],
            ),
            (
                1.8446744073709552e19,
                [0, 0, 0, 0, -2147483648, 0, -9223372036854775808, 0],
            ),
            (
                1.9e19,
                [0, 0, 0, 0, -2147483648, 0, -9223372036854775808, 0],
            ),
            (1e20, [0, 0, 0, 0, -2147483648, 0, -9223372036854775808, 0]),
            (
                -1e19,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (1e300, [0, 0, 0, 0, -2147483648, 0, -9223372036854775808, 0]),
            (
                f64::INFINITY,
                [0, 0, 0, 0, -2147483648, 0, -9223372036854775808, 0],
            ),
            (
                f64::NEG_INFINITY,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                f64::NAN,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                9223372036854774784.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    4294966272,
                    9223372036854774784,
                    9223372036854774784,
                ],
            ),
            (
                -2147483648.9,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    2147483648,
                    -2147483648,
                    18446744071562067968,
                ],
            ),
            (
                18446744073709549568.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    18446744073709549568,
                ],
            ),
            (
                -9223372036854775809.0,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                -9.223372036854777e18,
                [
                    0,
                    0,
                    0,
                    0,
                    -2147483648,
                    0,
                    -9223372036854775808,
                    9223372036854775808,
                ],
            ),
            (
                2147483647.9999,
                [
                    -1, 255, -1, 65535, 2147483647, 2147483647, 2147483647, 2147483647,
                ],
            ),
        ];
        for &(d, [i8v, u8v, i16v, u16v, i32v, u32v, i64v, u64v]) in rows {
            assert_eq!(f64_to_i8(d) as i128, i8v, "(char){d:e}");
            assert_eq!(f64_to_u8(d) as i128, u8v, "(epicsUInt8){d:e}");
            assert_eq!(f64_to_i16(d) as i128, i16v, "(epicsInt16){d:e}");
            assert_eq!(f64_to_u16(d) as i128, u16v, "(epicsUInt16){d:e}");
            assert_eq!(f64_to_i32(d) as i128, i32v, "(epicsInt32){d:e}");
            assert_eq!(f64_to_u32(d) as i128, u32v, "(epicsUInt32){d:e}");
            assert_eq!(f64_to_i64(d) as i128, i64v, "(epicsInt64){d:e}");
            assert_eq!(f64_to_c_long(d) as i128, i64v, "(long){d:e}");
            assert_eq!(f64_to_u64(d) as i128, u64v, "(epicsUInt64){d:e}");
        }
    }

    /// The two hardware primitives at their edges: the last representable
    /// value inside, and the first outside, and the sign of zero.
    #[test]
    fn the_primitives_fail_to_their_indefinite_value_at_the_edges() {
        assert_eq!(cvttsd2si32(2_147_483_647.9), i32::MAX);
        assert_eq!(cvttsd2si32(2_147_483_648.0), i32::MIN);
        assert_eq!(cvttsd2si32(-2_147_483_648.9), i32::MIN);
        assert_eq!(cvttsd2si32(-2_147_483_649.0), i32::MIN);
        assert_eq!(cvttsd2si32(f64::NAN), i32::MIN);
        assert_eq!(
            cvttsd2si64(9_223_372_036_854_774_784.0),
            9_223_372_036_854_774_784
        );
        assert_eq!(cvttsd2si64(9_223_372_036_854_775_808.0), i64::MIN);
        assert_eq!(cvttsd2si64(-9_223_372_036_854_775_808.0), i64::MIN);
        assert_eq!(cvttsd2si64(f64::INFINITY), i64::MIN);
        assert_eq!(cvttsd2si64(f64::NEG_INFINITY), i64::MIN);
        assert_eq!(cvttsd2si64(f64::NAN), i64::MIN);
        assert_eq!(cvttsd2si64(-0.0), 0);
        // Toward zero, not rounding: 0.5 and 1.5 and their negatives.
        assert_eq!(cvttsd2si32(0.5), 0);
        assert_eq!(cvttsd2si32(1.5), 1);
        assert_eq!(cvttsd2si32(-0.5), 0);
        assert_eq!(cvttsd2si32(-1.5), -1);
    }
}
