//! C's double-to-text conversions, ported once.
//!
//! sCalc never renders a double with a general-purpose formatter: it calls
//! `cvtDoubleToString(d, s, 8)` (`sCalcPerform.c:89-96`), and its PRINTF hands
//! the format to `snprintf`. Both are C functions with fixed, testable output,
//! and neither is Rust's `{}` — which prints the shortest round-trip form and
//! agrees with C only by accident. Everything in the engine that turns a double
//! into text comes here.

/// C `cvtDoubleToString` (`cvtFast.c:111-190`).
///
/// A fixed-point renderer with `precision` fractional digits for
/// |v| <= 1e7, and two `sprintf` fall-backs outside it:
///
/// ```c
/// if (isnan(v) || precision > 8 || v > 10000000.0 || v < -10000000.0) {
///     if (precision > 8 || v > 1e16 || v < -1e16) {
///         if (precision>17) precision=17;
///         sprintf(pdest, "%*.*e", precision+7, precision, v);   /* NOTE the WIDTH */
///     } else {
///         if (precision>3) precision=3;                         /* NOTE the clamp */
///         sprintf(pdest, "%.*f", precision, v);
///     }
///     return strlen(pdest);
/// }
/// ```
///
/// so at sCalc's precision of 8 the shape of the output changes twice as the
/// magnitude grows, and the exponential form is WIDTH-padded to `precision+7`:
///
/// | v          | C                     |
/// |------------|-----------------------|
/// | PI         | `3.14159265`          |
/// | 1e7        | `10000000.00000000`   |
/// | 10000000.5 | `10000000.500` (the precision-3 clamp) |
/// | 1e16       | `10000000000000000.000` |
/// | 1.0000001e16 | `_1.00000010e+16` (leading space: width 15) |
/// | +inf       | `____________inf`     |
///
/// Compiled against the real `cvtFast.c` on this host; the table above is its
/// output.
pub fn cvt_double_to_string(v: f64, precision: u16) -> String {
    const FRAC_MULTIPLIER: [i64; 9] = [
        1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000,
    ];

    if v.is_nan() || precision > 8 || !(-10_000_000.0..=10_000_000.0).contains(&v) {
        if precision > 8 || v > 1e16 || v < -1e16 {
            let p = precision.min(17) as usize;
            return pad_start(&fmt_e(v, p, false), p + 7);
        }
        return fmt_f(v, precision.min(3) as usize);
    }

    let precision = precision as usize;
    let mut out = String::new();
    let mut value = v;
    if value < 0.0 {
        out.push('-');
        value = -value;
    }

    // `whole = (epicsInt32)flt_value; ftemp = flt_value - whole;`
    let mut whole = value as i64;
    let ftemp = value - whole as f64;

    // `fraction = (epicsInt32)(ftemp * fplace * 10); fraction = (fraction + 5) / 10;`
    // — a round-half-up on the decimal digits, not IEEE's round-half-even.
    let fplace = FRAC_MULTIPLIER[precision];
    let mut fraction = (ftemp * fplace as f64 * 10.0) as i64;
    fraction = (fraction + 5) / 10;
    if fraction / fplace >= 1 {
        whole += 1;
        fraction -= fplace;
    }

    let mut got_one = false;
    let mut iplace = 10_000_000i64;
    while iplace >= 1 {
        if whole >= iplace {
            got_one = true;
            let number = whole / iplace;
            whole -= number * iplace;
            out.push((b'0' + number as u8) as char);
        } else if got_one {
            out.push('0');
        }
        iplace /= 10;
    }
    if !got_one {
        out.push('0');
    }

    if precision > 0 {
        out.push('.');
        let mut fplace = fplace / 10;
        for _ in 0..precision {
            let number = fraction / fplace;
            fraction -= number * fplace;
            out.push((b'0' + number as u8) as char);
            fplace /= 10;
        }
    }
    out
}

/// C `to_string` (`sCalcPerform.c:89-96`) — the ONE rule for a double on sCalc's
/// stack becoming a string. NaN is spelled by hand; everything else is
/// [`cvt_double_to_string`] at precision 8.
///
/// ```c
/// #define to_string(ps) {                                 \
///     (ps)->s = &((ps)->local_string[0]);                 \
///     if (isnan((ps)->d)) strcpy((ps)->s,"NaN");          \
///     else (void)cvtDoubleToString((ps)->d, (ps)->s, 8);  \
/// }
/// ```
pub fn to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    cvt_double_to_string(v, 8)
}

/// C `printf`'s `%f`: `precision` fractional digits, always fixed-point.
pub fn fmt_f(v: f64, precision: usize) -> String {
    if !v.is_finite() {
        return non_finite(v, false);
    }
    format!("{v:.precision$}")
}

/// C `printf`'s `%g`/`%G` (C99 7.21.6.1p8): `%e` when the decimal exponent is
/// below -4 or at least the precision, `%f` otherwise, and trailing zeros are
/// dropped from the fraction either way — unless `alt` (the `#` flag) keeps them.
/// A precision of 0 is taken as 1.
pub fn fmt_g(v: f64, precision: usize, upper: bool, alt: bool) -> String {
    if !v.is_finite() {
        return non_finite(v, upper);
    }
    let p = precision.max(1);
    // The exponent C tests is the one `%e` would PRINT at precision p-1 — i.e.
    // after rounding, so 9.99e2 at p=2 is 1.0e3 and takes the `%e` branch.
    let probe = format!("{v:.*e}", p - 1);
    let exp: i32 = probe
        .split_once('e')
        .expect("Rust LowerExp always emits `e`")
        .1
        .parse()
        .expect("Rust LowerExp always emits an integer");

    let rendered = if exp < -4 || exp >= p as i32 {
        fmt_e(v, p - 1, upper)
    } else {
        fmt_f(v, (p as i32 - 1 - exp).max(0) as usize)
    };
    if alt {
        return rendered;
    }
    strip_trailing_zeros(&rendered)
}

/// `%g`'s trailing-zero removal: only within the fractional digits, and the
/// decimal point goes with them.
fn strip_trailing_zeros(s: &str) -> String {
    let (num, suffix) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    if !num.contains('.') {
        return s.to_string();
    }
    let trimmed = num.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}{suffix}")
}

/// C `printf`'s `%e`/`%E`: one digit, the fraction, then `e±` and AT LEAST TWO
/// exponent digits. Rust's `{:e}` writes no sign and no padding (`1e16`), so the
/// exponent is rebuilt here.
pub fn fmt_e(v: f64, precision: usize, upper: bool) -> String {
    if !v.is_finite() {
        return non_finite(v, upper);
    }
    let s = format!("{v:.precision$e}");
    let (mantissa, exp) = s.split_once('e').expect("Rust LowerExp always emits `e`");
    let exp: i32 = exp.parse().expect("Rust LowerExp always emits an integer");
    let e = if upper { 'E' } else { 'e' };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}{e}{sign}{:02}", exp.abs())
}

/// glibc prints a non-finite double as `inf`/`-inf`/`nan` for the lower-case
/// conversions and `INF`/`-INF`/`NAN` for the upper-case ones. (sCalc's
/// `to_string` never gets here with a NaN — it spells that itself.)
fn non_finite(v: f64, upper: bool) -> String {
    let s = if v.is_nan() {
        "nan"
    } else if v > 0.0 {
        "inf"
    } else {
        "-inf"
    };
    if upper {
        s.to_uppercase()
    } else {
        s.to_string()
    }
}

/// C's `%*.*e` field width: right-justified, blank-padded.
fn pad_start(s: &str, width: usize) -> String {
    if s.len() >= width {
        return s.to_string();
    }
    format!("{}{s}", " ".repeat(width - s.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here is the output of the real `cvtFast.c`
    /// `cvtDoubleToString(v, s, 8)`, compiled and run on this host.
    #[test]
    fn matches_compiled_cvt_double_to_string_at_precision_8() {
        let cases: &[(f64, &str)] = &[
            (std::f64::consts::PI, "3.14159265"),
            (0.0, "0.00000000"),
            (-0.0, "0.00000000"),
            (1.0, "1.00000000"),
            (-1.0, "-1.00000000"),
            (42.0, "42.00000000"),
            (0.5, "0.50000000"),
            (-0.5, "-0.50000000"),
            (1e-9, "0.00000000"),
            (1e-8, "0.00000001"),
            (5e-9, "0.00000001"),
            (-5e-9, "-0.00000001"),
            (1.23456789, "1.23456789"),
            (1.999999995, "2.00000000"),
            (9999999.99999999, "9999999.99999999"),
            (10000000.0, "10000000.00000000"),
            (10000000.5, "10000000.500"),
            (1.0000001e7, "10000001.000"),
            (1e8, "100000000.000"),
            (-1e8, "-100000000.000"),
            (1e10, "10000000000.000"),
            (123456789.125, "123456789.125"),
            (1e15, "1000000000000000.000"),
            (1e16, "10000000000000000.000"),
            (1.0000001e16, " 1.00000010e+16"),
            (1e17, " 1.00000000e+17"),
            (-1e17, "-1.00000000e+17"),
            (1e300, "1.00000000e+300"),
            (-1e300, "-1.00000000e+300"),
            (f64::INFINITY, "            inf"),
            (f64::NEG_INFINITY, "           -inf"),
            (1e-300, "0.00000000"),
            (-1e-300, "-0.00000000"),
            (2.5, "2.50000000"),
            (-2.5, "-2.50000000"),
            (0.123456785, "0.12345679"),
            (0.1, "0.10000000"),
            (1.0 / 3.0, "0.33333333"),
            (100.0, "100.00000000"),
        ];
        for (v, want) in cases {
            assert_eq!(&cvt_double_to_string(*v, 8), want, "cvtDoubleToString({v})");
        }
    }

    #[test]
    fn to_string_spells_nan_by_hand() {
        assert_eq!(to_string(f64::NAN), "NaN");
        assert_eq!(to_string(-f64::NAN), "NaN");
    }

    /// C `printf("%.2e", 1.5)` etc. — Rust's `{:e}` writes `1.5e0`, C writes
    /// `1.50e+00`.
    #[test]
    fn exponent_has_a_sign_and_two_digits() {
        assert_eq!(fmt_e(1.5, 2, false), "1.50e+00");
        assert_eq!(fmt_e(1.5e-7, 3, false), "1.500e-07");
        assert_eq!(fmt_e(1.5e120, 1, true), "1.5E+120");
        assert_eq!(fmt_e(0.0, 6, false), "0.000000e+00");
    }

    /// C `printf("%g")` of each.
    #[test]
    fn g_switches_form_and_drops_trailing_zeros() {
        assert_eq!(fmt_g(100000.0, 6, false, false), "100000");
        assert_eq!(fmt_g(1000000.0, 6, false, false), "1e+06");
        assert_eq!(fmt_g(0.0001, 6, false, false), "0.0001");
        assert_eq!(fmt_g(0.00001, 6, false, false), "1e-05");
        assert_eq!(fmt_g(1e-7, 6, true, false), "1E-07");
        assert_eq!(fmt_g(3.14159265, 6, false, false), "3.14159");
        assert_eq!(fmt_g(1.5, 6, false, false), "1.5");
        assert_eq!(fmt_g(0.0, 6, false, false), "0");
        // `#` keeps them: C `printf("%#g", 1.5)`.
        assert_eq!(fmt_g(1.5, 6, false, true), "1.50000");
    }

    /// The `%*.*e` branch of `cvtDoubleToString` at a precision the record can
    /// actually set (`PREC` > 8 makes C take it for EVERY magnitude).
    #[test]
    fn a_precision_above_eight_is_always_exponential_and_width_padded() {
        assert_eq!(cvt_double_to_string(1.0, 10), " 1.0000000000e+00");
        assert_eq!(cvt_double_to_string(1.0, 9), " 1.000000000e+00");
        // ...and precision is capped at 17.
        assert_eq!(
            cvt_double_to_string(1.0, 30).trim_start(),
            "1.00000000000000000e+00"
        );
    }
}
