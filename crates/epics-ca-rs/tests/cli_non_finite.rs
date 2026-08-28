//! Regression test (R13-20): a non-finite double is spelled the way C's
//! `printf` spells it — `nan`, not Rust's `NaN`.
//!
//! Probed against glibc: `%g`, `%f` and `%e` all print `nan` / `-nan` / `inf`
//! / `-inf`, the sign bit carried through. Rust's `{}` agrees on the
//! infinities and prints `NaN` for the quiet NaN, so the port diverged on
//! every NaN-valued float:
//!
//! ```text
//! caget TST:NAN    C: TST:NAN   nan       RS (pre-fix): TST:NAN   NaN
//! ```
//!
//! Verified head-to-head against the compiled C `caget` (EPICS 7.0.10.1-DEV)
//! on a live softIoc with an `ao` holding NaN.
//!
//! This drives the library's public value formatter, which is what every tool
//! prints through — value, array element, and each graphic / control limit.

#![cfg(all(feature = "client-core", not(epics_embedded_target)))]

use epics_ca_rs::EpicsValue;
use epics_ca_rs::cli::{CountPrefix, ValueFormat, format_c_g, format_value};

#[test]
fn a_nan_value_prints_c_s_lowercase_nan() {
    let fmt = ValueFormat::default();
    assert_eq!(
        format_value(
            &EpicsValue::Double(f64::NAN),
            &fmt,
            None,
            CountPrefix::Never
        ),
        "nan",
        "C `%g` prints `nan`; Rust's `{{}}` prints `NaN`"
    );
    assert_eq!(
        format_value(&EpicsValue::Float(f32::NAN), &fmt, None, CountPrefix::Never),
        "nan"
    );
    // Every graphic / control limit takes C's literal `%g` too.
    assert_eq!(format_c_g(f64::NAN), "nan");
}

#[test]
fn the_infinities_and_the_nan_sign_bit_match_glibc() {
    let fmt = ValueFormat::default();
    let v = |x: f64| format_value(&EpicsValue::Double(x), &fmt, None, CountPrefix::Never);
    assert_eq!(v(f64::INFINITY), "inf");
    assert_eq!(v(f64::NEG_INFINITY), "-inf");
    // glibc: `printf("%g", -NAN)` → `-nan`.
    assert_eq!(v(-f64::NAN), "-nan");
}
