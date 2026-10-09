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
//! # Why this does not reproduce a compiled C IOC
//!
//! An out-of-range `double -> integer` cast is **undefined behaviour**
//! (C17 6.3.1.4p1), and the compiled IOC is therefore *not single-valued*.
//! Compiling that exact macro body for the two targets EPICS actually ships on
//! gives two different stored values:
//!
//! ```text
//! x86_64    cvttsd2si (%rdi), %eax   out of range -> INT_MIN;  NaN -> INT_MIN
//! aarch64   fcvtzs    w0, d0         out of range -> SATURATES; NaN -> 0
//! ```
//!
//! So `3.0e9 -> i32` is `-2147483648` on an x86-64 IOC and `2147483647` on an
//! ARM one (Raspberry Pi and Zynq IOCs are ordinary in EPICS). There is no
//! "what C does" here to be bug-for-bug faithful *to*: the behaviour is UB, and
//! UB is definitionally not a contract. An earlier revision of this module
//! reproduced the x86-64 `INT_MIN`, which did not close a divergence so much as
//! trade agreement-with-ARM for agreement-with-x86 while deliberately adopting
//! the undefined value.
//!
//! *C's bugs are not the contract; clean is the goal* — the port
//! **saturates**: an out-of-range value clamps to the destination's range and
//! NaN converts to 0. That is Rust's native `as`, and it is byte-identical to
//! a compiled C IOC on aarch64. This is a Tier 2 (semantics) decision, signed
//! off 2026-07-14; Tier 1 is untouched, because a `DBF_LONG` field carries 32
//! bits on the wire regardless of which 32 bits we chose.
//!
//! The widths narrower than 32 bits take one more, DEFINED step. Both
//! targets compile `(epicsInt8) d` / `(epicsUInt16) d` as a 32-bit convert
//! followed by a truncation to the low bits (x86-64 `cvttsd2si %eax` then
//! `%al`/`%ax`; aarch64 `fcvtzs w0` then the same): softIoc stores 300 into a
//! `DBF_CHAR` as 44 and -2.7 into a `DBF_USHORT` as 65534. Their 32-bit step
//! is x86-64's, whose out-of-range answer `INT_MIN` has all low bits clear:
//! x86-64 softIoc R7.0.10 stores 0 for inf, -inf, NaN, 5e9 and 1e10 into a
//! `DBF_SHORT`/`DBF_CHAR` field and reads 0 through a `DBR_USHORT` link. A
//! saturated step would store -1 there, a value no x86 IOC produces.
//!
//! No alarm is raised: `dbConvert`'s routines run on the put path, outside the
//! record's own process cycle, so an alarm raised here would be erased by the
//! next `recGblResetAlarms` unless routed through `nsta`/`nsev` — and even
//! routed correctly it would flag a record whose stored value is now perfectly
//! valid and in range.
//!
//! This module remains the SINGLE OWNER of the cast. Every `double -> integer`
//! conversion that models a `dbConvert` cast calls it; no such site may use a
//! bare `as`, so that the policy above is changeable in exactly one place.
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

/// C `(epicsInt32) d`, saturated: clamps to `i32`'s range, NaN -> 0.
pub fn f64_to_i32(v: f64) -> i32 {
    v as i32
}

/// C `(epicsInt64) d`, saturated: clamps to `i64`'s range, NaN -> 0.
pub fn f64_to_i64(v: f64) -> i64 {
    v as i64
}

/// The 32-bit step of a narrow cast, x86-64 `cvttsd2si`: truncated toward
/// zero, `INT_MIN` for NaN and anything outside `i32`.
fn narrow_step(v: f64) -> i32 {
    if v > -2_147_483_649.0 && v < 2_147_483_648.0 {
        v as i32
    } else {
        i32::MIN
    }
}

/// C `(char) d` / `(epicsInt8) d`: the 32-bit convert, then the low byte.
pub fn f64_to_i8(v: f64) -> i8 {
    narrow_step(v) as i8
}

/// C `(epicsUInt8) d`: the 32-bit convert, then the low byte.
pub fn f64_to_u8(v: f64) -> u8 {
    narrow_step(v) as u8
}

/// C `(epicsInt16) d`: the 32-bit convert, then the low 16 bits.
pub fn f64_to_i16(v: f64) -> i16 {
    narrow_step(v) as i16
}

/// C `(epicsUInt16) d` / `(epicsEnum16) d`: the 32-bit convert, then the low
/// 16 bits.
pub fn f64_to_u16(v: f64) -> u16 {
    narrow_step(v) as u16
}

/// C `(epicsUInt32) d`, saturated: clamps to `u32`'s range, negatives and
/// NaN -> 0.
pub fn f64_to_u32(v: f64) -> u32 {
    v as u32
}

/// C `(epicsUInt64) d`, saturated: clamps to `u64`'s range, negatives and
/// NaN -> 0.
pub fn f64_to_u64(v: f64) -> u64 {
    v as u64
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

    /// A 32-bit-or-wider dest saturates and sends NaN to 0 (CBUG-E2); a
    /// narrower one wraps x86-64's 32-bit step, whose out-of-range answer
    /// `INT_MIN` cuts to 0.
    #[test]
    fn out_of_range_saturates_and_nan_is_zero() {
        #[allow(clippy::type_complexity)]
        let rows: &[(f64, i8, u8, i16, u16, i32, u32, i64, u64)] = &[
            // In range for every destination — no policy in play, exact truncation.
            (1.7, 1, 1, 1, 1, 1, 1, 1, 1),
            (2.2, 2, 2, 2, 2, 2, 2, 2, 2),
            (-0.5, 0, 0, 0, 0, 0, 0, 0, 0),
            // Truncation is toward zero; a narrow dest takes the low bits of
            // the 32-bit result (softIoc: -2.7 -> DBF_USHORT 65534).
            (-3.9, -3, 253, -3, 65533, -3, 0, -3, 0),
            (-1.0, -1, 255, -1, 65535, -1, 0, -1, 0),
            (127.0, 127, 127, 127, 127, 127, 127, 127, 127),
            (255.9, -1, 255, 255, 255, 255, 255, 255, 255),
            (32767.5, -1, 255, 32767, 32767, 32767, 32767, 32767, 32767),
            (65535.7, -1, 255, -1, 65535, 65535, 65535, 65535, 65535),
            // softIoc: 70000 -> DBF_CHAR 112, DBF_USHORT 4464.
            (70000.9, 112, 112, 4464, 4464, 70000, 70000, 70000, 70000),
            // Past i32: a wide dest saturates; a narrow one takes x86-64's
            // INT_MIN step, so 0 (softIoc: 5e9 and 1e10 -> DBF_SHORT 0).
            (
                3.0e9,
                0,
                0,
                0,
                0,
                i32::MAX,
                3000000000,
                3000000000,
                3000000000,
            ),
            (-3.0e9, 0, 0, 0, 0, i32::MIN, 0, -3000000000, 0),
            (
                2147483647.5,
                -1,
                255,
                -1,
                65535,
                i32::MAX,
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
                i32::MAX,
                u32::MAX,
                4294967295,
                4294967295,
            ),
            (
                1.0e19,
                0,
                0,
                0,
                0,
                i32::MAX,
                u32::MAX,
                i64::MAX,
                10000000000000000000,
            ),
            (-1.0e19, 0, 0, 0, 0, i32::MIN, 0, i64::MIN, 0),
            (1.0e300, 0, 0, 0, 0, i32::MAX, u32::MAX, i64::MAX, u64::MAX),
            (
                f64::INFINITY,
                0,
                0,
                0,
                0,
                i32::MAX,
                u32::MAX,
                i64::MAX,
                u64::MAX,
            ),
            (f64::NEG_INFINITY, 0, 0, 0, 0, i32::MIN, 0, i64::MIN, 0),
            // NaN is 0 in every destination: x86's INT_MIN in the narrow ones.
            (f64::NAN, 0, 0, 0, 0, 0, 0, 0, 0),
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

    /// The module's reason to exist: one place to change the policy. A bare `as`
    /// at a call site would silently opt out of it.
    #[test]
    fn saturation_is_rusts_native_as() {
        assert_eq!(f64_to_i32(3.0e9), 3.0e9 as i32);
        assert_eq!(f64_to_u32(-1.0), -1.0_f64 as u32);
    }
}
