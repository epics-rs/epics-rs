//! C's `string -> number` parse, fallible — the single owner of the conversion.
//!
//! This is the `DBR_STRING` row of `dbConvert.c`'s put table, and it is the
//! only way a text value may become a number in a record field. Its sibling
//! [`c_cast`](super::c_cast) owns the `DBR_DOUBLE` row (a bare C cast, total);
//! the two rows are different *kinds* of conversion and that is the point:
//!
//! ```c
//! /* dbConvert.c:979-996 -- and the identical body per width */
//! static long putStringShort(dbAddr *paddr, const void *pfrom, ...)
//! {
//!     long status = epicsParseInt16(psrc, pdst++, dbConvertBase, &end);
//!     if (status)
//!         return status;          /* <-- the put FAILS. Nothing is stored. */
//!     ...
//! }
//! ```
//!
//! A `caput` of an out-of-range or unparseable string is **refused** by C: the
//! non-zero status leaves `dbPut` (`dbAccess.c:1362`), leaves `dbPutField`, and
//! `rsrv` answers the client `ECA_PUTFAIL`. The field keeps its old value. This
//! is not an obscure corner — `caput REC.PREC 32768` and `caput REC.VAL
//! notanumber` are both refusals, verified against the compiled softIoc.
//!
//! Coercing instead of parsing is therefore observably wrong in two ways at
//! once: the put is *accepted* when C rejects it, and a value C never stored is
//! stored (`32768 -> 32767`, `notanumber -> 0`).
//!
//! # `dbConvertBase == 0`
//!
//! The integer rows pass `dbConvertBase` (`epicsConvert.c:37`, `int
//! dbConvertBase = 0`) to `epicsParse*`, hence to `strtol`/`strtoul` with base
//! 0: a `0x` prefix is hex, `0b` binary, a leading `0` octal, otherwise
//! decimal. `iocInit.c:136-141` resets it to 10 only when
//! `EPICS_DB_CONVERT_DECIMAL_ONLY=YES`. Measured on the reference softIoc:
//! `caput REC.PREC 0x10` stores 16 and `caput REC.PREC 010` stores 8.
//!
//! # Trailing text is not an error
//!
//! Every `dbConvert` call site passes a non-NULL `units` pointer
//! (`epicsParseInt16(psrc, pdst++, dbConvertBase, &end)`), and `epicsParseLong`
//! only returns `S_stdlib_extraneous` when `units` is NULL
//! (`epicsStdlib.c:47-48`). So the parse takes the longest numeric prefix and
//! ignores the rest: `caput REC.PREC 5volts` stores 5, and `caput REC.PREC 1.7`
//! stores 1 (`strtol` stops at the `.`).

use crate::error::{CaError, CaResult};
use crate::types::{DbFieldType, EpicsValue};

/// A destination C reaches through a numeric parse on a `DBR_STRING` put.
///
/// The parse is total over this type, so a numeric field cannot be written from
/// a string without going through it: [`Self::of`] is the only constructor, and
/// it is the same test the caller would otherwise have had to spell out.
/// `DBF_STRING`, `DBF_ENUM` and `DBF_MENU` are absent because C gives them rows
/// of their own (`putStringString`, `putStringEnum`, `putStringMenu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericField {
    Char,
    UChar,
    Short,
    UShort,
    Long,
    ULong,
    Int64,
    UInt64,
    Float,
    Double,
}

impl NumericField {
    /// The numeric row for `t`, or `None` for the three types C parses with a
    /// converter of its own.
    pub fn of(t: DbFieldType) -> Option<Self> {
        Some(match t {
            DbFieldType::Char => Self::Char,
            DbFieldType::UChar => Self::UChar,
            DbFieldType::Short => Self::Short,
            DbFieldType::UShort => Self::UShort,
            DbFieldType::Long => Self::Long,
            DbFieldType::ULong => Self::ULong,
            DbFieldType::Int64 => Self::Int64,
            DbFieldType::UInt64 => Self::UInt64,
            DbFieldType::Float => Self::Float,
            DbFieldType::Double => Self::Double,
            DbFieldType::String | DbFieldType::Enum => return None,
        })
    }
}

/// The refusal. C distinguishes `S_stdlib_noConversion` / `_overflow` /
/// `_underflow`, but `dbPut` only tests the status for zero and `rsrv` maps
/// every non-zero to `ECA_PUTFAIL`, so the distinction is not observable to a
/// client and is not modelled.
fn refuse(field: &str, s: &str, target: NumericField) -> CaError {
    CaError::InvalidValue(format!(
        "{field}: cannot convert \"{s}\" to {target:?} (C epicsParse* refuses this put)"
    ))
}

/// C `dbFastPutConvertRoutine[DBR_STRING][target]` — parse `s`, or refuse the
/// put exactly where C's `epicsParse*` returns a non-zero status.
pub fn put_string(field: &str, target: NumericField, s: &str) -> CaResult<EpicsValue> {
    parse(target, s).ok_or_else(|| refuse(field, s, target))
}

fn parse(target: NumericField, s: &str) -> Option<EpicsValue> {
    Some(match target {
        // The signed widths range-check against the destination and refuse a
        // value outside it (`epicsStdlib.c:181-261`).
        NumericField::Char => EpicsValue::Char(in_range(parse_long(s)?, -0x80, 0x7f)? as i8 as u8),
        NumericField::Short => EpicsValue::Short(in_range(parse_long(s)?, -0x8000, 0x7fff)? as i16),
        NumericField::Long => {
            EpicsValue::Long(in_range(parse_long(s)?, -0x8000_0000, 0x7fff_ffff)? as i32)
        }
        // `epicsParseInt64` adds no check of its own: `long` is 64 bits here, so
        // `strtol`'s own ERANGE is the whole range test (`epicsStdlib.c:281-297`).
        NumericField::Int64 => EpicsValue::Int64(parse_long(s)?),

        // The unsigned widths deliberately admit a NEGATIVE value. C's test is
        //
        //     if (value > 0xffff && value <= ~0xffffUL) return S_stdlib_overflow;
        //                                     /* epicsStdlib.c:238 */
        //
        // and `strtoul("-1")` is `ULONG_MAX`, which is ABOVE `~0xffffUL` and so
        // escapes the band — C accepts it and truncates. `caput REC.<ushort> -1`
        // therefore stores 65535, it is not refused. Only the genuinely
        // out-of-range *positive* band is rejected.
        NumericField::UChar => EpicsValue::UChar(outside_band(parse_ulong(s)?, 0xff)? as u8),
        NumericField::UShort => EpicsValue::UShort(outside_band(parse_ulong(s)?, 0xffff)? as u16),
        NumericField::ULong => EpicsValue::ULong(parse_ulong_via_double(s)?),
        // No band: the destination is as wide as `unsigned long`.
        NumericField::UInt64 => EpicsValue::UInt64(parse_ulong(s)?),

        // `epicsParseFloat` (`epicsStdlib.c:318-335`) parses a double and then
        // refuses anything the `float` cast would destroy — but only for a
        // FINITE magnitude, so `Inf` and `NaN` are stored, not refused.
        NumericField::Float => {
            let v = parse_double(s)?;
            let abs = v.abs();
            if v > 0.0 && abs <= f32::MIN_POSITIVE as f64 {
                return None; // S_stdlib_underflow
            }
            if v.is_finite() && abs >= f32::MAX as f64 {
                return None; // S_stdlib_overflow
            }
            EpicsValue::Float(v as f32)
        }
        NumericField::Double => EpicsValue::Double(parse_double(s)?),
    })
}

/// `value < lo || value > hi` -> `S_stdlib_overflow`.
fn in_range(v: i64, lo: i64, hi: i64) -> Option<i64> {
    (lo..=hi).contains(&v).then_some(v)
}

/// C's unsigned band test: reject only `value > max && value <= !max`. The
/// complement — a sign-extended negative — is accepted and truncated.
fn outside_band(v: u64, max: u64) -> Option<u64> {
    (!(v > max && v <= !max)).then_some(v)
}

/// C `putStringUlong` (`dbConvert.c:1036-1067`) — the one string→integer row
/// with a fallback: on `S_stdlib_noConversion`, or on a successful parse that
/// stopped at `.`/`e`/`E`, re-parse via double, because "db_access pretends
/// unsigned long is double". So `1.0e3` stores 1000 and `".5"` stores 0. The
/// double is stored only inside `0..=UINT_MAX`; above/below it C keeps the
/// already-parsed integer prefix (`1.5e20` stores 1). An integer parse that
/// overflowed (ERANGE or the band test) gets NO fallback and stays refused.
///
/// Deviation: with no leading digits AND a double outside `0..=UINT_MAX`
/// (e.g. `-.5`), C reports success while writing nothing — the field keeps
/// its old value (`dbConvert.c:1055-1058` skips the store, status stays 0;
/// measured: `dbpf B.SVAL -.5` leaves the previous value in place).
/// "Accepted but writes nothing" is not expressible through this owner's
/// return value, so that input is refused instead. Relatedly, on a
/// double-parse ERANGE (`1e999`) C returns the error AFTER the first parse
/// already wrote the integer prefix through `pdst` (measured: the field
/// becomes 1); this owner refuses without writing — puts here are atomic.
fn parse_ulong_via_double(s: &str) -> Option<u32> {
    let d = scan_int(s);
    let int_value = if d.any {
        let v = u64::try_from(d.magnitude?).ok()?;
        let v = if d.negative { v.wrapping_neg() } else { v };
        Some(outside_band(v, 0xffff_ffff)? as u32)
    } else {
        None
    };
    let stopped_at_float = s
        .as_bytes()
        .get(d.end)
        .is_some_and(|c| matches!(c, b'.' | b'e' | b'E'));
    match int_value {
        Some(prefix) if stopped_at_float => match parse_double(s) {
            Some(dval) if (0.0..=u32::MAX as f64).contains(&dval) => Some(dval as u32),
            // Out-of-band double: C skips the store, keeping the integer
            // prefix the first parse already wrote into the field.
            Some(_) => Some(prefix),
            // Double-parse ERANGE (`1e999`): C returns that status.
            None => None,
        },
        Some(prefix) => Some(prefix),
        // No digits at all (S_stdlib_noConversion) → via-double or refuse.
        None => {
            let dval = parse_double(s)?;
            (0.0..=u32::MAX as f64)
                .contains(&dval)
                .then_some(dval as u32)
        }
    }
}

/// What the integer scanner found, before it is given a signedness.
struct Digits {
    negative: bool,
    /// Magnitude, without the sign. `None` once it exceeds `u64`, which is what
    /// `strtol`/`strtoul` report as `ERANGE`.
    magnitude: Option<u128>,
    /// Whether any digit was consumed at all. `false` is `strtol`'s
    /// `endp == str`, i.e. `S_stdlib_noConversion`.
    any: bool,
    /// Byte index where scanning stopped — `strtol`'s `*endp`. C's
    /// `putStringUlong` reads the character here to decide on the
    /// via-double fallback.
    end: usize,
}

/// The scanning half of `strtol`/`strtoul` with `base == 0`: leading space, an
/// optional sign, the base prefix, then the digits. Trailing text is left
/// unread — the caller's `units` pointer makes that legal.
fn scan_int(s: &str) -> Digits {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let negative = i < b.len() && b[i] == b'-';
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }

    // Base detection, `dbConvertBase == 0`. A prefix only counts when a digit of
    // that base follows it; otherwise the leading `0` is itself the number and
    // the `x`/`b` is trailing text. `0b` is binary on the reference toolchain
    // (glibc implements the C23 binary literal in `strtol` base 0, measured:
    // `caput REC.PREC 0b11` stores 3).
    let mut base = 10u32;
    if i < b.len() && b[i] == b'0' {
        let next = b.get(i + 1).map(|c| c.to_ascii_lowercase());
        let after = b.get(i + 2).copied();
        if next == Some(b'x') && after.is_some_and(|c| c.is_ascii_hexdigit()) {
            base = 16;
            i += 2;
        } else if next == Some(b'b') && after.is_some_and(|c| c == b'0' || c == b'1') {
            base = 2;
            i += 2;
        } else {
            // The leading `0` is a valid octal digit and is consumed by the loop.
            base = 8;
        }
    }

    let mut magnitude = Some(0u128);
    let mut any = false;
    while i < b.len() {
        let Some(d) = (b[i] as char).to_digit(base) else {
            break;
        };
        any = true;
        magnitude = magnitude
            .and_then(|m| m.checked_mul(u128::from(base)))
            .and_then(|m| m.checked_add(u128::from(d)));
        i += 1;
    }
    Digits {
        negative,
        magnitude,
        any,
        end: i,
    }
}

/// C `strtol(s, &end, 0)` plus `epicsParseLong`'s status: `None` for
/// `S_stdlib_noConversion` and for `ERANGE`.
fn parse_long(s: &str) -> Option<i64> {
    let d = scan_int(s);
    if !d.any {
        return None;
    }
    let m = d.magnitude?;
    if d.negative {
        // `-i64::MIN` has no positive counterpart, so the bound is asymmetric.
        (m <= i64::MAX as u128 + 1).then(|| (m as i128).wrapping_neg() as i64)
    } else {
        (m <= i64::MAX as u128).then_some(m as i64)
    }
}

/// C `strtoul(s, &end, 0)` plus `epicsParseULong`'s status. A leading `-`
/// negates modulo 2^64 and is NOT an error — that is what lets `-1` reach an
/// unsigned field as `ULONG_MAX`.
fn parse_ulong(s: &str) -> Option<u64> {
    let d = scan_int(s);
    if !d.any {
        return None;
    }
    let m = d.magnitude?;
    let v = u64::try_from(m).ok()?;
    Some(if d.negative { v.wrapping_neg() } else { v })
}

/// C `epicsParseDouble` (`epicsStdlib.c:150-176`): `strtod`, then ERANGE is a
/// refusal whichever way it went — overflow to infinity, or underflow to zero
/// or to a subnormal. Verified on the reference softIoc: `1e400`, `-1e400`,
/// `1e-320` and `4.9e-324` are all refused, while `NaN`, `Inf` and `infinity`
/// are stored.
fn parse_double(s: &str) -> Option<f64> {
    let (v, kind) = strtod(s)?;
    match kind {
        // An `inf`/`nan` LITERAL sets no errno: the value is exact, not a range
        // failure. Only a finite literal that *became* infinite overflowed.
        Literal::NonFinite => Some(v),
        Literal::Finite { significant } => {
            if v.is_infinite() {
                return None; // ERANGE, overflowed
            }
            if v == 0.0 && significant {
                return None; // ERANGE, underflowed to zero
            }
            if v != 0.0 && v.abs() < f64::MIN_POSITIVE {
                return None; // ERANGE, underflowed to a subnormal
            }
            Some(v)
        }
    }
}

enum Literal {
    /// An `inf`/`nan` word — exact, never a range error.
    NonFinite,
    /// A numeric literal. `significant` records whether its mantissa held a
    /// non-zero digit, which is what tells `0.0` (exact) from `1e-400` (a value
    /// that underflowed *to* zero and must be refused).
    Finite { significant: bool },
}

/// The scanning half of `strtod`: leading space, sign, then an `inf`/`nan`
/// word, a hex-float, or a decimal float. Returns the value and which kind of
/// literal it was; `None` is `endp == str` (`S_stdlib_noConversion`).
///
/// The hex form is not decoration — `strtod` accepts it and so does the
/// reference IOC: `caput REC.VAL 0x10` stores 16.
fn strtod(s: &str) -> Option<(f64, Literal)> {
    let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (sign, body) = match t.as_bytes().first() {
        Some(b'-') => (-1.0, &t[1..]),
        Some(b'+') => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    let lower = body.to_ascii_lowercase();

    if lower.starts_with("infinity") || lower.starts_with("inf") {
        return Some((sign * f64::INFINITY, Literal::NonFinite));
    }
    if lower.starts_with("nan") {
        // `strtod` gives NaN the sign it was written with; NaN has no ordering,
        // so only the sign bit differs and no consumer of a record field reads it.
        return Some((f64::NAN, Literal::NonFinite));
    }
    if lower.starts_with("0x") {
        let (v, significant) = hex_float(&lower[2..])?;
        return Some((sign * v, Literal::Finite { significant }));
    }

    // Decimal: the longest prefix Rust's own parser accepts, which is the same
    // grammar `strtod` scans. It yields `inf` on overflow and `0.0`/subnormal on
    // underflow instead of an errno, which `parse_double` then classifies.
    let b = body.as_bytes();
    let mut i = 0;
    let mut significant = false;
    let mut mantissa_digits = 0;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        if b[i].is_ascii_digit() {
            mantissa_digits += 1;
            significant |= b[i] != b'0';
        }
        i += 1;
    }
    if mantissa_digits == 0 {
        return None;
    }
    let mantissa_end = i;
    // An exponent counts only when it actually has digits — `1e` is the value 1
    // followed by the trailing text `e`.
    if i < b.len() && (b[i] | 0x20) == b'e' {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let v: f64 = body[..i]
        .parse()
        .or_else(|_| body[..mantissa_end].parse())
        .ok()?;
    Some((sign * v, Literal::Finite { significant }))
}

/// `strtod`'s hex form: `h.h[p±d]`, the digits scaled by 2^exponent.
fn hex_float(s: &str) -> Option<(f64, bool)> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut mantissa = 0.0f64;
    let mut significant = false;
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_hexdigit() {
        let d = (b[i] as char).to_digit(16)?;
        mantissa = mantissa * 16.0 + f64::from(d);
        significant |= d != 0;
        digits += 1;
        i += 1;
    }
    let mut scale = 0i32;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            let d = (b[i] as char).to_digit(16)?;
            mantissa = mantissa * 16.0 + f64::from(d);
            significant |= d != 0;
            digits += 1;
            scale -= 4;
            i += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && b[i] == b'p' {
        let mut j = i + 1;
        let neg = b.get(j) == Some(&b'-');
        if b.get(j) == Some(&b'+') || neg {
            j += 1;
        }
        let start = j;
        let mut e = 0i32;
        while j < b.len() && b[j].is_ascii_digit() {
            e = e.saturating_mul(10).saturating_add((b[j] - b'0') as i32);
            j += 1;
        }
        if j > start {
            scale += if neg { -e } else { e };
        }
    }
    Some((mantissa * (scale as f64).exp2(), significant))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expected value below was MEASURED against the compiled reference
    /// softIoc (`caput -c` then `caget`), never computed by hand.
    fn put(t: NumericField, s: &str) -> Option<EpicsValue> {
        put_string("F", t, s).ok()
    }

    // --- the refusals: the boundary C will not cross -------------------------

    #[test]
    fn integer_past_the_destination_is_refused_not_saturated() {
        // softIoc: `caput T:C.PREC 32768` -> ERROR, PREC keeps its old value.
        assert_eq!(put(NumericField::Short, "32768"), None);
        assert_eq!(put(NumericField::Short, "-32769"), None);
        assert_eq!(put(NumericField::Long, "2147483648"), None);
        assert_eq!(put(NumericField::Long, "-2147483649"), None);
        assert_eq!(put(NumericField::Char, "128"), None);
        assert_eq!(put(NumericField::Char, "-129"), None);
    }

    #[test]
    fn at_the_limit_is_accepted() {
        assert_eq!(
            put(NumericField::Short, "32767"),
            Some(EpicsValue::Short(32767))
        );
        assert_eq!(
            put(NumericField::Short, "-32768"),
            Some(EpicsValue::Short(-32768))
        );
        assert_eq!(
            put(NumericField::Long, "2147483647"),
            Some(EpicsValue::Long(2147483647))
        );
        assert_eq!(put(NumericField::Char, "127"), Some(EpicsValue::Char(127)));
    }

    #[test]
    fn text_that_is_not_a_number_is_refused_not_stored_as_zero() {
        // softIoc: `caput T:C.PREC notanumber` -> ERROR. The port used to store 0.
        assert_eq!(put(NumericField::Short, "notanumber"), None);
        assert_eq!(put(NumericField::Double, "notanumber"), None);
        assert_eq!(put(NumericField::Short, ""), None);
        assert_eq!(put(NumericField::Double, ""), None);
    }

    // --- the unsigned band: negatives are ACCEPTED, wide positives are not ---

    #[test]
    fn negative_into_unsigned_wraps_and_is_accepted() {
        // C: strtoul("-1") == ULONG_MAX, which is outside the reject band.
        assert_eq!(put(NumericField::UChar, "-1"), Some(EpicsValue::UChar(255)));
        assert_eq!(
            put(NumericField::UShort, "-1"),
            Some(EpicsValue::UShort(65535))
        );
        assert_eq!(
            put(NumericField::ULong, "-1"),
            Some(EpicsValue::ULong(4294967295))
        );
        assert_eq!(
            put(NumericField::UInt64, "-1"),
            Some(EpicsValue::UInt64(u64::MAX))
        );
    }

    /// C `putStringUlong`'s via-double fallback (`dbConvert.c:1042-1057`) —
    /// DBF_ULONG only, "db_access pretends unsigned long is double". Every
    /// expected value measured via `dbpf B.SVAL <s>` on the reference softIoc
    /// (bi.SVAL is a plain DBF_ULONG field).
    #[test]
    fn ulong_string_put_falls_back_via_double() {
        // Integer parse stops at '.'/'e' → re-parse as double.
        assert_eq!(
            put(NumericField::ULong, "1.0e3"),
            Some(EpicsValue::ULong(1000))
        );
        // No digits at all (S_stdlib_noConversion) → via-double, truncated.
        assert_eq!(put(NumericField::ULong, ".5"), Some(EpicsValue::ULong(0)));
        // Fallback double above UINT_MAX → the integer prefix is kept.
        assert_eq!(
            put(NumericField::ULong, "1.5e20"),
            Some(EpicsValue::ULong(1))
        );
        // Sign-extended prefix kept when the double is negative.
        assert_eq!(
            put(NumericField::ULong, "-1.5"),
            Some(EpicsValue::ULong(4294967295))
        );
        // Double-parse ERANGE is refused (C returns S_stdlib_overflow; it
        // has also already partially written the prefix — see the owner's
        // deviation note).
        assert_eq!(put(NumericField::ULong, "1e999"), None);
        // Band overflow on the integer parse gets NO fallback.
        assert_eq!(put(NumericField::ULong, "4294967296.5"), None);
        // No digits + double outside the band: C silently writes nothing;
        // this owner refuses (documented deviation).
        assert_eq!(put(NumericField::ULong, "-.5"), None);
        // The fallback is putStringUlong-only: UInt64 keeps the longest
        // integer prefix (C `putStringUInt64`, dbConvert.c:1089-1109, has
        // no via-double path).
        assert_eq!(
            put(NumericField::UInt64, "1.0e3"),
            Some(EpicsValue::UInt64(1))
        );
    }

    #[test]
    fn unsigned_past_the_destination_is_refused() {
        assert_eq!(put(NumericField::UChar, "256"), None);
        assert_eq!(put(NumericField::UShort, "65536"), None);
        assert_eq!(put(NumericField::ULong, "4294967296"), None);
        assert_eq!(put(NumericField::UInt64, "18446744073709551616"), None);
        // ...but the last value that fits still lands.
        assert_eq!(
            put(NumericField::UChar, "255"),
            Some(EpicsValue::UChar(255))
        );
        assert_eq!(
            put(NumericField::ULong, "4294967295"),
            Some(EpicsValue::ULong(4294967295))
        );
    }

    // --- doubles: NaN/Inf are values, 1e400 is a range failure ---------------

    #[test]
    fn nan_and_infinity_are_stored_but_overflow_is_refused() {
        // softIoc: NaN -> nan, Inf -> inf, 1e400 -> ERROR.
        assert!(
            matches!(put(NumericField::Double, "NaN"), Some(EpicsValue::Double(v)) if v.is_nan())
        );
        assert_eq!(
            put(NumericField::Double, "Inf"),
            Some(EpicsValue::Double(f64::INFINITY))
        );
        assert_eq!(
            put(NumericField::Double, "-Inf"),
            Some(EpicsValue::Double(f64::NEG_INFINITY))
        );
        assert_eq!(
            put(NumericField::Double, "infinity"),
            Some(EpicsValue::Double(f64::INFINITY))
        );
        assert_eq!(put(NumericField::Double, "1e400"), None);
        assert_eq!(put(NumericField::Double, "-1e400"), None);
        // A double holds 1e308 and 1e39 exactly fine.
        assert_eq!(
            put(NumericField::Double, "1e308"),
            Some(EpicsValue::Double(1e308))
        );
    }

    #[test]
    fn double_underflow_to_zero_or_subnormal_is_refused_but_a_real_zero_is_not() {
        // softIoc: 1e-320 and 4.9e-324 are both ERROR; plain 0 is accepted.
        assert_eq!(put(NumericField::Double, "1e-400"), None);
        assert_eq!(put(NumericField::Double, "1e-320"), None);
        assert_eq!(put(NumericField::Double, "4.9e-324"), None);
        assert_eq!(
            put(NumericField::Double, "0"),
            Some(EpicsValue::Double(0.0))
        );
        assert_eq!(
            put(NumericField::Double, "0.0"),
            Some(EpicsValue::Double(0.0))
        );
    }

    /// DBF_FLOAT has a narrower window than DBF_DOUBLE, and `epicsParseFloat`
    /// enforces it — this is the only row where `1e39` is a refusal.
    #[test]
    fn float_refuses_what_a_double_accepts() {
        assert_eq!(put(NumericField::Float, "1e39"), None);
        assert_eq!(put(NumericField::Float, "1e308"), None);
        assert_eq!(put(NumericField::Float, "1e-40"), None); // positive, <= FLT_MIN
        assert_eq!(put(NumericField::Float, "1"), Some(EpicsValue::Float(1.0)));
        // Non-finite escapes the `finite(value)` guard and is stored.
        assert_eq!(
            put(NumericField::Float, "Inf"),
            Some(EpicsValue::Float(f32::INFINITY))
        );
        assert!(
            matches!(put(NumericField::Float, "NaN"), Some(EpicsValue::Float(v)) if v.is_nan())
        );
    }

    // --- base 0, and the trailing text that is not an error ------------------

    #[test]
    fn integers_are_parsed_base_zero() {
        // softIoc: 0x10 -> 16, 010 -> 8, 0b11 -> 3.
        assert_eq!(
            put(NumericField::Short, "0x10"),
            Some(EpicsValue::Short(16))
        );
        assert_eq!(put(NumericField::Short, "010"), Some(EpicsValue::Short(8)));
        assert_eq!(put(NumericField::Short, "0b11"), Some(EpicsValue::Short(3)));
        assert_eq!(put(NumericField::Short, "+7"), Some(EpicsValue::Short(7)));
        // ...and the range check applies to the parsed value, not the text.
        assert_eq!(put(NumericField::Short, "0x8000"), None);
        // A double takes the hex form too.
        assert_eq!(
            put(NumericField::Double, "0x10"),
            Some(EpicsValue::Double(16.0))
        );
    }

    #[test]
    fn trailing_text_is_units_and_is_ignored() {
        // Every dbConvert call site passes a units pointer, so this is not
        // S_stdlib_extraneous. softIoc: 5volts -> 5, 1.7 -> 1, 1e2 -> 1.
        assert_eq!(
            put(NumericField::Short, "5volts"),
            Some(EpicsValue::Short(5))
        );
        assert_eq!(put(NumericField::Short, "1.7"), Some(EpicsValue::Short(1)));
        assert_eq!(put(NumericField::Short, "1e2"), Some(EpicsValue::Short(1)));
        assert_eq!(
            put(NumericField::Double, "1."),
            Some(EpicsValue::Double(1.0))
        );
        assert_eq!(
            put(NumericField::Double, ".5"),
            Some(EpicsValue::Double(0.5))
        );
        // Leading whitespace is skipped, as strtol does.
        assert_eq!(
            put(NumericField::Short, "  42"),
            Some(EpicsValue::Short(42))
        );
    }

    /// The three types with a converter of their own must never reach this row —
    /// enforced by the type, not by a caller remembering to check.
    #[test]
    fn string_and_enum_have_no_numeric_row() {
        assert_eq!(NumericField::of(DbFieldType::String), None);
        assert_eq!(NumericField::of(DbFieldType::Enum), None);
        assert_eq!(
            NumericField::of(DbFieldType::Double),
            Some(NumericField::Double)
        );
    }
}
