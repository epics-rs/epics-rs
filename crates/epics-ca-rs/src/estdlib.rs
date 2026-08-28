//! The C parsing primitives every `double`-valued knob in this crate goes
//! through: `epicsParseDouble` / `epicsScanDouble`
//! (`libcom/src/misc/epicsStdlib.c:149-176`) and `envGetDoubleConfigParam`
//! (`libcom/src/env/envSubr.c:191-211`), plus the panic-free `f64` seconds
//! → [`Duration`] conversion those knobs feed.
//!
//! `epicsParseDouble` itself lives in
//! [`epics_base_rs::runtime::stdlib`] and is re-exported here: the link
//! classifier (`dbStaticLib.c:2346`, "a link is CONSTANT iff
//! `epicsParseDouble` accepts it") needs the identical parse, and C has one
//! `epicsParseDouble`, not two.
//!
//! Why this module exists: `Duration::from_secs_f64` PANICS on NaN, on a
//! negative value, and on anything beyond `u64::MAX` seconds — i.e. on
//! exactly the inputs C's `strtod` accepts and hands to libca as a large
//! (or never-expiring) timeout. `EPICS_CA_CONN_TMO=inf` made the port abort
//! where C's `caget` reads the PV, and `EPICS_CAS_SEND_TMO=inf` let the
//! first client connect kill the whole server. Every env-derived duration
//! now resolves through [`env_double`] + [`duration_from_secs`], so no
//! environment string can panic the process.

use std::time::Duration;

pub use epics_base_rs::runtime::stdlib::{ParseDoubleError, epics_parse_double, epics_scan_double};

/// `envGetConfigParamPtr`-shaped presence test for a variable that is **not** an
/// EPICS Base `ENV_PARAM` — there is no compiled default to fall back to,
/// because C has no such parameter at all.
///
/// Every variable in C's table goes through
/// [`epics_base_rs::runtime::env_table`] and
/// [`EnvParam::get`](epics_base_rs::runtime::env::EnvParam::get) instead.
fn env_raw(name: &str) -> Option<String> {
    epics_base_rs::runtime::env::get(name).filter(|s| !s.is_empty())
}

/// Why [`env_double`] did not yield a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvDoubleError {
    /// The parameter is absent (or empty) — C falls back to the compiled
    /// default string, silently. No diagnostic.
    Unset,
    /// The parameter is set but `epicsScanDouble` rejected it — C's
    /// `envGetDoubleConfigParam` returns -1 after printing a diagnostic.
    Invalid(ParseDoubleError),
}

/// C `envGetDoubleConfigParam` (`envSubr.c:191-211`).
///
/// A set-but-unparseable value prints C's stderr line
///
/// ```text
/// Unable to find a real number in EPICS_CA_CONN_TMO=abc
/// ```
///
/// (`envSubr.c:205-206`, `fprintf(stderr, ...)`) before reporting failure.
/// C's callers then print their OWN named diagnostic on top of it — see
/// `cac.cpp:192-193`, `udpiiu.cpp:86-89`, `online_notify.c:59-64` — so a
/// bad value yields three lines, not one.
///
/// **Only for variables outside C's `ENV_PARAM` table** — the CA-TLS and
/// port-local knobs (`EPICS_CAS_SEND_TMO`, `EPICS_CA_PUT_TIMEOUT`, …), which do
/// not appear in `envDefs.h` and therefore have no compiled default for anyone
/// to read. The caller owning the fallback is correct *there*, and only there.
///
/// A parameter that IS in the table resolves its default through
/// [`epics_base_rs::runtime::env_table`] via
/// [`EnvParam::double`](epics_base_rs::runtime::env::EnvParam::double), which
/// takes no default argument at all.
pub fn env_double(name: &str) -> Result<f64, EnvDoubleError> {
    let raw = env_raw(name).ok_or(EnvDoubleError::Unset)?;
    epics_parse_double(&raw).map_err(|e| {
        eprintln!("Unable to find a real number in {name}={raw}");
        EnvDoubleError::Invalid(e)
    })
}

/// Panic-free `f64` seconds → [`Duration`]: the workspace owner,
/// [`epics_base_rs::runtime::time::duration_from_secs`], reached under the
/// name CA code already calls.
///
/// This used to restate the owner's body — same three arms, same
/// constants, a second place for the rule to drift. It delegates instead,
/// so `EPICS_CA_*` doubles and record delay fields cannot disagree about
/// what `1e300` means.
pub fn duration_from_secs(secs: f64) -> Duration {
    epics_base_rs::runtime::time::duration_from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row probed against the compiled C `strtod` on this platform
    /// (glibc; `epicsStrtod` is `#define strtod`).
    #[test]
    fn parses_what_strtod_parses() {
        assert_eq!(epics_parse_double("30.0"), Ok(30.0));
        assert_eq!(epics_parse_double("0.5"), Ok(0.5));
        assert_eq!(epics_parse_double(".5"), Ok(0.5));
        assert_eq!(epics_parse_double("5."), Ok(5.0));
        assert_eq!(epics_parse_double("+3"), Ok(3.0));
        assert_eq!(epics_parse_double("-2.5e2"), Ok(-250.0));
        // Leading AND trailing whitespace are skipped, not extraneous.
        assert_eq!(epics_parse_double(" \t 5 \n"), Ok(5.0));
    }

    /// C99 hex floats — `strtod("0x10")` is 16.0, not a parse failure.
    #[test]
    fn parses_hex_floats() {
        assert_eq!(epics_parse_double("0x10"), Ok(16.0));
        assert_eq!(epics_parse_double("0X1A"), Ok(26.0));
        assert_eq!(epics_parse_double("-0x10"), Ok(-16.0));
        assert_eq!(epics_parse_double("0X1p4"), Ok(16.0));
        assert_eq!(epics_parse_double("0x1.8p1"), Ok(3.0));
        // Bare "0x": strtod converts the leading '0' and leaves "x".
        assert_eq!(
            epics_parse_double("0x"),
            Err(ParseDoubleError::Extraneous),
            "strtod consumes only the '0', so 'x' is extraneous"
        );
    }

    /// The subnormal boundary, every row probed against the compiled glibc
    /// `strtod`. A hex float names a subnormal EXACTLY in three characters,
    /// and glibc returns it with errno clear — `mant * 2f64.powi(exp)` used to
    /// scale by an infinite reciprocal here and report the whole range as an
    /// underflow.
    #[test]
    fn parses_hex_floats_across_the_subnormal_boundary() {
        // Exactly representable: a value, not an error.
        assert_eq!(
            epics_parse_double("0x1p-1074").map(f64::to_bits),
            Ok(0x0000_0000_0000_0001),
            "the smallest subnormal"
        );
        assert_eq!(
            epics_parse_double("0x1p-1023").map(f64::to_bits),
            Ok(0x0008_0000_0000_0000)
        );
        assert_eq!(
            epics_parse_double("-0x1p-1074").map(f64::to_bits),
            Ok(0x8000_0000_0000_0001)
        );
        assert_eq!(
            epics_parse_double("0x1p-1022").map(f64::to_bits),
            Ok(0x0010_0000_0000_0000),
            "the smallest normal"
        );
        // A zero significand names zero exactly, at any exponent.
        assert_eq!(epics_parse_double("0x0p-5000"), Ok(0.0));

        // Tiny AND inexact: ERANGE. C reports a non-zero one as overflow and a
        // zero one as underflow (`epicsStdlib.c:164`).
        assert_eq!(
            epics_parse_double("0x1.8p-1075"),
            Err(ParseDoubleError::Overflow)
        );
        assert_eq!(
            epics_parse_double("0x1.fffffffffffffp-1023"),
            Err(ParseDoubleError::Overflow),
            "tiny before rounding, though it rounds up to the smallest normal"
        );
        assert_eq!(
            epics_parse_double("0x1p-1075"),
            Err(ParseDoubleError::Underflow),
            "exactly half of the smallest subnormal ties to even, i.e. to zero"
        );
        assert_eq!(
            epics_parse_double("0x1p-2000"),
            Err(ParseDoubleError::Underflow)
        );
        assert_eq!(
            epics_parse_double("0x1p1024"),
            Err(ParseDoubleError::Overflow)
        );
    }

    /// The `inf` / `nan` words are VALID doubles for `strtod` — errno stays
    /// clear, so `epicsParseDouble` returns them.
    #[test]
    fn accepts_inf_and_nan_words() {
        assert_eq!(epics_parse_double("inf"), Ok(f64::INFINITY));
        assert_eq!(epics_parse_double("INFINITY"), Ok(f64::INFINITY));
        assert_eq!(epics_parse_double("-inf"), Ok(f64::NEG_INFINITY));
        assert!(epics_parse_double("nan").unwrap().is_nan());
        assert!(epics_parse_double("NaN(x)").unwrap().is_nan());
    }

    /// ERANGE: `strtod("1e400")` returns inf AND sets errno, so
    /// `epicsParseDouble` fails where the bare `inf` word succeeds.
    #[test]
    fn rejects_erange() {
        assert_eq!(epics_parse_double("1e400"), Err(ParseDoubleError::Overflow));
        assert_eq!(
            epics_parse_double("-1e400"),
            Err(ParseDoubleError::Overflow)
        );
        assert_eq!(
            epics_parse_double("1e-400"),
            Err(ParseDoubleError::Underflow)
        );
        // ERANGE is checked before the trailing-garbage test, as in C.
        assert_eq!(
            epics_parse_double("1e400x"),
            Err(ParseDoubleError::Overflow)
        );
    }

    #[test]
    fn rejects_no_conversion_and_extraneous() {
        assert_eq!(epics_parse_double(""), Err(ParseDoubleError::NoConversion));
        assert_eq!(
            epics_parse_double("   "),
            Err(ParseDoubleError::NoConversion)
        );
        assert_eq!(
            epics_parse_double("abc"),
            Err(ParseDoubleError::NoConversion)
        );
        assert_eq!(epics_parse_double("3x"), Err(ParseDoubleError::Extraneous));
        assert_eq!(epics_parse_double("5 6"), Err(ParseDoubleError::Extraneous));
    }

    /// The panic boundary: none of these may abort, and each maps to the
    /// C outcome for that deadline.
    #[test]
    fn duration_conversion_is_total() {
        assert_eq!(duration_from_secs(2.5), Duration::from_millis(2500));
        assert_eq!(duration_from_secs(0.0), Duration::ZERO);
        assert_eq!(duration_from_secs(f64::INFINITY), Duration::MAX);
        assert_eq!(duration_from_secs(f64::NAN), Duration::MAX);
        assert_eq!(duration_from_secs(1e300), Duration::MAX);
        assert_eq!(duration_from_secs(-1.0), Duration::ZERO);
        assert_eq!(duration_from_secs(f64::NEG_INFINITY), Duration::ZERO);
    }

    #[test]
    fn env_double_treats_empty_as_unset() {
        let name = "EPICS_RS_ESTDLIB_TEST_EMPTY";
        unsafe { std::env::set_var(name, "") };
        assert_eq!(env_double(name), Err(EnvDoubleError::Unset));
        unsafe { std::env::set_var(name, "inf") };
        assert_eq!(env_double(name), Ok(f64::INFINITY));
        unsafe { std::env::set_var(name, "1e400") };
        assert_eq!(
            env_double(name),
            Err(EnvDoubleError::Invalid(ParseDoubleError::Overflow))
        );
        unsafe { std::env::remove_var(name) };
        assert_eq!(env_double(name), Err(EnvDoubleError::Unset));
    }
}
