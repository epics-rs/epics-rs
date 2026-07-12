//! C option-argument semantics for the CA command-line tools.
//!
//! The four C tools (`caget`, `camonitor`, `caput`, `cainfo`) parse their
//! options with `getopt(3)` and then hand each `optarg` to a *lenient*
//! scanner — `sscanf("%d")`, `sscanf("%u")`, `epicsScanDouble`, or a bare
//! `(char) *optarg`. None of those scanners can fail the program: a
//! malformed argument prints a warning on stderr and the tool CONTINUES
//! with a documented default (`caget.c:437-499`, and the identical blocks
//! in `camonitor.c`, `caput.c`, `cainfo.c`).
//!
//! This module is the single owner of that contract. Every option argument
//! the C tools scan enters the Rust binaries as an unvalidated `String` and
//! leaves through one of the resolvers here, which emit C's exact warning
//! text and C's exact fallback value. clap must never type-check such an
//! option: a `value_parser` on one of them re-introduces the hard exit-2
//! failure this module exists to remove, and it does so silently.

use crate::cli::IntStyle;

/// C `tool_lib.h:50` `DEFAULT_CA_PRIORITY`.
pub const DEFAULT_CA_PRIORITY: u8 = 0;
/// C `cadef.h:414` `CA_PRIORITY_MAX`.
pub const CA_PRIORITY_MAX: u8 = 99;
/// C `caget.c:42` / `camonitor.c:37` `VALID_DOUBLE_DIGITS` — the largest
/// precision `-e`/`-f`/`-g` will accept.
pub const VALID_DOUBLE_DIGITS: i32 = 18;

/// The digit run C's `%d`/`%u`/`%lu` conversions consume: leading
/// whitespace, an optional `+`/`-`, then decimal digits, stopping at the
/// first non-digit and IGNORING whatever trails (`"3x"` scans as `3`).
/// `None` when no digit was consumed — C's `sscanf(...) != 1`.
///
/// The magnitude is accumulated in 64 bits (C uses `strtoul`, which clamps
/// to `ULONG_MAX`); the caller narrows it to the width of its own format.
fn scan_digits(s: &str) -> Option<(bool, u64)> {
    let mut chars = s.trim_start().chars().peekable();
    let neg = match chars.peek() {
        Some('+') => {
            chars.next();
            false
        }
        Some('-') => {
            chars.next();
            true
        }
        _ => false,
    };
    let digits: String = chars.take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    Some((neg, digits.parse::<u64>().unwrap_or(u64::MAX)))
}

/// C `sscanf("%u")`: the magnitude truncates to 32 bits and a leading `-`
/// negates modulo 2^32. Probed on the compiled C (`sscanf.c` driver):
/// `"-1" → 4294967295`, `"99999999999" → 1215752191`, `"5000000000" →
/// 705032704`, `"3x" → 3`, `"abc" → no conversion`.
pub fn scan_u32(s: &str) -> Option<u32> {
    let (neg, mag) = scan_digits(s)?;
    let mag = mag as u32;
    Some(if neg { mag.wrapping_neg() } else { mag })
}

/// C `sscanf("%d")`: identical 32-bit truncation to [`scan_u32`], read as
/// signed. Probed: `"-3" → -3`, `"5000000000" → 705032704`.
pub fn scan_i32(s: &str) -> Option<i32> {
    scan_u32(s).map(|v| v as i32)
}

/// C `sscanf("%lu")` (`camonitor.c:447`, into an `unsigned long reqElems`):
/// 64-bit, so — unlike [`scan_u32`] — a large magnitude is NOT truncated.
/// Probed: `"-3" → 18446744073709551613`, `"99999999999" → 99999999999`.
pub fn scan_u64(s: &str) -> Option<u64> {
    let (neg, mag) = scan_digits(s)?;
    Some(if neg { mag.wrapping_neg() } else { mag })
}

/// C `epicsScanDouble` (`epicsStdlib.h:203`) = `!epicsParseDouble(str, to, NULL)`.
///
/// `epicsParseDouble` (`epicsStdlib.c`) skips leading whitespace, runs
/// `strtod`, skips trailing whitespace, and REJECTS any extraneous
/// character. So — unlike the digit scanners above — `"3x"` is a FAILURE
/// here, while `" 3 "` succeeds. `-w` is the only option that uses it.
pub fn scan_double(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

/// The tool's own name, used to stamp C's `('<tool> -h' for help.)` suffix
/// into every warning. Each binary constructs exactly one.
#[derive(Debug, Clone, Copy)]
pub struct CTool(&'static str);

impl CTool {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// C `-w`: `epicsScanDouble` into `caTimeout`, which is LEFT AT ITS
    /// CURRENT VALUE on a bad scan — that value being the `EPICS_CA_TIMEOUT`
    /// env default already loaded by `use_ca_timeout_env`, which is what the
    /// warning echoes back (`caget.c:437-443`).
    pub fn timeout(self, arg: Option<&str>, default: f64) -> f64 {
        let Some(a) = arg else { return default };
        match scan_double(a) {
            Some(v) => v,
            None => {
                eprintln!(
                    "'{a}' is not a valid timeout value - ignored, using '{default:.1}'. \
                     ('{tool} -h' for help.)",
                    tool = self.0
                );
                default
            }
        }
    }

    /// C `-#` in `caget`/`caput`: `sscanf("%d")` into an `int count`
    /// (`caget.c:447-453`) that is then WIDENED to the `unsigned long
    /// reqElems` parameter (`caget.c:557`), so a negative count sign-extends
    /// into a huge request which the `reqElems > nElems ? nElems : reqElems`
    /// clamp turns into "all elements" — while still counting as "requested"
    /// for the array count prefix. Observed: `caget -# -3 TST:LO` → `1 200`.
    ///
    /// A failed scan resets to `0`, which is C's ONLY encoding of "no `-#`"
    /// (`count = 0; /* 0 = not specified by -# option */`, `caget.c:386`).
    /// Returning a bare `u64` — not an `Option` — is what keeps that single
    /// meaning: there is no `Some(0)` that can drift away from `None`.
    pub fn req_elems_int(self, arg: Option<&str>) -> u64 {
        let Some(a) = arg else { return 0 };
        match scan_i32(a) {
            Some(v) => v as i64 as u64, // C's int → unsigned long widening
            None => {
                eprintln!(
                    "'{a}' is not a valid array element count - ignored. \
                     ('{tool} -h' for help.)",
                    tool = self.0
                );
                0
            }
        }
    }

    /// C `-#` in `camonitor`: `sscanf("%lu")` STRAIGHT into the `unsigned
    /// long reqElems` (`camonitor.c:445-452`) — no 32-bit hop, so a big
    /// count survives where `caget`'s `%d` would truncate it. Same `0` =
    /// "not specified" contract as [`CTool::req_elems_int`].
    pub fn req_elems_ulong(self, arg: Option<&str>) -> u64 {
        let Some(a) = arg else { return 0 };
        match scan_u64(a) {
            Some(v) => v,
            None => {
                eprintln!(
                    "'{a}' is not a valid array element count - ignored. \
                     ('{tool} -h' for help.)",
                    tool = self.0
                );
                0
            }
        }
    }

    /// C `-p`: `sscanf("%u")` into an `unsigned caPriority`, then
    /// `if (caPriority > CA_PRIORITY_MAX) caPriority = CA_PRIORITY_MAX`
    /// (`caget.c:455-462`). `%u` wraps `-1` to `UINT_MAX`, so a NEGATIVE
    /// priority is not an error in C — it clamps to 99, silently. Observed:
    /// `caget -p -1 TST:LO` and `caget -p 500 TST:LO` both read the PV with
    /// no diagnostic at all.
    pub fn priority(self, arg: Option<&str>) -> u8 {
        let Some(a) = arg else {
            return DEFAULT_CA_PRIORITY;
        };
        let raw = match scan_u32(a) {
            Some(v) => v,
            None => {
                eprintln!(
                    "'{a}' is not a valid CA priority - ignored. ('{tool} -h' for help.)",
                    tool = self.0
                );
                u32::from(DEFAULT_CA_PRIORITY)
            }
        };
        u8::try_from(raw)
            .unwrap_or(CA_PRIORITY_MAX)
            .min(CA_PRIORITY_MAX)
    }

    /// C `cainfo -s`: `sscanf("%u")` into `statLevel`, `0` on a bad scan
    /// (`cainfo.c:167-174`). Any non-zero level selects `ca_client_status`
    /// mode, so the `%u` wrap matters: `-s -1` is a non-zero level and DOES
    /// enter status mode.
    pub fn stat_level(self, arg: Option<&str>) -> u32 {
        let Some(a) = arg else { return 0 };
        match scan_u32(a) {
            Some(v) => v,
            None => {
                eprintln!(
                    "'{a}' is not a valid interest level - ignored. ('{tool} -h' for help.)",
                    tool = self.0
                );
                0
            }
        }
    }

    /// C `-e` / `-f` / `-g`: `sscanf("%d", &digits)` and then a range gate
    /// (`caget.c:470-484`). BOTH failures — an unscannable argument and an
    /// out-of-range digit count — warn and leave `dblFormatStr` at its
    /// default, so `None` here means "keep the default float format".
    /// The two messages are distinct and neither carries the `-h` suffix.
    ///
    /// `opt` is the option letter, which C interpolates into the message and
    /// also uses as the printf conversion (`sprintf(dblFormatStr,
    /// "%%-.%d%c", digits, opt)`).
    pub fn digits(self, opt: char, arg: &str) -> Option<u32> {
        let Some(d) = scan_i32(arg) else {
            eprintln!("Invalid precision argument '{arg}' for option '-{opt}' - ignored.");
            return None;
        };
        if (0..=VALID_DOUBLE_DIGITS).contains(&d) {
            Some(d as u32)
        } else {
            eprintln!("Precision {d} for option '-{opt}' out of range - ignored.");
            None
        }
    }

    /// C `-F`: `fieldSeparator = (char) *optarg` (`caget.c:505`) — the FIRST
    /// character of the argument, the rest discarded. Observed:
    /// `caget -F abc TST:LO` → `TST:LOa200`. An empty argument yields
    /// `'\0'`, which C then dutifully prints between elements.
    pub fn field_separator(self, arg: Option<&str>) -> Option<char> {
        let _ = self.0;
        arg.map(|a| a.chars().next().unwrap_or('\0'))
    }

    /// C `-0<base>` / `-l<base>`: `switch ((char) *optarg)` over `x`/`b`/`o`
    /// (`caget.c:486-499`, `camonitor.c:325-340`). These are single-dash
    /// getopt options that TAKE AN ARGUMENT (`"...l:#:d:0:w:..."`), not
    /// flags — `-0x` is option `0` with `optarg == "x"`.
    ///
    /// Two details the previous flag-shaped spelling could not express, both
    /// load-bearing and both observed on the compiled C `caget`:
    ///
    /// * only the FIRST character is read, so `-0xyz` is hex with no warning;
    /// * an invalid base warns but does NOT reset a base a previous
    ///   occurrence set — C guards the assignment with
    ///   `if (outType != dec)`, so `-0x -0q` warns and still prints hex.
    ///
    /// `occurrences` is every `-0` (or every `-l`) argument in command-line
    /// order. The return is C's `outTypeI` (`opt == '0'`) or `outTypeF`
    /// (`opt == 'l'`) after the whole getopt loop.
    pub fn base(self, opt: char, occurrences: &[String]) -> IntStyle {
        let _ = self.0;
        let mut style = IntStyle::Dec;
        for a in occurrences {
            match a.chars().next() {
                Some('x') => style = IntStyle::Hex,
                Some('b') => style = IntStyle::Bin,
                Some('o') => style = IntStyle::Oct,
                _ => eprintln!("Invalid argument '{a}' for option '-{opt}' - ignored."),
            }
        }
        style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAGET: CTool = CTool::new("caget");

    /// Observed on the compiled C `caget` (EPICS 7.0.10.1-DEV, linux-x86_64):
    ///   `caget -0x -0q PV` → warns, still prints `0xC8`
    ///   `caget -0xyz PV`   → prints `0xC8`, no warning
    ///   `caget -0q PV`     → warns, prints `200`
    #[test]
    fn invalid_base_does_not_reset_a_valid_one() {
        assert_eq!(
            CAGET.base('0', &["x".into(), "q".into()]),
            IntStyle::Hex,
            "C guards the assignment with `if (outType != dec)`"
        );
        assert_eq!(CAGET.base('0', &["xyz".into()]), IntStyle::Hex);
        assert_eq!(CAGET.base('0', &["q".into()]), IntStyle::Dec);
        assert_eq!(CAGET.base('0', &["".into()]), IntStyle::Dec);
        assert_eq!(CAGET.base('0', &[]), IntStyle::Dec);
        assert_eq!(CAGET.base('0', &["x".into(), "b".into()]), IntStyle::Bin);
    }

    /// The three C conversions differ, and the difference is observable.
    /// Probed with a gcc driver against glibc:
    ///   "-3"          %d → -3          %u → 4294967293  %lu → 2^64-3
    ///   "5000000000"  %d → 705032704   %u → 705032704   %lu → 5000000000
    ///   "3x"          all → 3 (trailing garbage ignored)
    ///   "abc"         all → no conversion
    #[test]
    fn digit_scanners_match_glibc_sscanf() {
        assert_eq!(scan_i32("-3"), Some(-3));
        assert_eq!(scan_u32("-3"), Some(4_294_967_293));
        assert_eq!(scan_u64("-3"), Some(u64::MAX - 2));

        assert_eq!(scan_i32("5000000000"), Some(705_032_704));
        assert_eq!(scan_u32("5000000000"), Some(705_032_704));
        assert_eq!(scan_u64("5000000000"), Some(5_000_000_000));

        assert_eq!(scan_u32("99999999999"), Some(1_215_752_191));
        assert_eq!(scan_u64("99999999999"), Some(99_999_999_999));

        for s in ["3x", "  3  ", "+3"] {
            assert_eq!(scan_i32(s), Some(3), "{s}");
        }
        for s in ["abc", "", "-", "+", "x3"] {
            assert_eq!(scan_i32(s), None, "{s}");
            assert_eq!(scan_u64(s), None, "{s}");
        }
    }

    /// `epicsParseDouble` rejects extraneous characters — the ONE scanner
    /// that is stricter than `sscanf`.
    #[test]
    fn scan_double_matches_epics_parse_double() {
        assert_eq!(scan_double(" 2.5 "), Some(2.5));
        assert_eq!(
            scan_double("3x"),
            None,
            "epicsParseDouble: S_stdlib_extraneous"
        );
        assert_eq!(scan_double("abc"), None);
        assert_eq!(scan_double(""), None);
    }

    /// Observed on the compiled C `caget`: `-p -1` and `-p 500` both clamp
    /// to 99 with NO diagnostic; only an unscannable argument warns.
    #[test]
    fn priority_wraps_then_clamps_like_c() {
        assert_eq!(CAGET.priority(Some("3")), 3);
        assert_eq!(CAGET.priority(Some("99")), 99);
        assert_eq!(CAGET.priority(Some("500")), 99);
        assert_eq!(
            CAGET.priority(Some("-1")),
            99,
            "%u wraps, then the clamp fires"
        );
        assert_eq!(CAGET.priority(Some("abc")), 0);
        assert_eq!(CAGET.priority(None), 0);
    }

    /// C's `-#` has exactly ONE "not specified" value: `0`. A failed scan
    /// resets to it, and a negative count sign-extends into "all elements"
    /// while still reading as "requested".
    #[test]
    fn req_elems_has_a_single_unspecified_value() {
        assert_eq!(CAGET.req_elems_int(None), 0);
        assert_eq!(CAGET.req_elems_int(Some("0")), 0, "-# 0 IS 'not specified'");
        assert_eq!(CAGET.req_elems_int(Some("abc")), 0);
        assert_eq!(CAGET.req_elems_int(Some("3")), 3);
        assert_eq!(CAGET.req_elems_int(Some("3x")), 3);
        assert_eq!(CAGET.req_elems_int(Some("-3")), u64::MAX - 2);

        // camonitor's %lu keeps 64 bits where caget's %d truncates.
        let cam = CTool::new("camonitor");
        assert_eq!(cam.req_elems_ulong(Some("5000000000")), 5_000_000_000);
        assert_eq!(CAGET.req_elems_int(Some("5000000000")), 705_032_704);
    }

    /// Any non-zero level selects `ca_client_status` mode, so the `%u` wrap
    /// on `-s -1` is load-bearing (`cainfo.c:167-174`).
    #[test]
    fn stat_level_wraps_like_sscanf_u() {
        assert_eq!(CAGET.stat_level(Some("10")), 10);
        assert_eq!(CAGET.stat_level(Some("-1")), 4_294_967_295);
        assert_eq!(CAGET.stat_level(Some("+3abc")), 3);
        assert_eq!(CAGET.stat_level(Some("abc")), 0);
        assert_eq!(CAGET.stat_level(None), 0);
    }

    /// Observed on the compiled C `caget`:
    ///   `-e 99` → "Precision 99 for option '-e' out of range - ignored."
    ///   `-e -2` → same, with -2
    ///   `-e 3x` → precision 3, NO warning
    ///   `-e abc`→ "Invalid precision argument 'abc' for option '-e' - ignored."
    /// All four still read the PV.
    #[test]
    fn digits_gates_on_c_range() {
        assert_eq!(CAGET.digits('e', "3"), Some(3));
        assert_eq!(CAGET.digits('e', "3x"), Some(3));
        assert_eq!(CAGET.digits('e', "0"), Some(0));
        assert_eq!(CAGET.digits('e', "18"), Some(18), "VALID_DOUBLE_DIGITS");
        assert_eq!(CAGET.digits('e', "19"), None);
        assert_eq!(CAGET.digits('e', "-2"), None);
        assert_eq!(CAGET.digits('f', "abc"), None);
    }

    /// C takes `(char) *optarg` — the first byte, rest discarded.
    /// Observed: `caget -F abc TST:LO` → `TST:LOa200`.
    #[test]
    fn field_separator_is_the_first_char() {
        assert_eq!(CAGET.field_separator(Some("abc")), Some('a'));
        assert_eq!(CAGET.field_separator(Some(",")), Some(','));
        assert_eq!(CAGET.field_separator(Some("")), Some('\0'));
        assert_eq!(CAGET.field_separator(None), None);
    }

    /// A bad `-w` leaves the env/default timeout in place and the tool RUNS.
    #[test]
    fn timeout_keeps_the_default_on_a_bad_scan() {
        assert_eq!(CAGET.timeout(Some("2.5"), 1.0), 2.5);
        assert_eq!(CAGET.timeout(Some(" 2.5 "), 1.0), 2.5);
        assert_eq!(CAGET.timeout(Some("abc"), 1.0), 1.0);
        assert_eq!(CAGET.timeout(Some("3x"), 1.0), 1.0);
        assert_eq!(CAGET.timeout(None, 1.0), 1.0);
    }
}
