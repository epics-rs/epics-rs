//! sCalc's double-to-text rules.
//!
//! sCalc never renders a double with a general-purpose formatter: it calls
//! `cvtDoubleToString(d, s, 8)` (`sCalcPerform.c:89-96`), and its PRINTF hands
//! the format to `snprintf`. Both have one port in this crate —
//! `cvt_double_to_string` and
//! [`CFloat`] — and neither is Rust's `{}`, which prints the shortest
//! round-trip form and agrees with C only by accident.

use epics_libcom_rs::cfmt::CFloat;

/// C `to_string` (`sCalcPerform.c:89-96`) — the ONE rule for a double on sCalc's
/// stack becoming a string. NaN is spelled by hand; everything else is
/// `cvtDoubleToString` at precision 8.
///
/// ```c
/// #define to_string(ps) {                                 \
///     (ps)->s = &((ps)->local_string[0]);                 \
///     if (isnan((ps)->d)) strcpy((ps)->s,"NaN");          \
///     else (void)cvtDoubleToString((ps)->d, (ps)->s, 8);  \
/// }
/// ```
///
/// At precision 8 the shape of the output changes twice as the magnitude
/// grows (`cvtFast.c:111-190`), and the exponential form is WIDTH-padded to
/// `precision+7`:
///
/// | v          | C                     |
/// |------------|-----------------------|
/// | PI         | `3.14159265`          |
/// | 1e7        | `10000000.00000000`   |
/// | 10000000.5 | `10000000.500` (the precision-3 clamp) |
/// | 1e16       | `10000000000000000.000` |
/// | 1.0000001e16 | `_1.00000010e+16` (leading space: width 15) |
/// | +inf       | `____________inf`     |
pub fn to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    crate::types::cvt_double_to_string(v, 8)
}

/// C `%e %E %f %F %g %G`, `#` included, through `cfmt::CFloat`. The
/// upper-case conversions differ from the lower only in letter case
/// (`E`, `INF`, `NAN`), and CFloat's output has no other letters.
pub(crate) fn format_float_conv(conv: u8, val: f64, prec: usize, alt_form: bool) -> String {
    let s = match (conv.to_ascii_lowercase(), alt_form) {
        (b'e', false) => format!("{:.prec$e}", CFloat(val)),
        (b'e', true) => format!("{:#.prec$e}", CFloat(val)),
        (b'g', false) => format!("{:.prec$}", CFloat(val).g()),
        (b'g', true) => format!("{:#.prec$}", CFloat(val).g()),
        (_, false) => format!("{:.prec$}", CFloat(val)),
        (_, true) => format!("{:#.prec$}", CFloat(val)),
    };
    if conv.is_ascii_uppercase() {
        s.to_ascii_uppercase()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::cvt_double_to_string;

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
            assert_eq!(&to_string(*v), want, "cvtDoubleToString({v})");
        }
    }

    /// The `%.*f` fall-back prints a NaN's sign bit, as glibc does.
    #[test]
    fn a_negative_nan_keeps_its_sign() {
        assert_eq!(cvt_double_to_string(-f64::NAN, 8), "-nan");
    }

    #[test]
    fn to_string_spells_nan_by_hand() {
        assert_eq!(to_string(f64::NAN), "NaN");
        assert_eq!(to_string(-f64::NAN), "NaN");
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
