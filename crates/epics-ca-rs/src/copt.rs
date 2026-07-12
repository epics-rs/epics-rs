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
//!
//! # Repeatability (R13-17)
//!
//! `getopt(3)` has NO notion of "this option was already used": it hands the
//! loop one `(opt, optarg)` pair per occurrence, and the loop body simply
//! overwrites the variable it owns. So EVERY C option is repeatable and the
//! LAST occurrence wins — `caget -w 5 -w 2 PV` waits 2 s, `caget -t -t PV`
//! warns and prints terse. clap's default actions (`Set`/`SetTrue`) are the
//! opposite: a repeat is a hard error with clap's multi-line usage block,
//! which no C tool ever prints.
//!
//! [`CTool::get_matches`] therefore REFUSES to run a command whose options
//! are not all declared `ArgAction::Append` (value options) or
//! `ArgAction::Count` (flags) — see [`assert_repeatable`]. The check fires on
//! every invocation, so a newly declared option cannot silently re-open the
//! family; and the resolvers here take the whole occurrence list, so
//! "last wins" is the only shape a caller can express.

use crate::cli::IntStyle;

/// C's "last occurrence wins" for a value-taking option: the getopt loop
/// overwrites its variable on each pass, so the final assignment is the one
/// that survives (`caget.c:398-505` and the identical loops in the other
/// three tools). `None` is "the option was never given", which is what every
/// resolver in this module treats as C's untouched default.
pub fn last(occurrences: &[String]) -> Option<&str> {
    occurrences.last().map(String::as_str)
}

/// Every option a C tool declares must survive a repeat, because `getopt(3)`
/// cannot fail one. This walks the clap spec and rejects any option that
/// would: a `Set`/`SetTrue` action makes the second occurrence a clap usage
/// error, which is a divergence no test of the option's *value* can catch.
///
/// It panics rather than warns because the failure is a DECLARATION bug, not
/// a user input: every binary and every `cli_*` integration test runs
/// [`CTool::get_matches`], so the panic surfaces the moment the bad option is
/// added. Positionals are exempt (C's `argv` tail is not a getopt option).
fn assert_repeatable(cmd: &clap::Command) {
    use clap::ArgAction;
    for a in cmd.get_arguments() {
        if a.is_positional() {
            continue;
        }
        match a.get_action() {
            // Repeatable: the resolvers here fold the occurrence list.
            ArgAction::Append | ArgAction::Count => {}
            // clap's own help/version actions terminate the parse, exactly as
            // C's `case 'h'` / `case 'V'` return from `main`; a repeat of
            // either is harmless.
            ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version => {}
            other => panic!(
                "option '{id}' is declared with {other:?}, so a repeat would be a clap usage \
                 error; C's getopt accepts every option any number of times (last wins). \
                 Declare it `action = clap::ArgAction::Append` (value option, `Vec<String>`) \
                 or `action = clap::ArgAction::Count` (flag, `u8`) — see epics_ca_rs::copt.",
                id = a.get_id()
            ),
        }
    }
}

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

/// The outcome of C's `-0<base>` / `-l<base>` getopt case: the base that
/// survived the loop, and WHICH occurrence put it there.
///
/// The two travel together because C ties them together — the same
/// `if (outType != dec)` guard that assigns `outTypeI` also assigns
/// `type = DBR_LONG` (`caget.c:493-495`). A caller that wants to know when
/// `-0` last touched `type` must therefore ask about the last VALID
/// occurrence, never the last occurrence; keeping the answer inside the fold
/// that decides validity is what stops the caller from asking the wrong
/// question (R13-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Base {
    /// C's `outTypeI` / `outTypeF` after the whole getopt loop.
    pub style: IntStyle,
    /// Ordinal, within the occurrence list, of the last occurrence that
    /// scanned VALID. `None` when none did — C's guard then never ran.
    valid: Option<usize>,
}

impl Base {
    /// The clap argument index of the occurrence that last assigned, i.e. the
    /// getopt position at which `-0` last wrote `type` — what a competing
    /// option (`caget -d`) must be compared against. `None` when no
    /// occurrence scanned valid, because C never wrote `type` at all.
    ///
    /// `id` must be the clap id whose occurrences produced this `Base`;
    /// clap yields `indices_of` in command-line order, the same order the
    /// fold walked.
    pub fn valid_index(self, matches: &clap::ArgMatches, id: &str) -> Option<usize> {
        let n = self.valid?;
        matches.indices_of(id)?.nth(n)
    }
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
    /// env default already loaded by `use_ca_timeout_env`, or whatever an
    /// EARLIER `-w` set. The warning echoes that surviving value back
    /// (`caget.c:437-443`), so `caget -w 5 -w abc` warns `using '5.0'` and
    /// waits 5 s.
    pub fn timeout(self, occurrences: &[String], default: f64) -> f64 {
        let mut t = default;
        for a in occurrences {
            match scan_double(a) {
                Some(v) => t = v,
                None => eprintln!(
                    "'{a}' is not a valid timeout value - ignored, using '{t:.1}'. \
                     ('{tool} -h' for help.)",
                    tool = self.0
                ),
            }
        }
        t
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
    ///
    /// A repeat re-runs the whole case, so a bad `-#` after a good one RESETS
    /// the count to 0 (unlike `-w`, which keeps its previous value).
    pub fn req_elems_int(self, occurrences: &[String]) -> u64 {
        let mut count = 0u64;
        for a in occurrences {
            match scan_i32(a) {
                Some(v) => count = v as i64 as u64, // C's int → unsigned long widening
                None => {
                    eprintln!(
                        "'{a}' is not a valid array element count - ignored. \
                         ('{tool} -h' for help.)",
                        tool = self.0
                    );
                    count = 0;
                }
            }
        }
        count
    }

    /// C `-#` in `camonitor`: `sscanf("%lu")` STRAIGHT into the `unsigned
    /// long reqElems` (`camonitor.c:445-452`) — no 32-bit hop, so a big
    /// count survives where `caget`'s `%d` would truncate it. Same `0` =
    /// "not specified" contract, and the same reset-on-bad-repeat rule, as
    /// [`CTool::req_elems_int`].
    pub fn req_elems_ulong(self, occurrences: &[String]) -> u64 {
        let mut count = 0u64;
        for a in occurrences {
            match scan_u64(a) {
                Some(v) => count = v,
                None => {
                    eprintln!(
                        "'{a}' is not a valid array element count - ignored. \
                         ('{tool} -h' for help.)",
                        tool = self.0
                    );
                    count = 0;
                }
            }
        }
        count
    }

    /// C `-p`: `sscanf("%u")` into an `unsigned caPriority`, then
    /// `if (caPriority > CA_PRIORITY_MAX) caPriority = CA_PRIORITY_MAX`
    /// (`caget.c:455-462`). `%u` wraps `-1` to `UINT_MAX`, so a NEGATIVE
    /// priority is not an error in C — it clamps to 99, silently. Observed:
    /// `caget -p -1 TST:LO` and `caget -p 500 TST:LO` both read the PV with
    /// no diagnostic at all.
    ///
    /// The clamp lives INSIDE the case, so it re-runs per occurrence; a bad
    /// `-p` after a good one resets to `DEFAULT_CA_PRIORITY`.
    pub fn priority(self, occurrences: &[String]) -> u8 {
        let mut prio = DEFAULT_CA_PRIORITY;
        for a in occurrences {
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
            prio = u8::try_from(raw)
                .unwrap_or(CA_PRIORITY_MAX)
                .min(CA_PRIORITY_MAX);
        }
        prio
    }

    /// C `cainfo -s`: `sscanf("%u")` into `statLevel`, `0` on a bad scan
    /// (`cainfo.c:167-174`). Any non-zero level selects `ca_client_status`
    /// mode, so the `%u` wrap matters: `-s -1` is a non-zero level and DOES
    /// enter status mode.
    pub fn stat_level(self, occurrences: &[String]) -> u32 {
        let mut level = 0u32;
        for a in occurrences {
            match scan_u32(a) {
                Some(v) => level = v,
                None => {
                    eprintln!(
                        "'{a}' is not a valid interest level - ignored. ('{tool} -h' for help.)",
                        tool = self.0
                    );
                    level = 0;
                }
            }
        }
        level
    }

    /// One `-e` / `-f` / `-g` occurrence: `sscanf("%d", &digits)` and then a
    /// range gate (`caget.c:470-484`). BOTH failures — an unscannable argument
    /// and an out-of-range digit count — warn and leave `dblFormatStr` at its
    /// current value, so `None` means "this occurrence changes nothing". The
    /// two messages are distinct and neither carries the `-h` suffix.
    ///
    /// `opt` is the option letter, which C interpolates into the message and
    /// also uses as the printf conversion (`sprintf(dblFormatStr,
    /// "%%-.%d%c", digits, opt)`).
    fn digits(self, opt: char, arg: &str) -> Option<u32> {
        let _ = self.0;
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

    /// C `-e` / `-f` / `-g` (W10-B2): the three letters share ONE
    /// `dblFormatStr`, and the `sprintf` that rewrites it sits in the VALID
    /// branch of a single getopt case (`caget.c:470-484`, `camonitor.c:310-324`
    /// — note `case 'e': case 'f': case 'g':` all fall into the same body).
    /// So the effective format is the LAST VALID occurrence *in command-line
    /// order across all three letters* — there is no `e` > `f` > `g`
    /// precedence, and an invalid occurrence never clears an earlier valid one:
    ///
    /// ```text
    /// caget -e 2 -f 4 TST:AO   C: 1.5000       (f is last)
    /// caget -f 4 -g 2 TST:AO   C: 1.5          (g is last)
    /// caget -f 4 -e 99 TST:AO  C: 1.5000       (the invalid -e changes nothing)
    /// ```
    ///
    /// Every occurrence is scanned, so each bad one emits its own warning in
    /// order. `opts` is `(letter, clap id)` for the three; the return is the
    /// winning `(letter, precision)`, or `None` to keep the default format.
    ///
    /// Taking the occurrences from `ArgMatches` rather than from three
    /// separate `Vec<String>` fields is what makes the ORDER recoverable — a
    /// per-field resolver cannot see that `-f` came after `-e`.
    pub fn float_precision(
        self,
        matches: &clap::ArgMatches,
        opts: &[(char, &str)],
    ) -> Option<(char, u32)> {
        let mut events: Vec<(usize, char, &str)> = Vec::new();
        for &(letter, id) in opts {
            if let (Some(idx), Some(vals)) =
                (matches.indices_of(id), matches.get_many::<String>(id))
            {
                events.extend(idx.zip(vals).map(|(i, v)| (i, letter, v.as_str())));
            }
        }
        events.sort_by_key(|&(i, _, _)| i);
        let mut chosen = None;
        for (_, letter, arg) in events {
            if let Some(d) = self.digits(letter, arg) {
                chosen = Some((letter, d));
            }
        }
        chosen
    }

    /// C `-F`: `fieldSeparator = (char) *optarg` (`caget.c:505`) — the FIRST
    /// character of the argument, the rest discarded. Observed:
    /// `caget -F abc TST:LO` → `TST:LOa200`. An empty argument yields
    /// `'\0'`, which C then dutifully prints between elements. The assignment
    /// cannot fail, so a repeat is a plain last-wins overwrite.
    pub fn field_separator(self, occurrences: &[String]) -> Option<char> {
        let _ = self.0;
        last(occurrences).map(|a| a.chars().next().unwrap_or('\0'))
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
    /// order. The return carries C's `outTypeI` (`opt == '0'`) / `outTypeF`
    /// (`opt == 'l'`) after the whole getopt loop, PLUS which occurrence last
    /// performed the assignment — see [`Base::valid_index`].
    pub fn base(self, opt: char, occurrences: &[String]) -> Base {
        let _ = self.0;
        let mut base = Base {
            style: IntStyle::Dec,
            valid: None,
        };
        for (n, a) in occurrences.iter().enumerate() {
            base.style = match a.chars().next() {
                Some('x') => IntStyle::Hex,
                Some('b') => IntStyle::Bin,
                Some('o') => IntStyle::Oct,
                // C's `default:` arm sets `outType = dec`, and the two
                // assignments below it are guarded by `if (outType != dec)` —
                // so an invalid base warns, assigns NOTHING, and leaves both
                // the base and `type` as an earlier occurrence left them.
                _ => {
                    eprintln!("Invalid argument '{a}' for option '-{opt}' - ignored.");
                    continue;
                }
            };
            base.valid = Some(n);
        }
        base
    }

    /// C's usage-error contract, shared by all four tools: ONE line on
    /// stderr, `<what>. ('<tool> -h' for help.)`, and `return 1` from
    /// `main` (`caget.c:527-531`, `camonitor.c:604-608`, `caput.c:457-465`,
    /// `cainfo.c:202-205`, plus the getopt `'?'`/`':'` cases). No C CA tool
    /// exits 2, and none dumps its usage block on an error.
    pub fn usage_error(self, what: &str) -> ! {
        eprintln!("{what}. ('{tool} -h' for help.)", tool = self.0);
        std::process::exit(1)
    }

    /// C `if (nPvs < 1)` after the getopt loop. The C tools have NO required
    /// positional — getopt parses, then `main` validates — so the Rust
    /// binaries must not let clap's `required` fire either.
    pub fn no_pv_name(self) -> ! {
        self.usage_error("No pv name specified")
    }

    /// C `caput.c:462-465` `if (nPvs < 2)`.
    pub fn no_value(self) -> ! {
        self.usage_error("No value specified")
    }

    /// Parse `argv` through clap, but answer a *usage error* the way C's
    /// getopt loop does. `-h`/`-V` still print and exit 0 (clap owns those);
    /// every error path exits 1 with C's diagnostic, never clap's exit 2.
    ///
    /// This is the only entry point the binaries use, so a new option cannot
    /// re-introduce an exit-2 path by being declared through the derive — nor
    /// a non-repeatable option, which [`assert_repeatable`] rejects here.
    pub fn get_matches(self, cmd: clap::Command) -> clap::ArgMatches {
        assert_repeatable(&cmd);
        let spec = cmd.clone();
        match cmd.try_get_matches() {
            Ok(m) => m,
            Err(e) => self.usage_exit(&spec, e),
        }
    }

    fn usage_exit(self, spec: &clap::Command, e: clap::Error) -> ! {
        use clap::error::{ContextKind, ErrorKind};

        match e.kind() {
            // Not errors: clap prints these on stdout and exits 0, exactly
            // like C's `usage()` / `-V` paths.
            ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => e.exit(),

            // getopt `'?'`: `"Unrecognized option: '-%c'."` The token is
            // echoed as the user typed it (C echoes `optopt`, which for a
            // long-form token it never supports collapses to `'--'`).
            ErrorKind::UnknownArgument => {
                let arg = context_arg(&e, ContextKind::InvalidArg);
                self.usage_error(&format!("Unrecognized option: '{arg}'"))
            }

            // getopt `':'`: `"Option '-%c' requires an argument."` clap
            // reports the offending arg in its long form (`--wait <TIMEOUT>`)
            // regardless of how it was typed, so resolve back to C's short
            // letter through the command spec.
            ErrorKind::InvalidValue
                if e.get(ContextKind::InvalidValue).map(|v| v.to_string())
                    == Some(String::new()) =>
            {
                let arg = context_arg(&e, ContextKind::InvalidArg);
                let flag = short_flag_of(spec, &arg);
                self.usage_error(&format!("Option '{flag}' requires an argument"))
            }

            // No other usage error exists in C. Keep clap's message (we have
            // nothing truer to say) but not its exit code: 2 is not a status
            // any CA tool returns.
            _ => {
                let _ = e.print();
                std::process::exit(1)
            }
        }
    }
}

/// The raw token clap blamed, e.g. `-X` or `--wait <TIMEOUT>`.
fn context_arg(e: &clap::Error, kind: clap::error::ContextKind) -> String {
    e.get(kind).map(|v| v.to_string()).unwrap_or_default()
}

/// `--wait <TIMEOUT>` → `-w`. Falls back to the long form when the option is
/// a Rust-only extension with no short letter.
fn short_flag_of(spec: &clap::Command, blamed: &str) -> String {
    let long = blamed.split_whitespace().next().unwrap_or(blamed);
    let name = long.trim_start_matches('-');
    spec.get_arguments()
        .find(|a| a.get_long() == Some(name) || a.get_id().as_str() == name)
        .and_then(|a| a.get_short())
        .map_or_else(|| long.to_string(), |c| format!("-{c}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAGET: CTool = CTool::new("caget");

    /// One occurrence of an option, the shape every resolver now takes.
    fn one(s: &str) -> [String; 1] {
        [s.to_string()]
    }

    fn many(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Observed on the compiled C `caget` (EPICS 7.0.10.1-DEV, linux-x86_64):
    ///   `caget -0x -0q PV` → warns, still prints `0xC8`
    ///   `caget -0xyz PV`   → prints `0xC8`, no warning
    ///   `caget -0q PV`     → warns, prints `200`
    #[test]
    fn invalid_base_does_not_reset_a_valid_one() {
        let base = |v: &[&str]| CAGET.base('0', &many(v));
        assert_eq!(
            base(&["x", "q"]).style,
            IntStyle::Hex,
            "C guards the assignment with `if (outType != dec)`"
        );
        assert_eq!(base(&["xyz"]).style, IntStyle::Hex);
        assert_eq!(base(&["q"]).style, IntStyle::Dec);
        assert_eq!(base(&[""]).style, IntStyle::Dec);
        assert_eq!(base(&[]).style, IntStyle::Dec);
        assert_eq!(base(&["x", "b"]).style, IntStyle::Bin);

        // R13-16: the SAME guard decides which occurrence last assigned, so
        // the fold reports it — a trailing invalid base is not "the last one".
        assert_eq!(base(&["x", "q"]).valid, Some(0), "`q` assigned nothing");
        assert_eq!(base(&["x", "b"]).valid, Some(1));
        assert_eq!(base(&["q"]).valid, None);
        assert_eq!(base(&[]).valid, None);
        assert_eq!(base(&["q", "x"]).valid, Some(1));
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
        assert_eq!(CAGET.priority(&one("3")), 3);
        assert_eq!(CAGET.priority(&one("99")), 99);
        assert_eq!(CAGET.priority(&one("500")), 99);
        assert_eq!(
            CAGET.priority(&one("-1")),
            99,
            "%u wraps, then the clamp fires"
        );
        assert_eq!(CAGET.priority(&one("abc")), 0);
        assert_eq!(CAGET.priority(&[]), 0);
    }

    /// C's `-#` has exactly ONE "not specified" value: `0`. A failed scan
    /// resets to it, and a negative count sign-extends into "all elements"
    /// while still reading as "requested".
    #[test]
    fn req_elems_has_a_single_unspecified_value() {
        assert_eq!(CAGET.req_elems_int(&[]), 0);
        assert_eq!(CAGET.req_elems_int(&one("0")), 0, "-# 0 IS 'not specified'");
        assert_eq!(CAGET.req_elems_int(&one("abc")), 0);
        assert_eq!(CAGET.req_elems_int(&one("3")), 3);
        assert_eq!(CAGET.req_elems_int(&one("3x")), 3);
        assert_eq!(CAGET.req_elems_int(&one("-3")), u64::MAX - 2);

        // camonitor's %lu keeps 64 bits where caget's %d truncates.
        let cam = CTool::new("camonitor");
        assert_eq!(cam.req_elems_ulong(&one("5000000000")), 5_000_000_000);
        assert_eq!(CAGET.req_elems_int(&one("5000000000")), 705_032_704);
    }

    /// Any non-zero level selects `ca_client_status` mode, so the `%u` wrap
    /// on `-s -1` is load-bearing (`cainfo.c:167-174`).
    #[test]
    fn stat_level_wraps_like_sscanf_u() {
        assert_eq!(CAGET.stat_level(&one("10")), 10);
        assert_eq!(CAGET.stat_level(&one("-1")), 4_294_967_295);
        assert_eq!(CAGET.stat_level(&one("+3abc")), 3);
        assert_eq!(CAGET.stat_level(&one("abc")), 0);
        assert_eq!(CAGET.stat_level(&[]), 0);
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

    /// The `-e`/`-f`/`-g` resolver, exercised through a clap `Command` shaped
    /// like the tools' (three `Append` options), so the command-line ORDER the
    /// C rule turns on is real rather than simulated.
    fn precision_of(argv: &[&str]) -> Option<(char, u32)> {
        let opt = |id: &'static str, c: char| {
            clap::Arg::new(id)
                .short(c)
                .action(clap::ArgAction::Append)
                .allow_hyphen_values(true)
        };
        let m = clap::Command::new("caget")
            .arg(opt("fmt_e", 'e'))
            .arg(opt("fmt_f", 'f'))
            .arg(opt("fmt_g", 'g'))
            .arg(clap::Arg::new("pv").num_args(0..))
            .get_matches_from(argv);
        CAGET.float_precision(&m, &[('e', "fmt_e"), ('f', "fmt_f"), ('g', "fmt_g")])
    }

    /// W10-B2. C's `case 'e': case 'f': case 'g':` share ONE body writing ONE
    /// `dblFormatStr` (`caget.c:470-484`), so the LAST VALID occurrence in
    /// command-line order wins — ACROSS the three letters, not just within
    /// one. Observed on the compiled C `caget` against `TST:AO` (VAL=1.5):
    ///   `-e 2 -f 4` → `1.5000`      `-e 5 -g 2`  → `1.5`
    ///   `-f 4 -g 2` → `1.5`         `-f 4 -e 99` → `1.5000`
    #[test]
    fn float_precision_is_the_last_valid_occurrence_in_getopt_order() {
        assert_eq!(precision_of(&["caget", "-e", "2"]), Some(('e', 2)));
        assert_eq!(precision_of(&["caget"]), None, "no -e/-f/-g → the default");

        assert_eq!(
            precision_of(&["caget", "-e", "2", "-f", "4"]),
            Some(('f', 4)),
            "-f is last; there is no e > f precedence"
        );
        assert_eq!(
            precision_of(&["caget", "-e", "5", "-g", "2"]),
            Some(('g', 2))
        );
        assert_eq!(
            precision_of(&["caget", "-f", "4", "-g", "2"]),
            Some(('g', 2))
        );
        assert_eq!(
            precision_of(&["caget", "-g", "2", "-e", "5"]),
            Some(('e', 5)),
            "and no g > e precedence either"
        );

        // An INVALID occurrence never reaches the sprintf, so it cannot clear
        // an earlier valid one — whatever letter it carries.
        assert_eq!(
            precision_of(&["caget", "-f", "4", "-e", "99"]),
            Some(('f', 4)),
            "out of range"
        );
        assert_eq!(
            precision_of(&["caget", "-f", "4", "-g", "abc"]),
            Some(('f', 4)),
            "unscannable"
        );

        // Repeats within one letter fold the same way (R13-17).
        assert_eq!(
            precision_of(&["caget", "-e", "2", "-e", "6"]),
            Some(('e', 6))
        );
        assert_eq!(
            precision_of(&["caget", "-e", "2", "-f", "4", "-e", "6"]),
            Some(('e', 6)),
            "the last valid one wins no matter how the letters interleave"
        );
    }

    /// C takes `(char) *optarg` — the first byte, rest discarded.
    /// Observed: `caget -F abc TST:LO` → `TST:LOa200`.
    #[test]
    fn field_separator_is_the_first_char() {
        assert_eq!(CAGET.field_separator(&one("abc")), Some('a'));
        assert_eq!(CAGET.field_separator(&one(",")), Some(','));
        assert_eq!(CAGET.field_separator(&one("")), Some('\0'));
        assert_eq!(CAGET.field_separator(&[]), None);
    }

    /// A bad `-w` leaves the env/default timeout in place and the tool RUNS.
    #[test]
    fn timeout_keeps_the_default_on_a_bad_scan() {
        assert_eq!(CAGET.timeout(&one("2.5"), 1.0), 2.5);
        assert_eq!(CAGET.timeout(&one(" 2.5 "), 1.0), 2.5);
        assert_eq!(CAGET.timeout(&one("abc"), 1.0), 1.0);
        assert_eq!(CAGET.timeout(&one("3x"), 1.0), 1.0);
        assert_eq!(CAGET.timeout(&[], 1.0), 1.0);
    }

    /// R13-17. `getopt(3)` re-enters the same `case` on every occurrence, so
    /// a repeat is legal and the loop body decides what survives. The four
    /// C bodies do NOT agree on that, and each difference is observable:
    ///
    /// * `-w` (`caget.c:437-443`): a bad scan leaves `caTimeout` alone, so a
    ///   good value SURVIVES a later bad one.
    /// * `-#` (`:447-453`) and `-p` (`:455-462`): a bad scan RESETS to the
    ///   documented default (`0` / `DEFAULT_CA_PRIORITY`).
    /// * `-e`/`-f`/`-g` (`:470-484`): only the valid branch rewrites
    ///   `dblFormatStr`, so the last VALID occurrence wins.
    /// * `-F` (`:505`): a bare assignment — plain last-wins.
    #[test]
    fn a_repeated_option_folds_the_way_its_c_case_does() {
        assert_eq!(CAGET.timeout(&many(&["5", "2"]), 1.0), 2.0, "last wins");
        assert_eq!(
            CAGET.timeout(&many(&["5", "abc"]), 1.0),
            5.0,
            "caTimeout is untouched by a bad scan"
        );

        assert_eq!(CAGET.req_elems_int(&many(&["2", "3"])), 3);
        assert_eq!(
            CAGET.req_elems_int(&many(&["3", "abc"])),
            0,
            "a bad -# resets count to 0"
        );

        assert_eq!(CAGET.priority(&many(&["1", "2"])), 2);
        assert_eq!(
            CAGET.priority(&many(&["5", "abc"])),
            DEFAULT_CA_PRIORITY,
            "a bad -p resets to DEFAULT_CA_PRIORITY"
        );

        assert_eq!(CAGET.field_separator(&many(&[",", ";"])), Some(';'));
        assert_eq!(CAGET.stat_level(&many(&["1", "2"])), 2);
        assert_eq!(CAGET.stat_level(&many(&["1", "abc"])), 0);
    }

    /// R13-17's structural guard: a value option declared with clap's default
    /// `Set` action makes `caget -w 5 -w 2` a usage error, which C's getopt
    /// cannot produce. `get_matches` must refuse to run such a spec rather
    /// than let the divergence ship.
    #[test]
    #[should_panic(expected = "C's getopt accepts every option any number of times")]
    fn a_non_repeatable_option_is_rejected_at_the_spec() {
        let cmd = clap::Command::new("caget").arg(
            clap::Arg::new("wait")
                .short('w')
                .action(clap::ArgAction::Set),
        );
        assert_repeatable(&cmd);
    }

    /// The shapes a C option IS allowed to take.
    #[test]
    fn append_and_count_options_are_accepted() {
        let cmd = clap::Command::new("caget")
            .arg(
                clap::Arg::new("wait")
                    .short('w')
                    .action(clap::ArgAction::Append),
            )
            .arg(
                clap::Arg::new("terse")
                    .short('t')
                    .action(clap::ArgAction::Count),
            )
            .arg(clap::Arg::new("pv").num_args(0..));
        assert_repeatable(&cmd); // must not panic
    }
}
