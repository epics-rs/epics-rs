//! Helpers shared across the `caget` / `caput` / `cainfo` / `camonitor`
//! command-line binaries.

use epics_base_rs::types::EpicsValue;

/// Default CA CLI timeout in seconds when neither `-w` nor a usable
/// `EPICS_CLI_TIMEOUT` env var is set.
pub const DEFAULT_CLI_TIMEOUT_SECS: f64 = 1.0;

/// Deadline meaning "wait indefinitely", used for a CLI `-w 0` /
/// `EPICS_CLI_TIMEOUT=0`. C `caget`/`caput`/`camonitor` pass `caTimeout`
/// straight to `ca_pend_io` / `ca_pend_event`, where a value of `0.0`
/// waits forever — `ca_pend_io(0)` calls `pendIO(DBL_MAX)` and
/// `ca_pend_event(0)` loops `pendEvent(60.0)` without end (EPICS base
/// `access.cpp:495-499,468-474`). A far-future finite `Duration`
/// (≈10 years) reproduces that "0 == forever" without an `Option`,
/// keeping the `Duration`-typed client API (`wait_connected`,
/// `get_with_timeout`, `put_with_timeout`) unchanged; it is effectively
/// unbounded for any CLI session and stays well inside tokio's timer
/// range, so arming a `timeout` / `sleep` with it does not overflow.
pub const INDEFINITE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// Read `EPICS_CLI_TIMEOUT` from the environment, falling back to
/// [`DEFAULT_CLI_TIMEOUT_SECS`] when unset, unparsable, or non-finite.
/// Mirrors C `tool_lib.c:use_ca_timeout_env` (commit 1d056c6): it sets
/// the timeout to any value `epicsScanDouble` accepts and only falls
/// back to the default on a parse failure — the env var is consulted
/// only when the caller did not pass `-w`/`--wait`. A value of `0` is a
/// valid timeout meaning "wait forever" (see [`INDEFINITE_TIMEOUT`]), so
/// it is passed through here and resolved by [`timeout_duration`];
/// negatives and non-finite values stay clamped to the default.
pub fn env_default_timeout() -> f64 {
    std::env::var("EPICS_CLI_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(DEFAULT_CLI_TIMEOUT_SECS)
}

/// Convert a user-supplied timeout (CLI `-w` or env var) into a
/// `std::time::Duration`.
///
/// A value of `0` means "wait indefinitely", matching C `ca_pend_io(0)`
/// / `ca_pend_event(0)` (see [`INDEFINITE_TIMEOUT`]) — NOT the 1 s
/// default. `Duration::from_secs_f64` panics on NaN / infinity /
/// negative values; clap accepts those literally so this guard clamps
/// them to [`DEFAULT_CLI_TIMEOUT_SECS`] (defensive, epics-base 1655d68e).
pub fn timeout_duration(secs: f64) -> std::time::Duration {
    if secs == 0.0 {
        return INDEFINITE_TIMEOUT;
    }
    let s = if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        DEFAULT_CLI_TIMEOUT_SECS
    };
    std::time::Duration::from_secs_f64(s)
}

/// Field width the C tools (`caget` / `camonitor` / `caput -l`) use
/// when printing the PV name column: `printf("%-30s ...", name)` —
/// 30 chars left-aligned, then one space before the value. Mirrors
/// `epics-base/modules/ca/src/tools/tool_lib.c::print_value`'s width.
pub const PV_NAME_WIDTH: usize = 30;

/// Float number representation requested via `-e` / `-f` / `-g`.
#[derive(Debug, Clone, Copy)]
pub enum FloatStyle {
    /// `%g` — shortest of `%f` / `%e`. C tools default. Precision is
    /// the count of *significant* digits.
    G,
    /// `%e` — scientific notation. Precision is digits after decimal.
    E,
    /// `%f` — fixed-point. Precision is digits after decimal.
    F,
}

/// Float formatting options. C precision defaults to 6 for all three
/// styles per `printf(3)`.
#[derive(Debug, Clone, Copy)]
pub struct FloatFormat {
    pub style: FloatStyle,
    pub precision: u32,
}

impl Default for FloatFormat {
    fn default() -> Self {
        Self {
            style: FloatStyle::G,
            precision: 6,
        }
    }
}

/// Integer formatting requested via `-0x` / `-0o` / `-0b` (base for
/// integer types) and `-lx` / `-lo` / `-lb` (round-float-to-long
/// then print in base). C tool default is decimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntStyle {
    Dec,
    Hex,
    Oct,
    Bin,
}

/// Per-tool CLI formatting state.
#[derive(Debug, Clone)]
pub struct ValueFormat {
    pub float: FloatFormat,
    pub int_style: IntStyle,
    /// Round-float-to-long-and-render in `int_style` (the `-lx` / `-lo`
    /// / `-lb` C-tool flags). Only applies to floating-point values.
    pub float_as_int: bool,
    /// `-n` flag: print enum value as its integer index instead of
    /// the menu string.
    pub enum_as_number: bool,
    /// `-S` flag: render `DBR_CHAR` arrays as a NUL-terminated string
    /// (long-string CA convention).
    pub char_array_as_string: bool,
    /// `-# <count>` flag: cap displayed array elements; `None` means
    /// "all". Acts on display only — the request still asks for the
    /// requested count from the IOC.
    pub max_elements: Option<usize>,
    /// `-F <ofs>` flag: replacement field separator. Defaults to a
    /// single space.
    pub field_separator: char,
}

impl Default for ValueFormat {
    fn default() -> Self {
        Self {
            float: FloatFormat::default(),
            int_style: IntStyle::Dec,
            float_as_int: false,
            enum_as_number: false,
            char_array_as_string: false,
            max_elements: None,
            field_separator: ' ',
        }
    }
}

/// Render `EpicsValue` for CA tool output, matching C `tool_lib.c::
/// print_value`. Scalars are bare (no count prefix); arrays are
/// `count<sep>v0<sep>v1...` (the count is part of the value, not the
/// PV-name column). Enum strings are NOT resolved here — caller passes
/// `enum_strings = Some(&["off","on",...])` when it has them, else
/// the integer index is used (matches `-n` flag default when no enum
/// metadata is available). `format_value` does not emit a trailing
/// newline.
///
/// `req_elems_present` mirrors C `caget.c:286` / the `PRN_TIME_VAL_STS`
/// macro (`tool_lib.c:486`): the array element-count prefix is emitted
/// only when `reqElems || pv->nElems > 1`. Pass `true` when the user
/// supplied `-#` on the command line; a genuine 1-element waveform read
/// without `-#` then prints just the value with no count prefix.
pub fn format_value(
    v: &EpicsValue,
    fmt: &ValueFormat,
    enum_strings: Option<&[String]>,
    req_elems_present: bool,
) -> String {
    let sep = fmt.field_separator;
    match v {
        EpicsValue::String(s) => escape_from_raw(s.as_bytes()),
        EpicsValue::Short(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::Long(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::Int64(n) => format_int_wide(n.to_string(), *n as u64, fmt.int_style),
        EpicsValue::UInt64(n) => format_int_wide(n.to_string(), *n, fmt.int_style),
        // u16/u32 widen losslessly into i64 (non-negative, in range), so the
        // plain integer formatter is correct — no wide-unsigned path needed.
        EpicsValue::UShort(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::ULong(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::Char(n) => format_int_i64((*n as i8) as i64, fmt.int_style),
        EpicsValue::Enum(idx) => format_enum(*idx as i64, fmt, enum_strings),
        // Transient NTEnum carrier never reaches CA serialization (coerced in
        // base at the link-write boundary); format its index like a DBF_ENUM.
        EpicsValue::EnumWithChoices { index, .. } => format_enum(*index as i64, fmt, enum_strings),
        EpicsValue::Float(x) => format_float(*x as f64, fmt),
        EpicsValue::Double(x) => format_float(*x, fmt),
        EpicsValue::ShortArray(arr) => render_array_int(
            arr.iter().map(|&n| n as i64),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::LongArray(arr) => render_array_int(
            arr.iter().map(|&n| n as i64),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::Int64Array(arr) => render_array_iter(
            arr.iter()
                .map(|&n| format_int_wide(n.to_string(), n as u64, fmt.int_style)),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::UInt64Array(arr) => render_array_iter(
            arr.iter()
                .map(|&n| format_int_wide(n.to_string(), n, fmt.int_style)),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::UShortArray(arr) => render_array_int(
            arr.iter().map(|&n| n as i64),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::ULongArray(arr) => render_array_int(
            arr.iter().map(|&n| n as i64),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::EnumArray(arr) => {
            let mut parts = Vec::with_capacity(arr.len() + 1);
            if req_elems_present || arr.len() > 1 {
                parts.push(arr.len().to_string());
            }
            let take = fmt.max_elements.unwrap_or(arr.len()).min(arr.len());
            for &idx in &arr[..take] {
                parts.push(format_enum(idx as i64, fmt, enum_strings));
            }
            parts.join(&sep.to_string())
        }
        EpicsValue::FloatArray(arr) => render_array_iter(
            arr.iter().map(|&x| format_float(x as f64, fmt)),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::DoubleArray(arr) => render_array_iter(
            arr.iter().map(|&x| format_float(x, fmt)),
            arr.len(),
            fmt,
            sep,
            req_elems_present,
        ),
        EpicsValue::CharArray(arr) => {
            // C `caget.c` renders a CHAR array as a long-string only when
            // `charArrAsStr && (reqElems || nElems > 1)` — a 1-element
            // CHAR array with `-S` but no `-#` falls through to numeric.
            if fmt.char_array_as_string && (req_elems_present || arr.len() > 1) {
                // Long-string convention: bytes up to first NUL, then
                // EPICS-escaped (caget.c:322-327 escapes the prefix).
                let end = arr.iter().position(|&b| b == 0).unwrap_or(arr.len());
                escape_from_raw(&arr[..end])
            } else {
                render_array_int(
                    arr.iter().map(|&b| (b as i8) as i64),
                    arr.len(),
                    fmt,
                    sep,
                    req_elems_present,
                )
            }
        }
        EpicsValue::StringArray(arr) => {
            let mut parts = Vec::with_capacity(arr.len() + 1);
            if req_elems_present || arr.len() > 1 {
                parts.push(arr.len().to_string());
            }
            let take = fmt.max_elements.unwrap_or(arr.len()).min(arr.len());
            parts.extend(arr[..take].iter().map(|s| escape_from_raw(s.as_bytes())));
            parts.join(&sep.to_string())
        }
    }
}

fn render_array_int<I: Iterator<Item = i64>>(
    iter: I,
    total: usize,
    fmt: &ValueFormat,
    sep: char,
    req_elems_present: bool,
) -> String {
    let take = fmt.max_elements.unwrap_or(total).min(total);
    let mut parts = Vec::with_capacity(take + 1);
    if req_elems_present || total > 1 {
        parts.push(total.to_string());
    }
    for n in iter.take(take) {
        parts.push(format_int_i64(n, fmt.int_style));
    }
    parts.join(&sep.to_string())
}

fn render_array_iter<I: Iterator<Item = String>>(
    iter: I,
    total: usize,
    fmt: &ValueFormat,
    sep: char,
    req_elems_present: bool,
) -> String {
    let take = fmt.max_elements.unwrap_or(total).min(total);
    let mut parts = Vec::with_capacity(take + 1);
    if req_elems_present || total > 1 {
        parts.push(total.to_string());
    }
    parts.extend(iter.take(take));
    parts.join(&sep.to_string())
}

fn format_enum(idx: i64, fmt: &ValueFormat, enum_strings: Option<&[String]>) -> String {
    if !fmt.enum_as_number
        && let Some(strs) = enum_strings
        && idx >= 0
        && (idx as usize) < strs.len()
    {
        return strs[idx as usize].clone();
    }
    format_int_i64(idx, fmt.int_style)
}

/// Port of EPICS `epicsStrnEscapedFromRaw` (`epicsString.c:120-159`):
/// render raw bytes as printable text for CA CLI readback. The C control
/// escapes (`\a \b \f \n \r \t \v \\ \' \" \0`) map to their two-char
/// form, ASCII-printable bytes (0x20-0x7E) pass through, and every other
/// byte becomes `\xHH` (lowercase hex). C `val2str` runs every DBR_STRING
/// element through this (tool_lib.c:135), and `caget -S` escapes the
/// long-string byte prefix (caget.c:322-327). Operates per byte so
/// multi-byte UTF-8 is escaped exactly as C escapes the raw char buffer,
/// rather than emitting real control/non-printable bytes into the stream.
fn escape_from_raw(src: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(src.len());
    for &c in src {
        match c {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0 => out.push_str("\\0"),
            0x20..=0x7e => out.push(c as char),
            _ => {
                out.push('\\');
                out.push('x');
                out.push(HEX[(c >> 4) as usize] as char);
                out.push(HEX[(c & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Format a CA classic integer (`DBR_INT`/`DBR_LONG`/`DBR_CHAR`, an ENUM
/// index, or a float rounded to long) with EPICS `sprint_long` semantics
/// (`tool_lib.c:64-91`). C widens every such value to a 32-bit
/// `dbr_long_t` and prints:
///   dec: `%d`   oct: `0o%o`   hex: `0x%X` (UPPERCASE)
///   bin: bare bit digits, leading zeros skipped, no `0b`; zero -> "0".
/// The non-decimal bases reinterpret the low 32 bits as unsigned, so `-1`
/// prints as `0xFFFFFFFF` / `0o37777777777` / 32 ones — NOT the 64-bit
/// `0xffffffffffffffff` the pre-fix `as u64` cast produced. `format!("{:b}",
/// 0u32)` already yields the bare `0` C special-cases.
fn format_int_i64(n: i64, style: IntStyle) -> String {
    let v32 = n as i32; // C: (dbr_long_t)(int) val
    let bits = v32 as u32; // unsigned reinterpretation for the bases
    match style {
        IntStyle::Dec => v32.to_string(),
        IntStyle::Hex => format!("0x{bits:X}"),
        IntStyle::Oct => format!("0o{bits:o}"),
        IntStyle::Bin => format!("{bits:b}"),
    }
}

/// Format a Rust-only 64-bit field (`DBF_INT64` / `DBF_UINT64`). These
/// have no CA classic wire type (served as `Double` over CA), so they are
/// NOT subject to the 32-bit `sprint_long` truncation in
/// [`format_int_i64`] — the full 64-bit value is printed. The base shape
/// still matches `sprint_long` (`0x` UPPERCASE hex, `0o` octal, bare
/// binary) applied to the 64-bit pattern; `decimal` carries the
/// type-correct signed (`Int64`) or unsigned (`UInt64`) rendering.
fn format_int_wide(decimal: String, bits: u64, style: IntStyle) -> String {
    match style {
        IntStyle::Dec => decimal,
        IntStyle::Hex => format!("0x{bits:X}"),
        IntStyle::Oct => format!("0o{bits:o}"),
        IntStyle::Bin => format!("{bits:b}"),
    }
}

fn format_float(x: f64, fmt: &ValueFormat) -> String {
    if fmt.float_as_int {
        // C tool: round-half-to-even via `lroundl`, then format as int.
        let rounded = if x.is_nan() { 0i64 } else { x.round() as i64 };
        return format_int_i64(rounded, fmt.int_style);
    }
    if !x.is_finite() {
        // C printf prints "nan" / "inf" / "-inf"; Rust matches by
        // default (`{}` on f64 yields the same lowercase forms).
        return format!("{x}");
    }
    let p = fmt.float.precision as usize;
    match fmt.float.style {
        FloatStyle::F => format!("{x:.p$}"),
        FloatStyle::E => format_e(x, p),
        FloatStyle::G => format_g(x, p.max(1)),
    }
}

/// Decimal exponent of `abs` after rounding to `precision` significant
/// digits — i.e. `floor(log10(round_to_sig_digits(abs, precision)))`.
///
/// C `%g` rounds to `precision` significant digits FIRST and only then
/// decides between `%e` and `%f`. At a rounding boundary the rounded
/// magnitude can tick up by a power of ten (e.g. `999999.5` at
/// precision 6 rounds to `1000000`, exponent 5 → 6), which flips the
/// fixed-vs-scientific choice. Computing the decision exponent from the
/// UNROUNDED value misses that — see the regression test
/// `g_rounding_boundary_picks_scientific`.
fn decision_exponent(abs: f64, precision: usize) -> i32 {
    let raw_exp = abs.log10().floor() as i32;
    // Scale so the value has `precision` digits before the decimal
    // point, round half-to-even, and read back the magnitude. If the
    // round carries into a new decade the exponent increments.
    let scale = 10f64.powi(precision as i32 - 1 - raw_exp);
    // For magnitudes near the f64 range limits the scale factor can
    // overflow to ±inf (or underflow to 0); `abs * scale` then yields a
    // non-finite product whose `log10` saturates to a garbage exponent.
    // The rounded magnitude cannot meaningfully differ from the raw one
    // at those scales, so fall back to `raw_exp`.
    if !scale.is_finite() || scale == 0.0 {
        return raw_exp;
    }
    let rounded_scaled = (abs * scale).round();
    if !rounded_scaled.is_finite() || rounded_scaled <= 0.0 {
        return raw_exp;
    }
    raw_exp + (rounded_scaled.log10().floor() as i32 - (precision as i32 - 1))
}

/// `%g`-equivalent formatter. C semantics: choose `%e` or `%f`
/// depending on the exponent, drop trailing zeros and the trailing
/// decimal point. Precision is the *significant-digit* count.
fn format_g(x: f64, precision: usize) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    let abs = x.abs();
    // C `%g` rounds to `precision` significant digits before choosing
    // the format, so the decision exponent must come from the rounded
    // magnitude, not the raw value.
    let exp = decision_exponent(abs, precision);
    // C `%g` uses fixed-point when `precision > exp >= -4`. Compare as
    // i32 to avoid the silent `i32 → usize` wrap for negative `exp`.
    if exp >= -4 && exp < precision as i32 {
        // Fixed-point. Digits after the decimal point = precision-1-exp.
        let digits = (precision as i32 - 1 - exp).max(0) as usize;
        let s = format!("{x:.digits$}");
        trim_g_fixed(&s)
    } else {
        format_g_scientific(x, precision)
    }
}

fn trim_g_fixed(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_g_scientific(x: f64, precision: usize) -> String {
    // Rust `{:e}` precision means digits after the decimal in the
    // mantissa. C `%g` with N significant digits is mantissa
    // precision N-1.
    let s = format!("{:.*e}", precision - 1, x);
    rewrite_rust_e_as_c(&s, true)
}

/// Pure `%e`: precision is digits AFTER the decimal point.
fn format_e(x: f64, precision: usize) -> String {
    let s = format!("{x:.precision$e}");
    rewrite_rust_e_as_c(&s, false)
}

/// Rust formats scientific as `1.23e2`; C as `1.23e+02` (signed,
/// 2-digit exponent minimum). Optionally also strips the trailing
/// zeros from the mantissa (the `%g` post-trim behaviour).
fn rewrite_rust_e_as_c(s: &str, trim_mantissa: bool) -> String {
    let Some(e_pos) = s.find('e') else {
        return s.to_string();
    };
    let mantissa = &s[..e_pos];
    let exp_part = &s[e_pos + 1..];
    let mantissa_out = if trim_mantissa && mantissa.contains('.') {
        let t = mantissa.trim_end_matches('0').trim_end_matches('.');
        t.to_string()
    } else {
        mantissa.to_string()
    };
    let (sign, digits) = if let Some(d) = exp_part.strip_prefix('-') {
        ('-', d)
    } else if let Some(d) = exp_part.strip_prefix('+') {
        ('+', d)
    } else {
        ('+', exp_part)
    };
    let exp_padded = if digits.len() < 2 {
        format!("{sign}0{digits}")
    } else {
        format!("{sign}{digits}")
    };
    format!("{mantissa_out}e{exp_padded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_default() -> ValueFormat {
        ValueFormat::default()
    }

    #[test]
    fn g_default_precision_matches_c() {
        // C `printf("%g", 475.123)` → "475.123"
        assert_eq!(format_g(475.123, 6), "475.123");
        // C `printf("%g", 1.0)` → "1"
        assert_eq!(format_g(1.0, 6), "1");
        // C `printf("%g", 0.0)` → "0"
        assert_eq!(format_g(0.0, 6), "0");
        // C `printf("%g", 1e-5)` → "1e-05"
        assert_eq!(format_g(1e-5, 6), "1e-05");
        // C `printf("%g", 1e10)` → "1e+10"
        assert_eq!(format_g(1e10, 6), "1e+10");
        // C `printf("%g", 0.0001)` → "0.0001" (boundary)
        assert_eq!(format_g(0.0001, 6), "0.0001");
        // C `printf("%g", 1234567.0)` → "1.23457e+06"
        assert_eq!(format_g(1234567.0, 6), "1.23457e+06");
    }

    /// C `%g` rounds to `precision` significant digits BEFORE deciding
    /// between `%e` and `%f`. At a rounding boundary the rounded
    /// magnitude can carry into a new decade and flip the choice:
    /// `printf("%g", 999999.5)` → "1e+06" (not "1000000"), because the
    /// rounded value `1000000` has exponent 6 >= precision 6.
    #[test]
    fn g_rounding_boundary_picks_scientific() {
        // 999999.5 rounds up to 1000000 → exponent ticks 5 → 6 → %e.
        assert_eq!(format_g(999999.5, 6), "1e+06");
        // Just below the boundary stays fixed-point.
        assert_eq!(format_g(999998.0, 6), "999998");
        // 9.999995 rounds to 10 (exponent 0 → 1, still fixed range).
        assert_eq!(format_g(9.999995, 6), "10");
        // Negative value at the same boundary keeps the sign.
        assert_eq!(format_g(-999999.5, 6), "-1e+06");
    }

    #[test]
    fn g_extreme_magnitudes_do_not_produce_garbage() {
        // Magnitudes near the f64 range limits make the internal scale
        // factor overflow/underflow; decision_exponent must fall back
        // to the raw exponent instead of saturating to a garbage value.
        // Tiny: classifies as scientific (exp < -4).
        assert_eq!(format_g(1e-308, 6), "1e-308");
        assert_eq!(format_g(5e-300, 6), "5e-300");
        // Huge: classifies as scientific (exp >= precision).
        assert_eq!(format_g(1e308, 6), "1e+308");
        // Smallest normal f64 — no panic, scientific form.
        assert!(format_g(f64::MIN_POSITIVE, 6).contains("e-"));
    }

    #[test]
    fn e_format_matches_c() {
        // C `printf("%e", 1.5)` → "1.500000e+00"
        assert_eq!(format_e(1.5, 6), "1.500000e+00");
        // C `printf("%.2e", 1234.5)` → "1.23e+03"
        assert_eq!(format_e(1234.5, 2), "1.23e+03");
    }

    #[test]
    fn classic_int_base_format_matches_sprint_long() {
        // EPICS sprint_long (tool_lib.c:64-91) formats a 32-bit dbr_long_t.
        // -1: hex/oct/bin reinterpret the low 32 bits as unsigned; hex is
        // uppercase with `0x`, octal `0o`, binary BARE (no `0b`).
        assert_eq!(format_int_i64(-1, IntStyle::Hex), "0xFFFFFFFF");
        assert_eq!(format_int_i64(-1, IntStyle::Oct), "0o37777777777");
        assert_eq!(format_int_i64(-1, IntStyle::Bin), "1".repeat(32));
        assert_eq!(format_int_i64(-1, IntStyle::Dec), "-1");
        // C special-cases val == 0 in binary as a bare "0".
        assert_eq!(format_int_i64(0, IntStyle::Bin), "0");
        // Positive value: uppercase hex, `0o` octal, bare binary.
        assert_eq!(format_int_i64(1235, IntStyle::Hex), "0x4D3");
        assert_eq!(format_int_i64(1235, IntStyle::Oct), "0o2323");
        assert_eq!(format_int_i64(1235, IntStyle::Bin), "10011010011");
    }

    #[test]
    fn wide_int64_uint64_keep_64_bits_explicit() {
        // The Rust-only 64-bit extension is NOT truncated to 32 bits.
        let mut hex = fmt_default();
        hex.int_style = IntStyle::Hex;
        assert_eq!(
            format_value(&EpicsValue::Int64(-1), &hex, None, false),
            "0xFFFFFFFFFFFFFFFF",
            "Int64 -1 keeps the full 64-bit pattern, uppercase"
        );
        // UInt64 above i64::MAX prints its full unsigned decimal.
        assert_eq!(
            format_value(&EpicsValue::UInt64(u64::MAX), &fmt_default(), None, false),
            u64::MAX.to_string()
        );
        // UInt64 binary is bare (no `0b`).
        let mut bin = fmt_default();
        bin.int_style = IntStyle::Bin;
        assert_eq!(
            format_value(&EpicsValue::UInt64(5), &bin, None, false),
            "101"
        );
    }

    #[test]
    fn array_renders_count_then_values() {
        let v = EpicsValue::DoubleArray(vec![1.0, 2.5, 3.0]);
        let s = format_value(&v, &fmt_default(), None, false);
        // C: `3 1 2.5 3` (count + space-separated %g values)
        assert_eq!(s, "3 1 2.5 3");
    }

    /// C `caget.c:286` gates the count prefix on `reqElems || nElems > 1`.
    /// A genuine 1-element waveform read WITHOUT `-#` prints just the
    /// value, no `1 ` prefix.
    #[test]
    fn single_element_array_omits_count_without_req_elems() {
        let v = EpicsValue::DoubleArray(vec![2.5]);
        // No `-#` on the command line → no count prefix.
        assert_eq!(format_value(&v, &fmt_default(), None, false), "2.5");
        // `-#` supplied → count prefix returns even for 1 element.
        assert_eq!(format_value(&v, &fmt_default(), None, true), "1 2.5");
        // Multi-element always carries the count prefix.
        let v2 = EpicsValue::DoubleArray(vec![1.0, 2.5]);
        assert_eq!(format_value(&v2, &fmt_default(), None, false), "2 1 2.5");
    }

    #[test]
    fn enum_with_strings_renders_string() {
        let strs = vec!["off".to_string(), "on".to_string()];
        let v = EpicsValue::Enum(1);
        let s = format_value(&v, &fmt_default(), Some(&strs), false);
        assert_eq!(s, "on");
    }

    #[test]
    fn enum_n_flag_renders_index() {
        let strs = vec!["off".to_string(), "on".to_string()];
        let v = EpicsValue::Enum(1);
        let mut fmt = fmt_default();
        fmt.enum_as_number = true;
        let s = format_value(&v, &fmt, Some(&strs), false);
        assert_eq!(s, "1");
    }

    #[test]
    fn char_array_long_string_strips_at_nul() {
        let v = EpicsValue::CharArray(b"hello\0xxxx".to_vec());
        let mut fmt = fmt_default();
        fmt.char_array_as_string = true;
        assert_eq!(format_value(&v, &fmt, None, false), "hello");
    }

    #[test]
    fn cli_readback_escapes_raw_string_bytes() {
        // C val2str runs every DBR_STRING element through
        // epicsStrnEscapedFromRaw (tool_lib.c:135); caget -S escapes the
        // long-string byte prefix (caget.c:322-327). Control chars,
        // backslash, quotes and non-printable bytes escape; ASCII passes.
        assert_eq!(escape_from_raw(b"a\tb\nc"), "a\\tb\\nc");
        assert_eq!(escape_from_raw(b"a\\b\"c'd"), "a\\\\b\\\"c\\'d");
        assert_eq!(escape_from_raw(&[0x00, 0x01, b'A', 0x7f]), "\\0\\x01A\\x7f");
        // 'é' = UTF-8 0xC3 0xA9: each raw byte escapes as \xHH, like C.
        assert_eq!(escape_from_raw(&[0xc3, 0xa9]), "\\xc3\\xa9");
        // String scalar through format_value is escaped.
        assert_eq!(
            format_value(
                &EpicsValue::String("x\ty".into()),
                &fmt_default(),
                None,
                false
            ),
            "x\\ty"
        );
        // StringArray elements escaped; count prefix preserved.
        let a = EpicsValue::StringArray(vec!["a\nb".into(), "c".into()]);
        assert_eq!(format_value(&a, &fmt_default(), None, false), "2 a\\nb c");
        // `-S` long-string: escape the printable prefix up to NUL.
        let mut sfmt = fmt_default();
        sfmt.char_array_as_string = true;
        let cv = EpicsValue::CharArray(b"hi\tthere\0junk".to_vec());
        assert_eq!(format_value(&cv, &sfmt, None, true), "hi\\tthere");
    }

    #[test]
    fn float_as_int_rounds_then_renders() {
        let v = EpicsValue::Double(1234.6);
        let mut fmt = fmt_default();
        fmt.float_as_int = true;
        fmt.int_style = IntStyle::Hex;
        // 1235 = 0x4D3 (sprint_long uses uppercase %X)
        assert_eq!(format_value(&v, &fmt, None, false), "0x4D3");
    }

    #[test]
    fn pv_name_width_constant_is_30() {
        // Lock the pad width so a future tweak gets caught.
        assert_eq!(PV_NAME_WIDTH, 30);
    }

    /// `Duration::from_secs_f64` panics on NaN / +Inf / negative.
    /// `timeout_duration` must clamp those to the safe default rather
    /// than panic — the user-supplied value reaches us via clap which
    /// happily parses "NaN", "inf", "-1" as f64.
    #[test]
    fn timeout_duration_clamps_pathological_floats() {
        let d = timeout_duration(f64::NAN);
        assert_eq!(d.as_secs_f64(), DEFAULT_CLI_TIMEOUT_SECS);
        let d = timeout_duration(f64::INFINITY);
        assert_eq!(d.as_secs_f64(), DEFAULT_CLI_TIMEOUT_SECS);
        let d = timeout_duration(f64::NEG_INFINITY);
        assert_eq!(d.as_secs_f64(), DEFAULT_CLI_TIMEOUT_SECS);
        let d = timeout_duration(-1.0);
        assert_eq!(d.as_secs_f64(), DEFAULT_CLI_TIMEOUT_SECS);
    }

    /// Sane positive values pass through unchanged.
    #[test]
    fn timeout_duration_preserves_positive_finite() {
        let d = timeout_duration(2.5);
        assert!((d.as_secs_f64() - 2.5).abs() < 1e-9);
    }

    /// C `caget`/`caput`/`camonitor` pass `-w 0` straight to
    /// `ca_pend_io(0)` / `ca_pend_event(0)`, which wait forever. `-w 0`
    /// must therefore resolve to the far-future [`INDEFINITE_TIMEOUT`],
    /// NOT the 1 s default — the bug was clamping it to the default.
    #[test]
    fn timeout_zero_means_indefinite() {
        assert_eq!(timeout_duration(0.0), INDEFINITE_TIMEOUT);
        assert_eq!(timeout_duration(-0.0), INDEFINITE_TIMEOUT);
        // Effectively unbounded: far longer than any CLI session.
        assert!(INDEFINITE_TIMEOUT.as_secs() > 365 * 24 * 60 * 60);
    }

    /// `INDEFINITE_TIMEOUT` must be safe to arm a tokio timer with — a
    /// duration that overflowed `Instant` would panic when `timeout` /
    /// `sleep` computes its deadline. Proven by setting up a `timeout`
    /// with it (the inner future is already ready, so it returns at once).
    #[tokio::test]
    async fn indefinite_timeout_arms_tokio_timer_without_panic() {
        let r = tokio::time::timeout(INDEFINITE_TIMEOUT, async { 7 }).await;
        assert_eq!(r.unwrap(), 7);
    }

    /// Env-var path must reject the same pathological set so a
    /// misconfigured `EPICS_CLI_TIMEOUT` doesn't propagate. Serialised
    /// because env-var mutation races every other test that consults
    /// `EPICS_CLI_TIMEOUT` (none today, but #[serial] is cheap insurance).
    #[serial_test::serial]
    #[test]
    fn env_default_timeout_rejects_nan_inf() {
        // SAFETY: serial_test::serial guarantees no concurrent env access.
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "NaN") };
        assert_eq!(env_default_timeout(), DEFAULT_CLI_TIMEOUT_SECS);
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "inf") };
        assert_eq!(env_default_timeout(), DEFAULT_CLI_TIMEOUT_SECS);
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "-3") };
        assert_eq!(env_default_timeout(), DEFAULT_CLI_TIMEOUT_SECS);
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "2.5") };
        assert!((env_default_timeout() - 2.5).abs() < 1e-9);
        unsafe { std::env::remove_var("EPICS_CLI_TIMEOUT") };
    }

    /// C `use_ca_timeout_env` (tool_lib.c:646) sets `caTimeout` to any
    /// value `epicsScanDouble` accepts — including `0`, which then means
    /// "wait forever". `EPICS_CLI_TIMEOUT=0` must pass through as `0.0`
    /// and resolve to [`INDEFINITE_TIMEOUT`], not the 1 s default.
    #[serial_test::serial]
    #[test]
    fn env_zero_resolves_to_indefinite() {
        // SAFETY: serial_test::serial guarantees no concurrent env access.
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "0") };
        assert_eq!(env_default_timeout(), 0.0);
        assert_eq!(timeout_duration(env_default_timeout()), INDEFINITE_TIMEOUT);
        unsafe { std::env::remove_var("EPICS_CLI_TIMEOUT") };
    }

    #[test]
    fn max_elements_caps_array() {
        let v = EpicsValue::LongArray((0..10).collect());
        let mut fmt = fmt_default();
        fmt.max_elements = Some(3);
        // `-#` implies `req_elems_present` so the count prefix is present.
        let s = format_value(&v, &fmt, None, true);
        // Total count is full (10) per C `caget -# 3` behaviour:
        //   "10 0 1 2"
        assert_eq!(s, "10 0 1 2");
    }
}
