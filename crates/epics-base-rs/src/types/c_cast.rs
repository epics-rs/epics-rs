//! C's `double -> integer` cast, as the compiler actually emits it on x86-64.
//!
//! `dbConvert.c` converts a `DBF_DOUBLE` value into an integer field with a
//! bare C cast — the PUT/GET macros are literally `*pdst = (typeb) *psrc`
//! (`dbConvert.c:96-113` and the GET twin at `:63-70`), instantiated per
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
//! An out-of-range cast is undefined behaviour by the C standard, so there is
//! no portable answer — but **the compiled IOC is the parity target**, the same
//! precedent the port already follows for HexSignificand and for the shift-mask
//! UB. Rust's `as` is *not* that behaviour: it saturates
//! (`3.0e9 as i32 == 2147483647`), while the compiled C yields `INT_MIN`. A
//! client writing 3.0e9 to a `longout` gets `-2147483648` on a C IOC and
//! `2147483647` on the pre-fix port; an `aao` DOL of `[1.7, 2.2, -3.9, 70000,
//! 5, 6]` into an `FTVL=SHORT` waveform gives C `[1, 2, -3, 4464]`.
//!
//! This module is the SINGLE OWNER of that cast. Every `double -> integer`
//! conversion that models a `dbConvert` cast calls it; no such site may use a
//! bare `as`.
//!
//! # The model (x86-64 SysV, gcc/clang, verified against compiled output)
//!
//! - **32-bit and narrower signed/unsigned dests** (`i8`/`u8`/`i16`/`u16`/
//!   `i32`/`u32`... except `u32`, see below): the compiler emits a *32-bit*
//!   `cvttsd2si`, then truncates the `i32` to the destination width. A value
//!   whose truncation does not fit in `i32` — and NaN — gives the "integer
//!   indefinite" `INT_MIN` (`0x80000000`), which then truncates to 0 in the
//!   narrower widths. Hence `70000.9 -> short 4464` (70000 & 0xFFFF) but
//!   `3.0e9 -> short 0` (INT_MIN & 0xFFFF).
//! - **`u32`**: the compiler emits a *64-bit* `cvttsd2si` and keeps the low 32
//!   bits, so `3.0e9 -> 3000000000` (in range for the 64-bit convert) while
//!   `1e19 -> 0` (INT64_MIN's low half).
//! - **`i64`**: 64-bit `cvttsd2si`; out of range or NaN gives `INT64_MIN`.
//! - **`u64`**: the compiler's branch sequence — below 2^63 a plain 64-bit
//!   `cvttsd2si`, at or above it `cvttsd2si(d - 2^63)` with bit 63 flipped
//!   back. NaN fails the `>= 2^63` test (every NaN comparison is false) and so
//!   takes the low branch, yielding 2^63.
//!
//! Every value in the table below was produced by compiling the casts with the
//! same gcc that builds the reference softIoc (`gcc -O2`, x86-64):
//!
//! ```text
//! double          char  uint8   int16  uint16        int32       uint32                int64                 uint64
//! 1.7                1      1       1       1            1            1                    1                      1
//! -3.9              -3    253      -3   65533           -3   4294967293                   -3   18446744073709551613
//! 70000.9          112    112    4464    4464        70000        70000                70000                  70000
//! 3e+09              0      0       0       0  -2147483648   3000000000           3000000000             3000000000
//! -3e+09             0      0       0       0  -2147483648   1294967296          -3000000000   18446744070709551616
//! 5e+09              0      0       0       0  -2147483648    705032704           5000000000             5000000000
//! -1                -1    255      -1   65535           -1   4294967295                   -1   18446744073709551615
//! 1e+19              0      0       0       0  -2147483648            0 -9223372036854775808   10000000000000000000
//! 65535.7           -1    255      -1   65535        65535        65535                65535                  65535
//! nan                0      0       0       0  -2147483648            0 -9223372036854775808    9223372036854775808
//! inf                0      0       0       0  -2147483648            0 -9223372036854775808                      0
//! -inf               0      0       0       0  -2147483648            0 -9223372036854775808    9223372036854775808
//! ```

/// 2^31 as an `f64` — the exclusive upper bound of the 32-bit `cvttsd2si`.
const TWO_POW_31: f64 = 2_147_483_648.0;
/// 2^63 as an `f64` — the exclusive upper bound of the 64-bit `cvttsd2si`.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// x86-64 `cvttsd2si r32` — truncate toward zero into `i32`, with the
/// "integer indefinite" `i32::MIN` for NaN and for anything out of range.
///
/// The low-side bound is `<` rather than `<=`: `trunc(-2147483648.5)` is
/// `-2147483648`, which fits, and the indefinite is that same bit pattern, so
/// the two agree either way.
pub fn f64_to_i32(v: f64) -> i32 {
    if v.is_nan() || v >= TWO_POW_31 || v < -TWO_POW_31 {
        i32::MIN
    } else {
        v as i32
    }
}

/// x86-64 `cvttsd2si r64` — truncate toward zero into `i64`, indefinite
/// (`i64::MIN`) for NaN and out of range.
pub fn f64_to_i64(v: f64) -> i64 {
    if v.is_nan() || v >= TWO_POW_63 || v < -TWO_POW_63 {
        i64::MIN
    } else {
        v as i64
    }
}

/// C `(char) d` / `(epicsInt8) d` — the 32-bit convert, truncated to 8 bits.
pub fn f64_to_i8(v: f64) -> i8 {
    f64_to_i32(v) as i8
}

/// C `(epicsUInt8) d` — the same 8 bits as [`f64_to_i8`], unsigned carrier.
pub fn f64_to_u8(v: f64) -> u8 {
    f64_to_i32(v) as u8
}

/// C `(epicsInt16) d` — the 32-bit convert, truncated to 16 bits.
/// `70000.9 -> 4464`, not the saturating `32767`.
pub fn f64_to_i16(v: f64) -> i16 {
    f64_to_i32(v) as i16
}

/// C `(epicsUInt16) d` / `(epicsEnum16) d` — [`f64_to_i16`]'s bits, unsigned.
pub fn f64_to_u16(v: f64) -> u16 {
    f64_to_i32(v) as u16
}

/// C `(epicsUInt32) d` — the compiler uses the **64-bit** convert here and
/// keeps the low half, so `3.0e9` survives as `3000000000` where the signed
/// `i32` cast would have gone indefinite.
pub fn f64_to_u32(v: f64) -> u32 {
    f64_to_i64(v) as u32
}

/// C `(epicsUInt64) d` — the compiler's two-branch sequence (see the module
/// docs). NaN compares false against `2^63` and so takes the low branch.
pub fn f64_to_u64(v: f64) -> u64 {
    if v >= TWO_POW_63 {
        (f64_to_i64(v - TWO_POW_63) as u64) ^ (1u64 << 63)
    } else {
        f64_to_i64(v) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the module-doc table, straight from `gcc -O2` on x86-64.
    /// Rust's `as` saturates and fails all of the out-of-range rows.
    #[test]
    fn matches_compiled_c_x86_64() {
        #[allow(clippy::type_complexity)]
        let rows: &[(f64, i8, u8, i16, u16, i32, u32, i64, u64)] = &[
            (1.7, 1, 1, 1, 1, 1, 1, 1, 1),
            (2.2, 2, 2, 2, 2, 2, 2, 2, 2),
            (
                -3.9,
                -3,
                253,
                -3,
                65533,
                -3,
                4294967293,
                -3,
                18446744073709551613,
            ),
            (70000.9, 112, 112, 4464, 4464, 70000, 70000, 70000, 70000),
            (
                3.0e9,
                0,
                0,
                0,
                0,
                i32::MIN,
                3000000000,
                3000000000,
                3000000000,
            ),
            (
                -3.0e9,
                0,
                0,
                0,
                0,
                i32::MIN,
                1294967296,
                -3000000000,
                18446744070709551616,
            ),
            (
                5.0e9,
                0,
                0,
                0,
                0,
                i32::MIN,
                705032704,
                5000000000,
                5000000000,
            ),
            (-1.0, -1, 255, -1, 65535, -1, 4294967295, -1, u64::MAX),
            (
                1.0e19,
                0,
                0,
                0,
                0,
                i32::MIN,
                0,
                i64::MIN,
                10000000000000000000,
            ),
            (
                -1.0e19,
                0,
                0,
                0,
                0,
                i32::MIN,
                0,
                i64::MIN,
                9223372036854775808,
            ),
            (
                2147483647.5,
                -1,
                255,
                -1,
                65535,
                2147483647,
                2147483647,
                2147483647,
                2147483647,
            ),
            (
                4294967295.5,
                0,
                0,
                0,
                0,
                i32::MIN,
                4294967295,
                4294967295,
                4294967295,
            ),
            (65535.7, -1, 255, -1, 65535, 65535, 65535, 65535, 65535),
            (255.9, -1, 255, 255, 255, 255, 255, 255, 255),
            (-0.5, 0, 0, 0, 0, 0, 0, 0, 0),
            (1.0e300, 0, 0, 0, 0, i32::MIN, 0, i64::MIN, 0),
            (
                f64::NAN,
                0,
                0,
                0,
                0,
                i32::MIN,
                0,
                i64::MIN,
                9223372036854775808,
            ),
            (f64::INFINITY, 0, 0, 0, 0, i32::MIN, 0, i64::MIN, 0),
            (
                f64::NEG_INFINITY,
                0,
                0,
                0,
                0,
                i32::MIN,
                0,
                i64::MIN,
                9223372036854775808,
            ),
        ];
        for &(d, i8v, u8v, i16v, u16v, i32v, u32v, i64v, u64v) in rows {
            assert_eq!(f64_to_i8(d), i8v, "(char){d}");
            assert_eq!(f64_to_u8(d), u8v, "(epicsUInt8){d}");
            assert_eq!(f64_to_i16(d), i16v, "(epicsInt16){d}");
            assert_eq!(f64_to_u16(d), u16v, "(epicsUInt16){d}");
            assert_eq!(f64_to_i32(d), i32v, "(epicsInt32){d}");
            assert_eq!(f64_to_u32(d), u32v, "(epicsUInt32){d}");
            assert_eq!(f64_to_i64(d), i64v, "(epicsInt64){d}");
            assert_eq!(f64_to_u64(d), u64v, "(epicsUInt64){d}");
        }
    }
}
