//! C `printf` float conversions, the workspace's one owner of "double → text
//! the way C prints it".
//!
//! Rust's float formatting is not `printf`: `{}` is the shortest round-trip
//! form, `{:e}` writes a bare exponent (`1e5`, not `1e+05`), there is no `%g`,
//! and a NaN is `NaN` with its sign dropped. [`CFloat`] keeps Rust's exact
//! decimal digits and changes only what C spells differently:
//!
//! | Rust | C | conversion |
//! | --- | --- | --- |
//! | `{:.N}` of `CFloat(x)` | `%.Nf` | [`Display`](fmt::Display) |
//! | `{:.Ne}` / `{:.NE}` | `%.Ne` / `%.NE` | [`LowerExp`](fmt::LowerExp) / [`UpperExp`](fmt::UpperExp) |
//! | `{:.N}` of `CFloat(x).g()` | `%.Ng` | [`CFloatG`] |
//!
//! Without a precision each takes C's default of 6. The width, `-`, `+`, `0`
//! and `#` flags mean what they mean in C. Non-finite values are spelled as
//! glibc spells them for every conversion — `nan`, `-nan` (the sign bit of a
//! NaN is printed), `inf`, `-inf`, upper case for `%E` — and the `0` flag pads
//! them with spaces, as glibc does. `-0.0` keeps its sign: `-0.000000`,
//! `-0`.

use std::fmt::{self, Write as _};

/// A double printed as C `printf` prints it. See the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CFloat(pub f64);

/// A double printed by C `%g`; made by [`CFloat::g`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CFloatG(pub f64);

impl CFloat {
    /// The `%g` conversion of this value.
    pub fn g(self) -> CFloatG {
        CFloatG(self.0)
    }
}

#[derive(Clone, Copy)]
enum Conv {
    F,
    E { upper: bool },
    G,
}

const DEFAULT_PRECISION: usize = 6;

impl fmt::Display for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::F)
    }
}

impl fmt::LowerExp for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::E { upper: false })
    }
}

impl fmt::UpperExp for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::E { upper: true })
    }
}

impl fmt::Display for CFloatG {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::G)
    }
}

fn emit(f: &mut fmt::Formatter<'_>, x: f64, conv: Conv) -> fmt::Result {
    let sign = if x.is_sign_negative() {
        "-"
    } else if f.sign_plus() {
        "+"
    } else {
        ""
    };
    let upper = matches!(conv, Conv::E { upper: true });
    let body = if x.is_nan() {
        if upper { "NAN" } else { "nan" }.to_string()
    } else if x.is_infinite() {
        if upper { "INF" } else { "inf" }.to_string()
    } else {
        let a = x.abs();
        let alt = f.alternate();
        let p = f.precision().unwrap_or(DEFAULT_PRECISION);
        match conv {
            Conv::F => fixed(a, p, alt),
            Conv::E { upper } => exponential(a, p, alt, upper),
            Conv::G => general(a, p, alt),
        }
    };
    let len = sign.len() + body.len();
    let pad = f.width().unwrap_or(0).saturating_sub(len);
    if pad == 0 {
        return write!(f, "{sign}{body}");
    }
    if matches!(f.align(), Some(fmt::Alignment::Left)) {
        write!(f, "{sign}{body}")?;
        (0..pad).try_for_each(|_| f.write_char(f.fill()))
    } else if f.sign_aware_zero_pad() && x.is_finite() {
        write!(f, "{sign}{:0>pad$}{body}", "")
    } else {
        (0..pad).try_for_each(|_| f.write_char(f.fill()))?;
        write!(f, "{sign}{body}")
    }
}

/// `%.pf` of a non-negative finite value. Rust's fixed-precision digits are
/// exact and round half to even, as glibc's are.
fn fixed(a: f64, p: usize, alt: bool) -> String {
    let mut s = format!("{a:.p$}");
    if alt && p == 0 {
        s.push('.');
    }
    s
}

/// `%.pe`: Rust's digits, C's exponent — signed and at least two digits.
fn exponential(a: f64, p: usize, alt: bool, upper: bool) -> String {
    let s = format!("{a:.p$e}");
    let (mantissa, exp) = s.split_once('e').expect("LowerExp emits an exponent");
    let exp: i32 = exp.parse().expect("LowerExp emits a decimal exponent");
    c_exponent(mantissa, exp, alt && p == 0, upper)
}

fn c_exponent(mantissa: &str, exp: i32, add_point: bool, upper: bool) -> String {
    let point = if add_point { "." } else { "" };
    let e = if upper { 'E' } else { 'e' };
    let es = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}{point}{e}{es}{:02}", exp.unsigned_abs())
}

/// `%.pg` (C99 7.19.6.1): with `P` the precision (0 counts as 1) and `X` the
/// exponent `%e` would print at `P - 1`, `%f` at `P - 1 - X` when
/// `P > X >= -4`, otherwise `%e` at `P - 1`; trailing zeros and a bare point
/// go unless `#`.
fn general(a: f64, p: usize, alt: bool) -> String {
    let p = p.max(1);
    let sci = format!("{a:.prec$e}", prec = p - 1);
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp emits an exponent");
    let exp: i32 = exp.parse().expect("LowerExp emits a decimal exponent");
    if exp >= -4 && exp < p as i32 {
        let s = fixed(a, (p as i32 - 1 - exp) as usize, alt);
        if alt { s } else { strip_zeros(&s).to_string() }
    } else if alt {
        c_exponent(mantissa, exp, p == 1, false)
    } else {
        c_exponent(strip_zeros(mantissa), exp, false, false)
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Byte equality with glibc's `snprintf`; newlib (RTEMS), musl and the macOS
/// libc (which prints a negative NaN as `nan`) are not that reference.
#[cfg(all(test, unix, target_env = "gnu"))]
mod glibc {
    use super::tests::GLIBC_ALT_G_CARRY;
    use super::*;

    /// glibc's own rendering of `x` through the one-conversion `spec`.
    fn c(spec: &str, x: f64) -> String {
        let spec = std::ffi::CString::new(spec).unwrap();
        let mut buf = vec![0u8; 512];
        // SAFETY: `spec` holds exactly one double conversion and `buf` is
        // writable for its whole length, which is the size passed.
        let n = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), spec.as_ptr(), x) };
        assert!(n >= 0 && (n as usize) < buf.len(), "snprintf {n}");
        buf.truncate(n as usize);
        String::from_utf8(buf).unwrap()
    }

    fn values() -> Vec<f64> {
        let mut v = vec![
            0.0,
            -0.0,
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0xfff8_0000_0000_0001),
            f64::INFINITY,
            f64::NEG_INFINITY,
            1.0,
            -1.0,
            0.5,
            1.5,
            2.5,
            0.125,
            0.375,
            -0.125,
            0.1,
            0.2 + 0.1,
            1.0 / 3.0,
            2.0 / 3.0,
            9.5,
            9.999995,
            9.9999949,
            99999.95,
            999999.5,
            123456.0,
            1234567.0,
            1e-4,
            1e-5,
            0.00012345,
            0.000099999,
            1e15,
            1e16,
            1e21,
            1e22,
            1e100,
            -1e-300,
            123.456,
            -987654.321,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            f64::EPSILON,
            std::f64::consts::PI,
            -std::f64::consts::E,
            4503599627370497.0,
        ];
        v.extend((-30..=30).map(|e| 1.2345678901234567 * 10f64.powi(e)));
        v
    }

    #[test]
    fn precision_conversions_match_glibc() {
        for x in values() {
            for p in 0..=20usize {
                let ps = format!(".{p}");
                assert_eq!(
                    format!("{:.p$}", CFloat(x)),
                    c(&format!("%{ps}f"), x),
                    "%{ps}f {x:e}"
                );
                assert_eq!(
                    format!("{:.p$e}", CFloat(x)),
                    c(&format!("%{ps}e"), x),
                    "%{ps}e {x:e}"
                );
                assert_eq!(
                    format!("{:.p$E}", CFloat(x)),
                    c(&format!("%{ps}E"), x),
                    "%{ps}E {x:e}"
                );
                assert_eq!(
                    format!("{:.p$}", CFloat(x).g()),
                    c(&format!("%{ps}g"), x),
                    "%{ps}g {x:e}"
                );
                if !GLIBC_ALT_G_CARRY.iter().any(|&(v, q, _)| v == x && q == p) {
                    assert_eq!(
                        format!("{:#.p$}", CFloat(x).g()),
                        c(&format!("%#{ps}g"), x),
                        "%#{ps}g {x:e}"
                    );
                }
            }
        }
    }

    #[test]
    fn default_precision_and_flags_match_glibc() {
        for x in values() {
            let cases = [
                (format!("{}", CFloat(x)), "%f"),
                (format!("{:e}", CFloat(x)), "%e"),
                (format!("{}", CFloat(x).g()), "%g"),
                (format!("{:+.3}", CFloat(x)), "%+.3f"),
                (format!("{:12.3}", CFloat(x)), "%12.3f"),
                (format!("{:<12.3}", CFloat(x)), "%-12.3f"),
                (format!("{:012.3}", CFloat(x)), "%012.3f"),
                (format!("{:+015.4e}", CFloat(x)), "%+015.4e"),
                (format!("{:#.0}", CFloat(x)), "%#.0f"),
                (format!("{:#.0e}", CFloat(x)), "%#.0e"),
                (format!("{:<14}", CFloat(x).g()), "%-14g"),
                (format!("{:010}", CFloat(x).g()), "%010g"),
            ];
            for (got, spec) in cases {
                assert_eq!(got, c(spec, x), "{spec} {x:e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// glibc 2.39 `%#.Pg` drops the zeros `#` keeps when rounding carries the
    /// value into `%e` form (`1.e+05`); C99 7.19.6.1 keeps them. Not copied.
    pub(super) const GLIBC_ALT_G_CARRY: [(f64, usize, &str); 2] =
        [(99999.95, 5, "1.0000e+05"), (999999.5, 6, "1.00000e+06")];

    #[test]
    fn alternate_g_keeps_zeros_through_a_rounding_carry() {
        for (x, p, want) in GLIBC_ALT_G_CARRY {
            assert_eq!(format!("{:#.p$}", CFloat(x).g()), want);
        }
    }

    #[test]
    fn non_finite_and_negative_zero_spellings() {
        assert_eq!(CFloat(f64::NAN).to_string(), "nan");
        assert_eq!(CFloat(-f64::NAN).to_string(), "-nan");
        assert_eq!(format!("{:e}", CFloat(-f64::INFINITY)), "-inf");
        assert_eq!(format!("{:E}", CFloat(f64::NAN)), "NAN");
        assert_eq!(CFloat(f64::INFINITY).g().to_string(), "inf");
        assert_eq!(CFloat(-0.0).to_string(), "-0.000000");
        assert_eq!(CFloat(-0.0).g().to_string(), "-0");
    }

    #[test]
    fn flags_pad_as_c_does() {
        assert_eq!(format!("{:+08.2}", CFloat(-1.5)), "-0001.50");
        assert_eq!(format!("{:+.1e}", CFloat(12345.0)), "+1.2e+04");
        assert_eq!(format!("{:<8}|", CFloat(0.5).g()), "0.5     |");
        assert_eq!(format!("{:08}", CFloat(-f64::NAN).g()), "    -nan");
        assert_eq!(format!("{:#.0}", CFloat(3.0)), "3.");
        assert_eq!(format!("{:.3}", CFloat(1e-5).g()), "1e-05");
    }
}
