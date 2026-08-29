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

/// C `(char) d` / `(epicsInt8) d`, saturated: clamps to `i8`'s range, NaN -> 0.
pub fn f64_to_i8(v: f64) -> i8 {
    v as i8
}

/// C `(epicsUInt8) d`, saturated: clamps to `u8`'s range, negatives and NaN -> 0.
pub fn f64_to_u8(v: f64) -> u8 {
    v as u8
}

/// C `(epicsInt16) d`, saturated: clamps to `i16`'s range, NaN -> 0.
pub fn f64_to_i16(v: f64) -> i16 {
    v as i16
}

/// C `(epicsUInt16) d` / `(epicsEnum16) d`, saturated: clamps to `u16`'s range,
/// negatives and NaN -> 0.
pub fn f64_to_u16(v: f64) -> u16 {
    v as u16
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

    /// The conversion saturates at the destination's bounds and sends NaN to 0.
    ///
    /// The out-of-range rows are exactly where a compiled x86-64 IOC would store
    /// the "integer indefinite" instead (`3.0e9 -> i32` = `INT_MIN` there). We
    /// deliberately do not: see the module docs — that value is UB, an ARM IOC
    /// saturates just as this does, and CBUG-E2 signed off on clean.
    #[test]
    fn out_of_range_saturates_and_nan_is_zero() {
        #[allow(clippy::type_complexity)]
        let rows: &[(f64, i8, u8, i16, u16, i32, u32, i64, u64)] = &[
            // In range for every destination — no policy in play, exact truncation.
            (1.7, 1, 1, 1, 1, 1, 1, 1, 1),
            (2.2, 2, 2, 2, 2, 2, 2, 2, 2),
            (-0.5, 0, 0, 0, 0, 0, 0, 0, 0),
            // Truncation is toward zero, as in C.
            (-3.9, -3, 0, -3, 0, -3, 0, -3, 0),
            // Boundary: exactly representable at each width's edge.
            (127.0, 127, 127, 127, 127, 127, 127, 127, 127),
            (255.9, 127, 255, 255, 255, 255, 255, 255, 255),
            (32767.5, 127, 255, 32767, 32767, 32767, 32767, 32767, 32767),
            (65535.7, 127, 255, 32767, 65535, 65535, 65535, 65535, 65535),
            // Over an inner width, still inside a wider one: the narrow dest
            // CLAMPS (C's x86 build would wrap here — 70000 & 0xFFFF = 4464).
            (70000.9, 127, 255, 32767, 65535, 70000, 70000, 70000, 70000),
            // Over i32, inside u32/i64/u64.
            (
                3.0e9,
                127,
                255,
                32767,
                65535,
                i32::MAX,
                3000000000,
                3000000000,
                3000000000,
            ),
            // Negative: every unsigned dest clamps to 0, not to a wrapped bit pattern.
            (-1.0, -1, 0, -1, 0, -1, 0, -1, 0),
            (-3.0e9, i8::MIN, 0, i16::MIN, 0, i32::MIN, 0, -3000000000, 0),
            // At and beyond the i32 edge.
            (
                2147483647.5,
                127,
                255,
                32767,
                65535,
                i32::MAX,
                2147483647,
                2147483647,
                2147483647,
            ),
            (
                4294967295.5,
                127,
                255,
                32767,
                65535,
                i32::MAX,
                u32::MAX,
                4294967295,
                4294967295,
            ),
            // Beyond every 32-bit dest.
            (
                1.0e19,
                127,
                255,
                32767,
                65535,
                i32::MAX,
                u32::MAX,
                i64::MAX,
                10000000000000000000,
            ),
            (-1.0e19, i8::MIN, 0, i16::MIN, 0, i32::MIN, 0, i64::MIN, 0),
            // Beyond every dest, and the infinities: saturate, never indefinite.
            (
                1.0e300,
                127,
                255,
                32767,
                65535,
                i32::MAX,
                u32::MAX,
                i64::MAX,
                u64::MAX,
            ),
            (
                f64::INFINITY,
                127,
                255,
                32767,
                65535,
                i32::MAX,
                u32::MAX,
                i64::MAX,
                u64::MAX,
            ),
            (
                f64::NEG_INFINITY,
                i8::MIN,
                0,
                i16::MIN,
                0,
                i32::MIN,
                0,
                i64::MIN,
                0,
            ),
            // NaN is 0 in every destination — an ARM IOC agrees; an x86 one stores
            // INT_MIN.
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
        assert_eq!(f64_to_i16(70000.9), 70000.9_f64 as i16);
        assert_eq!(f64_to_u32(-1.0), -1.0_f64 as u32);
    }
}
