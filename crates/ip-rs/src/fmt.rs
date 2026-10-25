//! C `printf` of a format string that is data, not a literal: the record
//! link's `FMT=`. [`CFloat`] owns the conversion; this module parses the
//! format and applies its flags.

use epics_libcom_rs::cfmt::CFloat;

/// One `printf` conversion, as parsed out of a C format string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    zero: bool,
    alt: bool,
    width: usize,
    precision: Option<usize>,
    conversion: char,
}

/// The conversions that take a `double`, and those that take an `int`.
const DOUBLE_CONVERSIONS: &str = "fFeEgG";
const INT_CONVERSIONS: &str = "diuxXo";

/// Split a C format string into `(prefix, spec, suffix)`.
///
/// `devXxEurotherm.c` takes the payload format from the record link (`FMT=`,
/// default `%f`), so the format is data, not a literal: `SL%4.0lf` must produce
/// the same bytes C's `sprintf` would. Exactly one conversion is allowed and it
/// must be one of `conversions`, the ones that take the argument C passes;
/// any other is undefined behaviour there and is rejected here.
fn parse_format(format: &str, conversions: &str) -> Result<(String, Spec, String), String> {
    let mut prefix = String::new();
    let mut chars = format.chars().peekable();

    // Literal text, with "%%" standing for a literal '%'.
    loop {
        match chars.next() {
            None => return Err(format!("format {format:?} has no conversion")),
            Some('%') if chars.peek() == Some(&'%') => {
                chars.next();
                prefix.push('%');
            }
            Some('%') => break,
            Some(c) => prefix.push(c),
        }
    }

    let mut spec = Spec {
        left: false,
        plus: false,
        space: false,
        zero: false,
        alt: false,
        width: 0,
        precision: None,
        conversion: ' ',
    };
    while let Some(&c) = chars.peek() {
        match c {
            '-' => spec.left = true,
            '+' => spec.plus = true,
            ' ' => spec.space = true,
            '0' => spec.zero = true,
            '#' => spec.alt = true,
            _ => break,
        }
        chars.next();
    }
    while let Some(&c) = chars.peek() {
        let Some(digit) = c.to_digit(10) else { break };
        spec.width = spec.width * 10 + digit as usize;
        chars.next();
    }
    if chars.peek() == Some(&'.') {
        chars.next();
        let mut precision = 0;
        while let Some(&c) = chars.peek() {
            let Some(digit) = c.to_digit(10) else { break };
            precision = precision * 10 + digit as usize;
            chars.next();
        }
        spec.precision = Some(precision);
    }
    // Length modifiers: the C databases write "%4.0lf".
    while let Some(&c) = chars.peek() {
        if matches!(c, 'l' | 'L' | 'h') {
            chars.next();
        } else {
            break;
        }
    }
    spec.conversion = match chars.next() {
        Some(c) if conversions.contains(c) => c,
        Some(c) => {
            return Err(format!(
                "format {format:?} has %{c}, which takes another type"
            ));
        }
        None => return Err(format!("format {format:?} ends inside its conversion")),
    };

    let mut suffix = String::new();
    while let Some(c) = chars.next() {
        match c {
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                suffix.push('%');
            }
            '%' => return Err(format!("format {format:?} has a second conversion")),
            c => suffix.push(c),
        }
    }
    Ok((prefix, spec, suffix))
}

/// Format `value` with the C format string `format`, which must carry exactly
/// one `double` conversion (`%f`, `%e`, `%E`, `%g`, `%G`, with the usual flags,
/// width, precision and `l` modifier).
/// `sprintf` of a format with no conversion: its text, `%%` printed as `%`.
pub fn c_literal(format: &str) -> Option<String> {
    (!format.replace("%%", "").contains('%')).then(|| format.replace("%%", "%"))
}

pub fn format_c_double(format: &str, value: f64) -> Result<String, String> {
    let (prefix, spec, suffix) = parse_format(format, DOUBLE_CONVERSIONS)?;

    let magnitude = value.abs();
    let p = spec.precision.unwrap_or(6);
    let mut body = match (spec.conversion, spec.alt) {
        ('f' | 'F', false) => format!("{:.p$}", CFloat(magnitude)),
        ('f' | 'F', true) => format!("{:#.p$}", CFloat(magnitude)),
        ('e' | 'E', false) => format!("{:.p$e}", CFloat(magnitude)),
        ('e' | 'E', true) => format!("{:#.p$e}", CFloat(magnitude)),
        (_, false) => format!("{:.p$}", CFloat(magnitude).g()),
        (_, true) => format!("{:#.p$}", CFloat(magnitude).g()),
    };
    if spec.conversion.is_uppercase() {
        body = body.to_uppercase();
    }

    let sign = if value.is_sign_negative() {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    };

    let width = spec.width.saturating_sub(sign.len() + body.chars().count());
    let padded = if width == 0 {
        format!("{sign}{body}")
    } else if spec.left {
        format!("{sign}{body}{}", " ".repeat(width))
    } else if spec.zero && value.is_finite() {
        format!("{sign}{}{body}", "0".repeat(width))
    } else {
        format!("{}{sign}{body}", " ".repeat(width))
    };
    Ok(format!("{prefix}{padded}{suffix}"))
}

/// Format `value` with the C format string `format`, which must carry exactly
/// one `int` conversion (`%d`, `%i`, `%u`, `%x`, `%X`, `%o`, with the usual
/// flags, width, precision and length modifiers).
pub fn format_c_int(format: &str, value: i32) -> Result<String, String> {
    let (prefix, spec, suffix) = parse_format(format, INT_CONVERSIONS)?;
    let unsigned = value as u32;
    let (mut digits, sign) = match spec.conversion {
        'd' | 'i' => (
            value.unsigned_abs().to_string(),
            if value < 0 {
                "-"
            } else if spec.plus {
                "+"
            } else if spec.space {
                " "
            } else {
                ""
            },
        ),
        'u' => (unsigned.to_string(), ""),
        'x' => (format!("{unsigned:x}"), ""),
        'X' => (format!("{unsigned:X}"), ""),
        _ => (format!("{unsigned:o}"), ""),
    };
    // A precision is the minimum digit count, and `%.0d` of 0 prints nothing.
    if let Some(precision) = spec.precision {
        if precision == 0 && unsigned == 0 {
            digits.clear();
        } else if digits.len() < precision {
            digits = "0".repeat(precision - digits.len()) + &digits;
        }
    }
    let alt = match spec.conversion {
        'x' if spec.alt && unsigned != 0 => "0x",
        'X' if spec.alt && unsigned != 0 => "0X",
        'o' if spec.alt && !digits.starts_with('0') => "0",
        _ => "",
    };
    let head = format!("{sign}{alt}");
    let width = spec.width.saturating_sub(head.len() + digits.len());
    let padded = if width == 0 {
        format!("{head}{digits}")
    } else if spec.left {
        format!("{head}{digits}{}", " ".repeat(width))
    } else if spec.zero && spec.precision.is_none() {
        format!("{head}{}{digits}", "0".repeat(width))
    } else {
        format!("{}{head}{digits}", " ".repeat(width))
    };
    Ok(format!("{prefix}{padded}{suffix}"))
}

/// Read the leading number of `text` as C's `sscanf("%lf")` does: skip leading
/// whitespace, take the longest prefix that is a number, ignore the rest.
/// `None` when there is no number at all (C: the conversion fails).
pub fn scan_f64(text: &str) -> Option<f64> {
    let text = text.trim_start();
    let mut value = None;
    for (end, _) in text.char_indices().skip(1) {
        match text[..end].parse::<f64>() {
            Ok(parsed) => value = Some(parsed),
            // Keep going: "1e" does not parse but "1e-3" does.
            Err(_) => continue,
        }
    }
    text.parse::<f64>().ok().or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here is the output of the same `printf` call compiled
    /// with gcc and run on this host.
    #[test]
    fn matches_c_printf() {
        assert_eq!(format_c_double("%4.0lf", 123.4).unwrap(), " 123");
        assert_eq!(format_c_double("SL%4.0lf", 123.4).unwrap(), "SL 123");
        assert_eq!(format_c_double("%f", 1.5).unwrap(), "1.500000");
        assert_eq!(format_c_double("%f", -1.5).unwrap(), "-1.500000");
        assert_eq!(format_c_double("%8.2f", -1.5).unwrap(), "   -1.50");
        assert_eq!(format_c_double("%-8.2f", 1.5).unwrap(), "1.50    ");
        assert_eq!(format_c_double("%08.2f", -1.5).unwrap(), "-0001.50");
        assert_eq!(format_c_double("%+.1f", 1.5).unwrap(), "+1.5");
        assert_eq!(format_c_double("% .1f", 1.5).unwrap(), " 1.5");
        assert_eq!(format_c_double("%e", 1234.5).unwrap(), "1.234500e+03");
        assert_eq!(format_c_double("%.2E", -0.00012345).unwrap(), "-1.23E-04");
        assert_eq!(format_c_double("%g", 100.0).unwrap(), "100");
        assert_eq!(format_c_double("%g", 0.0001).unwrap(), "0.0001");
        assert_eq!(format_c_double("%g", 0.00001).unwrap(), "1e-05");
        assert_eq!(format_c_double("%g", 123456789.0).unwrap(), "1.23457e+08");
        assert_eq!(format_c_double("%g", 0.0).unwrap(), "0");
        assert_eq!(format_c_double("%.3g", 1234.0).unwrap(), "1.23e+03");
        assert_eq!(format_c_double("%#g", 100.0).unwrap(), "100.000");
        assert_eq!(format_c_double("%G", 0.00001).unwrap(), "1E-05");
        assert_eq!(format_c_double("%#.0f", 1.0).unwrap(), "1.");
        assert_eq!(format_c_double("%#.0e", 1.0).unwrap(), "1.e+00");
        assert_eq!(format_c_double("%f", f64::NAN).unwrap(), "nan");
        assert_eq!(format_c_double("%f", -f64::NAN).unwrap(), "-nan");
        assert_eq!(format_c_double("%8.2E", -f64::NAN).unwrap(), "    -NAN");
        assert_eq!(
            format_c_double("%08.2f", -f64::INFINITY).unwrap(),
            "    -inf"
        );
    }

    #[test]
    fn rejects_formats_that_do_not_take_one_double() {
        assert!(format_c_double("SL", 1.0).is_err());
        assert!(format_c_double("%d", 1.0).is_err());
        assert!(format_c_double("%f%f", 1.0).is_err());
        assert!(format_c_double("%", 1.0).is_err());
        // "%%" is a literal percent, not a conversion.
        assert!(format_c_double("100%%", 1.0).is_err());
        assert_eq!(format_c_double("%.0f%%", 12.0).unwrap(), "12%");
    }

    /// Each expectation is gcc's `printf` of the same call on this host.
    #[test]
    fn matches_c_printf_of_an_int() {
        let cases = [
            ("%d", -42, "-42"),
            ("%5d", 7, "    7"),
            ("%-5d|", 7, "7    |"),
            ("%05d", -42, "-0042"),
            ("%+d", 5, "+5"),
            ("% d", 5, " 5"),
            ("%.3d", 7, "007"),
            ("%.0d", 0, ""),
            ("%x", 255, "ff"),
            ("%#X", 255, "0XFF"),
            ("%o", 8, "10"),
            ("%#o", 8, "010"),
            ("%ld", 3, "3"),
            ("%u", -1, "4294967295"),
            ("SET%dX", 1, "SET1X"),
            ("%08.3d", 12, "     012"),
        ];
        for (format, value, want) in cases {
            assert_eq!(format_c_int(format, value).unwrap(), want, "{format}");
        }
        assert!(format_c_int("%f", 1).is_err());
    }

    #[test]
    fn scans_the_leading_number_like_sscanf() {
        assert_eq!(scan_f64("12.3456"), Some(12.3456));
        assert_eq!(scan_f64("  12.3456 mm"), Some(12.3456));
        assert_eq!(scan_f64("1.2E-06 Torr"), Some(1.2e-6));
        assert_eq!(scan_f64("-4.5"), Some(-4.5));
        assert_eq!(scan_f64("0.0"), Some(0.0));
        assert_eq!(scan_f64("mm"), None);
        assert_eq!(scan_f64(""), None);
    }

    #[test]
    fn matches_c_percent_e() {
        assert_eq!(format_c_double("%7.1E", 1.0e-6).unwrap(), "1.0E-06");
        assert_eq!(format_c_double("%7.1E", 5.5e-8).unwrap(), "5.5E-08");
        assert_eq!(format_c_double("%7.1E", 0.0).unwrap(), "0.0E+00");
        assert_eq!(format_c_double("%7.1E", 760.0).unwrap(), "7.6E+02");
        assert_eq!(format_c_double("%8.4E", 1.0e-6).unwrap(), "1.0000E-06");
        assert_eq!(format_c_double("%8.4E", 1.2345e-9).unwrap(), "1.2345E-09");
        // Three-digit exponents keep all their digits, as in C.
        assert_eq!(format_c_double("%7.1E", 1.0e-120).unwrap(), "1.0E-120");
    }
}
