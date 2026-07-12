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

/// The tool's own name, used to stamp C's `('<tool> -h' for help.)` suffix
/// into every warning. Each binary constructs exactly one.
#[derive(Debug, Clone, Copy)]
pub struct CTool(&'static str);

impl CTool {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
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
}
