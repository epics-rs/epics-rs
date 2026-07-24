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
//! `ArgAction::Count` (flags) — see `assert_repeatable`. The check fires on
//! every invocation, so a newly declared option cannot silently re-open the
//! family; and the resolvers here take the whole occurrence list, so
//! "last wins" is the only shape a caller can express.
//!
//! # Order (R13-26)
//!
//! getopt processes options STRICTLY in command-line order, and `case 'h'` /
//! `case 'V'` `return` from `main` the moment the loop reaches them. Two
//! consequences C gets for free and a clap program does not:
//!
//! * a warning from an option BEFORE `-h`/`-V` is already on stderr when the
//!   usage block prints — `caget -w abc -h` warns, then prints usage;
//! * an option AFTER `-h`/`-V` is never scanned, so it never warns.
//!
//! clap has no such loop: it parses everything at once, and its own
//! Help/Version actions terminate the process before any resolver here runs —
//! which is why `-h`/`-V` used to SWALLOW every warning.
//!
//! [`Scan`] is C's loop. Every resolver reads its occurrences from
//! [`clap::ArgMatches`], so every warning it raises carries the argv position
//! getopt would have processed it at; the scan buffers them, and [`Scan::finish`]
//! — the single owner of the loop's exit — replays them in that position order
//! and only up to the first terminal option, then performs it. `-h` and `-V`
//! are therefore ORDINARY repeatable options in every tool's spec
//! (`assert_repeatable` now rejects clap's Help/Version actions outright), and
//! no resolver may write to stderr directly.
//!
//! The loop's THIRD exit is the getopt error — `case '?'` / `case ':'` — and it
//! is a case like any other: it stands at a position, the options before it
//! have already warned, the ones after it are never scanned. clap cannot
//! express that (it fails the whole parse and hands back no `ArgMatches`), so
//! [`CTool::get_matches`] cuts the command line where getopt cuts it and
//! returns a [`Parsed`] over the prefix, carrying the diagnostic for
//! [`Scan::finish`] to print in its place (R14-18).
//!
//! # Long-form options (DELIBERATE deviation — kept by user decision, wave 11)
//!
//! C parses SHORT options only: `getopt(3)` treats `caget --wait 5` as the
//! unknown option `'-'` and answers with the `'?'` arm's diagnostic (the
//! echoed `optopt` collapses the token to `'--'` — see
//! [`CTool::get_matches`]'s UnknownArgument arm). The Rust binaries
//! additionally declare a clap long form per option (`--wait`, `--terse`,
//! `--dbr-type`, ...). This is a deliberate SUPERSET: every command line C
//! accepts parses identically here (short letters, argv order,
//! repeatability, warning replay), and the long forms only admit command
//! lines C would refuse — no C-valid invocation changes meaning. The
//! accepted cost is one-way portability: a script written against the Rust
//! long forms does not run on the C tools.

use std::io::Write as _;

use crate::cli::IntStyle;

/// C's "last occurrence wins" for a value-taking option: the getopt loop
/// overwrites its variable on each pass, so the final assignment is the one
/// that survives (`caget.c:398-505` and the identical loops in the other
/// three tools). `None` is "the option was never given", which is what every
/// resolver in this module treats as C's untouched default.
pub fn last(occurrences: &[String]) -> Option<&str> {
    occurrences.last().map(String::as_str)
}

/// Every option a C tool declares must survive two things `getopt(3)` cannot
/// fail: a repeat, and an option-argument that begins with `-`. This walks the
/// clap spec and rejects any option that would.
///
/// A `Set`/`SetTrue` action makes the second occurrence a clap usage error. A
/// value option without `allow_hyphen_values` makes `-s -1` one: clap reads the
/// `-1` as an unknown option and exits 2, where `getopt` hands the next `argv`
/// entry over verbatim, `-` and all, and the `case` arm decides what it means
/// (`cainfo.c:167-172` scans `-1` with `%u` and gets a status level;
/// `caget.c:486-494` and `camonitor.c:285-300` reject the string with a warning
/// and carry on). Either way the tool must reach its own resolver, so clap may
/// never type-check an option's argument — that job belongs to this module.
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
        let takes_value = matches!(a.get_action(), ArgAction::Set | ArgAction::Append);
        if takes_value && !a.is_allow_hyphen_values_set() {
            panic!(
                "option '{id}' takes a value but is not declared \
                 `allow_hyphen_values = true`, so clap would reject an argument that begins \
                 with '-' before the tool ever sees it; C's getopt passes it through. Declare \
                 it `allow_hyphen_values = true` and let the resolver in epics_ca_rs::copt \
                 decide whether the value is valid.",
                id = a.get_id()
            );
        }
        match a.get_action() {
            // Repeatable: the resolvers here fold the occurrence list.
            ArgAction::Append | ArgAction::Count => {}
            // clap's Help/Version actions are rejected too, and not only for
            // repeatability: they terminate the PARSE, which happens before any
            // resolver runs, so a tool that declares them loses every warning
            // C would have printed ahead of `-h`/`-V` (R13-26). `-h` and `-V`
            // are `Count` options like any other, and [`Scan::finish`] performs
            // them at their place in the getopt order.
            other => panic!(
                "option '{id}' is declared with {other:?}, so a repeat would be a clap usage \
                 error (and a Help/Version action would terminate the parse before the option \
                 warnings C prints first); C's getopt accepts every option any number of times \
                 (last wins). Declare it `action = clap::ArgAction::Append` (value option, \
                 `Vec<String>`) or `action = clap::ArgAction::Count` (flag, `u8`), and give the \
                 command `disable_help_flag`/`disable_version_flag` — see epics_ca_rs::copt.",
                id = a.get_id()
            ),
        }
    }
}

/// A getopt case that RETURNS from C's `main`, ending the option loop where it
/// stands: `case 'h'`, `case 'V'`, and `cainfo`'s `default:` (`cainfo.c:196-198`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// C `usage()` then `return <status>`. `caget.c:399-401` returns 0; the
    /// `default:` arm that a declared-but-unhandled option letter falls into
    /// returns 1.
    ///
    /// The block goes to STDERR at either status, because that is the only
    /// stream C's `usage()` ever writes: all four tools open it with
    /// `fprintf (stderr, "\nUsage: ...")` (`caget.c:55-58`, `camonitor.c:45-47`,
    /// `caput.c:60-62`, `cainfo.c:37-39`), and the `-h` case just calls that
    /// same function. One block, one stream, both statuses — the port used to
    /// split it, sending the exit-0 half to stdout because that is where clap
    /// puts help. The block's WORDING is still clap's, not C's — a standing,
    /// separate divergence. `-V` keeps stdout: C prints the version banner with
    /// `printf` (`caget.c:403`).
    Usage(i32),
    /// C `case 'V'`: the version banner on stdout, `return 0`.
    Version,
}

/// C's getopt loop: the ordered scan of one tool's options.
///
/// Holds the parsed `ArgMatches` and the warnings raised so far, each tagged
/// with the argv position of the occurrence that raised it. Nothing here
/// writes to stderr — [`Scan::finish`] does, once, in position order. That is
/// what makes the two order-dependent behaviours of C's loop reproducible:
/// warnings precede the `-h`/`-V` output, and warnings from options AFTER
/// `-h`/`-V` never appear at all (R13-26).
pub struct Scan<'m> {
    tool: CTool,
    matches: &'m clap::ArgMatches,
    warnings: Vec<(usize, String)>,
    /// The `'?'` / `':'` diagnostic the loop is heading for, if the command
    /// line has one ([`Parsed`]) — already rendered, newline included. It ends
    /// the loop at the offending token, AFTER every option before it, which is
    /// why it cannot be printed at parse time (R14-18).
    error: Option<&'m str>,
}

impl<'m> Scan<'m> {
    /// Every occurrence of one option, in command-line order, each with the
    /// argv position getopt would have seen it at. This is the ONLY way a
    /// resolver reads an option: a `Vec<String>` from the derive has the
    /// values but not the positions, and without the positions a warning
    /// cannot be ordered against the rest of the loop.
    pub fn occurrences(&self, id: &str) -> Vec<(usize, &'m str)> {
        match (
            self.matches.indices_of(id),
            self.matches.get_many::<String>(id),
        ) {
            (Some(idx), Some(vals)) => idx.zip(vals).map(|(i, v)| (i, v.as_str())).collect(),
            _ => Vec::new(),
        }
    }

    /// Was this option actually on the command line?
    ///
    /// `indices_of` alone cannot answer that: clap gives an ABSENT `Count`
    /// flag its `0` default AND an index, so an `indices_of`-only test reads
    /// every unused flag as present — which made `-h` look given on every
    /// invocation. The value SOURCE is the gate.
    fn was_given(&self, id: &str) -> bool {
        matches!(
            self.matches.value_source(id),
            Some(clap::parser::ValueSource::CommandLine)
        )
    }

    /// Position of the LAST occurrence of an option, or `None` if never
    /// given — C's "which of these two options came last" question.
    pub fn last_index(&self, id: &str) -> Option<usize> {
        if !self.was_given(id) {
            return None;
        }
        self.matches.indices_of(id)?.next_back()
    }

    /// How many times a `Count` flag was given. C's getopt loop re-runs the
    /// case body once per occurrence, so a flag's repeat count is observable
    /// (`caget -t -t` warns once about the mutual exclusion).
    pub fn count(&self, id: &str) -> u8 {
        self.matches.get_count(id)
    }

    /// Raise one of C's getopt-loop warnings, at the argv position of the
    /// occurrence that caused it. Public because a few warnings belong to a
    /// tool rather than to a shared scanner (`caget`'s `Options t,d,a are
    /// mutually exclusive`, `camonitor`'s `-m` / `-t`) — but they go through
    /// the same buffer, so they land in the same order C prints them.
    pub fn warn(&mut self, at: usize, message: String) {
        self.warnings.push((at, message));
    }

    /// The tool's name, for warnings a binary formats itself.
    pub fn tool(&self) -> CTool {
        self.tool
    }

    /// The warnings raised so far, in the order [`Scan::finish`] would print
    /// them — i.e. C's getopt order. Exposed so a tool's tests can pin that
    /// order without spawning the binary; `finish` is the only thing that
    /// prints them.
    pub fn ordered_warnings(&self) -> Vec<String> {
        let mut w = self.warnings.clone();
        w.sort_by_key(|&(i, _)| i);
        w.into_iter().map(|(_, m)| m).collect()
    }

    /// The first terminal option on the command line, if any — the point C's
    /// loop returns from `main`.
    fn terminal(&self, terminals: &[(&str, Terminal)]) -> Option<(usize, Terminal)> {
        terminals
            .iter()
            .filter(|&&(id, _)| self.was_given(id))
            .filter_map(|&(id, t)| self.matches.indices_of(id)?.next().map(|i| (i, t)))
            .min_by_key(|&(i, _)| i)
    }

    /// End C's getopt loop.
    ///
    /// Prints every buffered warning in argv order — but only those the loop
    /// would have REACHED, i.e. positioned before the first terminal option —
    /// and then performs that terminal option and exits. With no terminal
    /// option the whole buffer prints and the caller carries on into the
    /// post-loop checks (`nPvs < 1`, …), exactly where C goes next.
    ///
    /// A getopt ERROR (`'?'` / `':'`) is a third way out, and it is performed
    /// here for the same reason the other two are: it ends the loop where the
    /// offending token stands, so every warning BEFORE that token has already
    /// printed (`caget -w abc -X` is two stderr lines in C, the `-w` warning
    /// and the `-X` diagnostic) and every option after it is never scanned.
    /// [`Parsed`] holds the error back for exactly this moment; an `-h` that
    /// precedes the offending token still wins, because C's loop reaches it
    /// first and `return`s 0 (R14-18).
    ///
    /// `cmd` renders the usage block; `version_info` is the tool's `-V` banner.
    pub fn finish(self, cmd: &clap::Command, version_info: &str, terminals: &[(&str, Terminal)]) {
        let terminal = self.terminal(terminals);
        // The error sits at the offending token, which is past every option in
        // `matches` — [`Parsed`] parsed only the argv PREFIX before it — so any
        // terminal option there is came first and takes the loop's exit.
        let cutoff = terminal.map(|(i, _)| i);

        let mut warnings = self.warnings;
        warnings.sort_by_key(|&(i, _)| i);
        // One `write_all`, in getopt order: the loop's stderr, replayed.
        let mut out = String::new();
        for (i, w) in warnings {
            if cutoff.is_some_and(|c| i > c) {
                continue;
            }
            out.push_str(&w);
            out.push('\n');
        }
        if !out.is_empty() {
            let _ = std::io::stderr().write_all(out.as_bytes());
        }

        match terminal {
            None => {
                if let Some(diagnostic) = self.error {
                    // C's `case '?'` / `case ':'`: the one line, then `return 1`.
                    let _ = std::io::stderr().write_all(diagnostic.as_bytes());
                    std::process::exit(1)
                }
            }
            Some((_, Terminal::Version)) => {
                println!("{version_info}");
                std::process::exit(0)
            }
            Some((_, Terminal::Usage(status))) => {
                // C's `usage()` writes to stderr whatever brought it here —
                // `-h`, or the `default:` arm's exit-1 path.
                let _ = cmd.clone().write_help(&mut std::io::stderr());
                std::process::exit(status)
            }
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
/// `epicsParseDouble` (`epicsStdlib.c:149-176`) skips leading whitespace,
/// runs `strtod`, skips trailing whitespace, and REJECTS any extraneous
/// character. So — unlike the digit scanners above — `"3x"` is a FAILURE
/// here, while `" 3 "` succeeds. `-w` is the only option that uses it.
///
/// It is `strtod`, not `str::parse::<f64>()`, and the two differ at both
/// ends: `strtod` ACCEPTS C99 hex floats (`caget -w 0x10` is a 16 s
/// timeout in C; the port used to reject it, fall back to 1 s, and print a
/// spurious warning) and REJECTS `ERANGE` (`caget -w 1e400` warns "not a
/// valid timeout" in C; the port used to accept the infinity and swallow it
/// downstream). Both come from the shared [`crate::estdlib`] core, which
/// is also what every env-derived double goes through.
pub fn scan_double(s: &str) -> Option<f64> {
    crate::estdlib::epics_scan_double(s)
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
    /// The getopt position at which this base last ASSIGNED, i.e. where C last
    /// ran `if (outType != dec) { ...; type = DBR_LONG; }` — what a competing
    /// option (`caget -d`) must be compared against. `None` when no occurrence
    /// scanned valid, because C never wrote `type` at all.
    ///
    /// A position, not an ordinal into the occurrence list: the caller's
    /// question is "which option wrote `type` last", and only positions answer
    /// it across two different options.
    pub valid_at: Option<usize>,
}

/// The tool's own name, used to stamp C's `('<tool> -h' for help.)` suffix
/// into every warning. Each binary constructs exactly one.
#[derive(Debug, Clone, Copy)]
pub struct CTool(&'static str);

impl CTool {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// Begin C's getopt loop over an already-parsed command line. Every option
    /// this tool scans must be resolved through the returned [`Scan`], so that
    /// every warning it raises is ordered against the rest of the loop.
    ///
    /// A command line with a getopt ERROR in it must be scanned through
    /// [`Parsed::scan`] instead — that is what carries the error into
    /// [`Scan::finish`]. This entry point is for a command line that has none
    /// (the crate's own tests build their `ArgMatches` directly).
    pub fn scan(self, matches: &clap::ArgMatches) -> Scan<'_> {
        Scan {
            tool: self,
            matches,
            warnings: Vec::new(),
            error: None,
        }
    }
}

/// One tool's command line after clap, and the getopt error it is heading for.
///
/// C's `'?'` / `':'` cases are ordinary arms of the loop: getopt hands the loop
/// the options BEFORE the offending token first — each scanned, each warning
/// printed — and only then the error, which prints its own line and `return`s 1
/// (`caget.c:509-518`). clap has no loop: it fails the whole parse at the
/// offending token and yields no `ArgMatches` at all, so the port used to exit
/// on the error with the preceding warnings never raised (`caget -w abc -X`
/// printed ONE line where C prints two — R14-18).
///
/// So the parse is cut where getopt cuts it: on a usage error, the longest argv
/// PREFIX that parses is what the loop reached, and it is parsed on its own.
/// The tool then runs its resolvers over that prefix exactly as it would over a
/// clean command line — same code path, no replay of a second kind — and
/// [`Scan::finish`], the single owner of the loop's exit, performs the error
/// after the warnings. An option after the offending token is not in the
/// prefix, so it is never scanned and never warns, which is also C
/// (`caget -X -w abc` prints only the `-X` line).
pub struct Parsed {
    tool: CTool,
    matches: clap::ArgMatches,
    /// C's diagnostic for the offending token, rendered at parse time and
    /// PRINTED by [`Scan::finish`] — never before the warnings that precede it.
    error: Option<String>,
}

impl Parsed {
    /// The parsed options — of the whole command line, or of the prefix before
    /// the offending token when there is a getopt error.
    pub fn matches(&self) -> &clap::ArgMatches {
        &self.matches
    }

    /// Begin the getopt loop over this command line, carrying its error (if
    /// any) to [`Scan::finish`].
    pub fn scan(&self) -> Scan<'_> {
        Scan {
            tool: self.tool,
            matches: &self.matches,
            warnings: Vec::new(),
            error: self.error.as_deref(),
        }
    }
}

impl Scan<'_> {
    /// C `-w`: `epicsScanDouble` into `caTimeout`, which is LEFT AT ITS
    /// CURRENT VALUE on a bad scan — that value being the `EPICS_CA_TIMEOUT`
    /// env default already loaded by `use_ca_timeout_env`, or whatever an
    /// EARLIER `-w` set. The warning echoes that surviving value back
    /// (`caget.c:437-443`), so `caget -w 5 -w abc` warns `using '5.0'` and
    /// waits 5 s.
    pub fn timeout(&mut self, id: &str, default: f64) -> f64 {
        let mut t = default;
        for (at, a) in self.occurrences(id) {
            match scan_double(a) {
                Some(v) => t = v,
                None => self.warn(
                    at,
                    format!(
                        "'{a}' is not a valid timeout value - ignored, using '{t:.1}'. \
                         ('{tool} -h' for help.)",
                        tool = self.tool.0
                    ),
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
    pub fn req_elems_int(&mut self, id: &str) -> u64 {
        let mut count = 0u64;
        for (at, a) in self.occurrences(id) {
            match scan_i32(a) {
                Some(v) => count = v as i64 as u64, // C's int → unsigned long widening
                None => {
                    self.warn(
                        at,
                        format!(
                            "'{a}' is not a valid array element count - ignored. \
                             ('{tool} -h' for help.)",
                            tool = self.tool.0
                        ),
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
    /// [`Scan::req_elems_int`].
    pub fn req_elems_ulong(&mut self, id: &str) -> u64 {
        let mut count = 0u64;
        for (at, a) in self.occurrences(id) {
            match scan_u64(a) {
                Some(v) => count = v,
                None => {
                    self.warn(
                        at,
                        format!(
                            "'{a}' is not a valid array element count - ignored. \
                             ('{tool} -h' for help.)",
                            tool = self.tool.0
                        ),
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
    pub fn priority(&mut self, id: &str) -> u8 {
        let mut prio = DEFAULT_CA_PRIORITY;
        for (at, a) in self.occurrences(id) {
            let raw = match scan_u32(a) {
                Some(v) => v,
                None => {
                    self.warn(
                        at,
                        format!(
                            "'{a}' is not a valid CA priority - ignored. ('{tool} -h' for help.)",
                            tool = self.tool.0
                        ),
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
    pub fn stat_level(&mut self, id: &str) -> u32 {
        let mut level = 0u32;
        for (at, a) in self.occurrences(id) {
            match scan_u32(a) {
                Some(v) => level = v,
                None => {
                    self.warn(
                        at,
                        format!(
                            "'{a}' is not a valid interest level - ignored. \
                             ('{tool} -h' for help.)",
                            tool = self.tool.0
                        ),
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
    fn digits(&mut self, at: usize, opt: char, arg: &str) -> Option<u32> {
        let Some(d) = scan_i32(arg) else {
            self.warn(
                at,
                format!("Invalid precision argument '{arg}' for option '-{opt}' - ignored."),
            );
            return None;
        };
        if (0..=VALID_DOUBLE_DIGITS).contains(&d) {
            Some(d as u32)
        } else {
            self.warn(
                at,
                format!("Precision {d} for option '-{opt}' out of range - ignored."),
            );
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
    pub fn float_precision(&mut self, opts: &[(char, &str)]) -> Option<(char, u32)> {
        let mut events: Vec<(usize, char, String)> = Vec::new();
        for &(letter, id) in opts {
            events.extend(
                self.occurrences(id)
                    .into_iter()
                    .map(|(i, v)| (i, letter, v.to_string())),
            );
        }
        events.sort_by_key(|&(i, _, _)| i);
        let mut chosen = None;
        for (at, letter, arg) in events {
            if let Some(d) = self.digits(at, letter, &arg) {
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
    pub fn field_separator(&self, id: &str) -> Option<char> {
        self.occurrences(id)
            .last()
            .map(|(_, a)| a.chars().next().unwrap_or('\0'))
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
    /// `id` is the clap id of `-0` (or of `-l`). The return carries C's
    /// `outTypeI` (`opt == '0'`) / `outTypeF` (`opt == 'l'`) after the whole
    /// getopt loop, PLUS the position at which it last performed the
    /// assignment — see [`Base::valid_at`].
    pub fn base(&mut self, opt: char, id: &str) -> Base {
        let mut base = Base {
            style: IntStyle::Dec,
            valid_at: None,
        };
        for (at, a) in self.occurrences(id) {
            base.style = match a.chars().next() {
                Some('x') => IntStyle::Hex,
                Some('b') => IntStyle::Bin,
                Some('o') => IntStyle::Oct,
                // C's `default:` arm sets `outType = dec`, and the two
                // assignments below it are guarded by `if (outType != dec)` —
                // so an invalid base warns, assigns NOTHING, and leaves both
                // the base and `type` as an earlier occurrence left them.
                _ => {
                    self.warn(
                        at,
                        format!("Invalid argument '{a}' for option '-{opt}' - ignored."),
                    );
                    continue;
                }
            };
            base.valid_at = Some(at);
        }
        base
    }
}

impl CTool {
    /// C's usage-error contract, shared by all four tools: ONE line on
    /// stderr, `<what>. ('<tool> -h' for help.)`, and `return 1` from
    /// `main` (`caget.c:527-531`, `camonitor.c:604-608`, `caput.c:457-465`,
    /// `cainfo.c:202-205`, plus the getopt `'?'`/`':'` cases). No C CA tool
    /// exits 2, and none dumps its usage block on an error.
    pub fn usage_error(self, what: &str) -> ! {
        let _ = std::io::stderr().write_all(self.diagnostic(what).as_bytes());
        std::process::exit(1)
    }

    /// That one line, rendered — newline included.
    fn diagnostic(self, what: &str) -> String {
        format!("{what}. ('{tool} -h' for help.)\n", tool = self.0)
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
    /// getopt loop does: every error path exits 1 with C's diagnostic, never
    /// clap's exit 2.
    ///
    /// clap TERMINATES nothing here. `-h`/`-V` are ordinary options
    /// (`assert_repeatable` rejects the Help/Version actions) and a usage
    /// error is held in the returned [`Parsed`]; both are performed by
    /// [`Scan::finish`] at their place in the getopt order, so the warnings C
    /// prints ahead of them survive (R13-26, R14-18).
    ///
    /// This is the only entry point the binaries use, so a new option cannot
    /// re-introduce an exit-2 path by being declared through the derive — nor
    /// a non-repeatable option, which `assert_repeatable` rejects here.
    pub fn get_matches(self, cmd: clap::Command) -> Parsed {
        self.get_matches_from(cmd, std::env::args_os())
    }

    /// [`CTool::get_matches`] over an explicit `argv` — the whole command line,
    /// program name included, as `getopt` sees it.
    pub fn get_matches_from<I, T>(self, cmd: clap::Command, argv: I) -> Parsed
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        assert_repeatable(&cmd);
        let argv: Vec<std::ffi::OsString> = argv.into_iter().map(Into::into).collect();
        let error = match cmd.clone().try_get_matches_from(&argv) {
            Ok(matches) => {
                return Parsed {
                    tool: self,
                    matches,
                    error: None,
                };
            }
            Err(e) => e,
        };

        // getopt stops AT the offending token: the options before it were all
        // processed, the ones after it never are. The longest argv prefix clap
        // accepts is exactly that "before" — parse it on its own, and take the
        // error from the shortest prefix that FAILS, so the diagnostic names
        // the FIRST offending token even when the command line has several.
        for k in (1..argv.len()).rev() {
            let Ok(matches) = cmd.clone().try_get_matches_from(&argv[..k]) else {
                continue;
            };
            let error = cmd
                .clone()
                .try_get_matches_from(&argv[..=k])
                .err()
                .unwrap_or(error);
            return Parsed {
                tool: self,
                matches,
                error: Some(self.usage_diagnostic(&cmd, error)),
            };
        }
        // No prefix parses, so no option preceded the offending token and there
        // is nothing to warn about first.
        let _ = std::io::stderr().write_all(self.usage_diagnostic(&cmd, error).as_bytes());
        std::process::exit(1)
    }

    /// What C's getopt loop prints for the offending token — rendered, never
    /// printed here: it is [`Scan::finish`] that puts it on stderr, after the
    /// warnings of the options that preceded it (R14-18). Newline included, so
    /// the caller writes it verbatim.
    fn usage_diagnostic(self, spec: &clap::Command, e: clap::Error) -> String {
        use clap::error::{ContextKind, ErrorKind};

        match e.kind() {
            // getopt `'?'`: `"Unrecognized option: '-%c'."` The token is
            // echoed as the user typed it (C echoes `optopt`, which for a
            // long-form token it never supports collapses to `'--'`).
            ErrorKind::UnknownArgument => {
                let arg = context_arg(&e, ContextKind::InvalidArg);
                self.diagnostic(&format!("Unrecognized option: '{arg}'"))
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
                self.diagnostic(&format!("Option '{flag}' requires an argument"))
            }

            // No other usage error exists in C. Keep clap's message (we have
            // nothing truer to say) but not its exit code: 2 is not a status
            // any CA tool returns.
            _ => {
                let mut rendered = e.render().to_string();
                if !rendered.ends_with('\n') {
                    rendered.push('\n');
                }
                rendered
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

    /// A `caget`-shaped spec: every option the resolvers below read, declared
    /// exactly as the binary declares it. Tests parse real argv through this,
    /// so the command-line ORDER the C rules turn on is real rather than
    /// simulated — and it is the only way to reach a resolver now, which is
    /// the point of [`Scan`]: a warning with no position cannot be ordered.
    fn caget_spec() -> clap::Command {
        let val = |id: &'static str, c: char| {
            clap::Arg::new(id)
                .short(c)
                .action(clap::ArgAction::Append)
                .allow_hyphen_values(true)
        };
        let flag =
            |id: &'static str, c: char| clap::Arg::new(id).short(c).action(clap::ArgAction::Count);
        clap::Command::new("caget")
            .disable_help_flag(true)
            .disable_version_flag(true)
            .arg(val("timeout", 'w'))
            .arg(val("priority", 'p'))
            .arg(val("max_elements", '#'))
            .arg(val("stat_level", 's'))
            .arg(val("int_base", '0'))
            .arg(val("float_base", 'l'))
            .arg(val("field_separator", 'F'))
            .arg(val("fmt_e", 'e'))
            .arg(val("fmt_f", 'f'))
            .arg(val("fmt_g", 'g'))
            .arg(flag("help", 'h'))
            .arg(flag("version", 'V'))
            .arg(clap::Arg::new("pv").num_args(0..))
    }

    fn matches_of(argv: &[&str]) -> clap::ArgMatches {
        caget_spec().get_matches_from(argv)
    }

    /// `caget <argv...>`, with the warnings the getopt loop raised, in order.
    fn warnings_of(argv: &[&str]) -> Vec<String> {
        let m = matches_of(argv);
        let mut s = CAGET.scan(&m);
        // Every resolver, in the order `caget-rs::main` calls them — which is
        // deliberately NOT the command-line order, so the ordering under test
        // can only come from the positions the scan records.
        let _ = s.timeout("timeout", 1.0);
        let _ = s.priority("priority");
        let _ = s.base('0', "int_base");
        let _ = s.base('l', "float_base");
        let _ = s.float_precision(&[('e', "fmt_e"), ('f', "fmt_f"), ('g', "fmt_g")]);
        let _ = s.req_elems_int("max_elements");
        s.ordered_warnings()
    }

    /// Observed on the compiled C `caget` (EPICS 7.0.10.1-DEV, linux-x86_64):
    ///   `caget -0x -0q PV` → warns, still prints `0xC8`
    ///   `caget -0xyz PV`   → prints `0xC8`, no warning
    ///   `caget -0q PV`     → warns, prints `200`
    #[test]
    fn invalid_base_does_not_reset_a_valid_one() {
        let base = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).base('0', "int_base")
        };
        assert_eq!(
            base(&["caget", "-0x", "-0q"]).style,
            IntStyle::Hex,
            "C guards the assignment with `if (outType != dec)`"
        );
        assert_eq!(base(&["caget", "-0xyz"]).style, IntStyle::Hex);
        assert_eq!(base(&["caget", "-0q"]).style, IntStyle::Dec);
        assert_eq!(base(&["caget", "-0", ""]).style, IntStyle::Dec);
        assert_eq!(base(&["caget"]).style, IntStyle::Dec);
        assert_eq!(base(&["caget", "-0x", "-0b"]).style, IntStyle::Bin);

        // R13-16: the SAME guard decides which occurrence last assigned, so
        // the fold reports WHERE — a trailing invalid base is not "the last
        // one", and that position is what races `-d` (`caget.c:497-503`).
        let at = |argv: &[&str]| base(argv).valid_at;
        let first = at(&["caget", "-0x"]);
        assert_eq!(
            at(&["caget", "-0x", "-0q"]),
            first,
            "`q` assigned nothing, so `x`'s position still stands"
        );
        assert!(
            at(&["caget", "-0x", "-0b"]) > first,
            "a valid later base moves the position"
        );
        assert!(at(&["caget", "-0q", "-0x"]) > first);
        assert_eq!(at(&["caget", "-0q"]), None);
        assert_eq!(at(&["caget"]), None);
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
    /// that is stricter than `sscanf` — and is `strtod`, so it takes hex
    /// floats and rejects ERANGE. Every row probed against the compiled C
    /// `caget -w`.
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

        // strtod takes C99 hex floats: `caget -w 0x10` waits 16 s in C.
        assert_eq!(scan_double("0x10"), Some(16.0));
        assert_eq!(scan_double("0X1p4"), Some(16.0));
        assert_eq!(scan_double("-0x10"), Some(-16.0));

        // ERANGE is a rejection, so `caget -w 1e400` WARNS in C rather than
        // silently taking an infinite deadline.
        assert_eq!(scan_double("1e400"), None, "epicsParseDouble: overflow");
        assert_eq!(scan_double("-1e400"), None, "epicsParseDouble: overflow");
        assert_eq!(scan_double("1e-400"), None, "epicsParseDouble: underflow");

        // The `inf` / `nan` words leave errno clear, so C ACCEPTS them here
        // (`-w inf` hangs; `-w nan` hangs). `cli::timeout_duration` holds
        // the port's documented deviation for what to DO with them.
        assert_eq!(scan_double("inf"), Some(f64::INFINITY));
        assert!(scan_double("nan").unwrap().is_nan());
    }

    /// Observed on the compiled C `caget`: `-p -1` and `-p 500` both clamp
    /// to 99 with NO diagnostic; only an unscannable argument warns.
    #[test]
    fn priority_wraps_then_clamps_like_c() {
        let prio = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).priority("priority")
        };
        assert_eq!(prio(&["caget", "-p", "3"]), 3);
        assert_eq!(prio(&["caget", "-p", "99"]), 99);
        assert_eq!(prio(&["caget", "-p", "500"]), 99);
        assert_eq!(
            prio(&["caget", "-p", "-1"]),
            99,
            "%u wraps, then the clamp fires"
        );
        assert_eq!(prio(&["caget", "-p", "abc"]), 0);
        assert_eq!(prio(&["caget"]), 0);
    }

    /// C's `-#` has exactly ONE "not specified" value: `0`. A failed scan
    /// resets to it, and a negative count sign-extends into "all elements"
    /// while still reading as "requested".
    #[test]
    fn req_elems_has_a_single_unspecified_value() {
        let n = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).req_elems_int("max_elements")
        };
        assert_eq!(n(&["caget"]), 0);
        assert_eq!(n(&["caget", "-#", "0"]), 0, "-# 0 IS 'not specified'");
        assert_eq!(n(&["caget", "-#", "abc"]), 0);
        assert_eq!(n(&["caget", "-#", "3"]), 3);
        assert_eq!(n(&["caget", "-#", "3x"]), 3);
        assert_eq!(n(&["caget", "-#", "-3"]), u64::MAX - 2);

        // camonitor's %lu keeps 64 bits where caget's %d truncates.
        let m = matches_of(&["caget", "-#", "5000000000"]);
        let cam = CTool::new("camonitor");
        assert_eq!(cam.scan(&m).req_elems_ulong("max_elements"), 5_000_000_000);
        assert_eq!(n(&["caget", "-#", "5000000000"]), 705_032_704);
    }

    /// Any non-zero level selects `ca_client_status` mode, so the `%u` wrap
    /// on `-s -1` is load-bearing (`cainfo.c:167-174`).
    #[test]
    fn stat_level_wraps_like_sscanf_u() {
        let lvl = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).stat_level("stat_level")
        };
        assert_eq!(lvl(&["caget", "-s", "10"]), 10);
        assert_eq!(lvl(&["caget", "-s", "-1"]), 4_294_967_295);
        assert_eq!(lvl(&["caget", "-s", "+3abc"]), 3);
        assert_eq!(lvl(&["caget", "-s", "abc"]), 0);
        assert_eq!(lvl(&["caget"]), 0);
    }

    /// Observed on the compiled C `caget`:
    ///   `-e 99` → "Precision 99 for option '-e' out of range - ignored."
    ///   `-e -2` → same, with -2
    ///   `-e 3x` → precision 3, NO warning
    ///   `-e abc`→ "Invalid precision argument 'abc' for option '-e' - ignored."
    /// All four still read the PV.
    #[test]
    fn digits_gates_on_c_range() {
        let d = |arg: &str| {
            let m = matches_of(&["caget"]);
            CAGET.scan(&m).digits(0, 'e', arg)
        };
        assert_eq!(d("3"), Some(3));
        assert_eq!(d("3x"), Some(3));
        assert_eq!(d("0"), Some(0));
        assert_eq!(d("18"), Some(18), "VALID_DOUBLE_DIGITS");
        assert_eq!(d("19"), None);
        assert_eq!(d("-2"), None);
        assert_eq!(d("abc"), None);
    }

    /// The `-e`/`-f`/`-g` resolver over real argv, so the command-line ORDER
    /// the C rule turns on is real rather than simulated.
    fn precision_of(argv: &[&str]) -> Option<(char, u32)> {
        let m = matches_of(argv);
        CAGET
            .scan(&m)
            .float_precision(&[('e', "fmt_e"), ('f', "fmt_f"), ('g', "fmt_g")])
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
        let sep = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).field_separator("field_separator")
        };
        assert_eq!(sep(&["caget", "-F", "abc"]), Some('a'));
        assert_eq!(sep(&["caget", "-F", ","]), Some(','));
        assert_eq!(sep(&["caget", "-F", ""]), Some('\0'));
        assert_eq!(sep(&["caget"]), None);
    }

    /// A bad `-w` leaves the env/default timeout in place and the tool RUNS.
    #[test]
    fn timeout_keeps_the_default_on_a_bad_scan() {
        let t = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).timeout("timeout", 1.0)
        };
        assert_eq!(t(&["caget", "-w", "2.5"]), 2.5);
        assert_eq!(t(&["caget", "-w", " 2.5 "]), 2.5);
        assert_eq!(t(&["caget", "-w", "abc"]), 1.0);
        assert_eq!(t(&["caget", "-w", "3x"]), 1.0);
        assert_eq!(t(&["caget"]), 1.0);
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
        let t = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).timeout("timeout", 1.0)
        };
        assert_eq!(t(&["caget", "-w", "5", "-w", "2"]), 2.0, "last wins");
        assert_eq!(
            t(&["caget", "-w", "5", "-w", "abc"]),
            5.0,
            "caTimeout is untouched by a bad scan"
        );

        let n = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).req_elems_int("max_elements")
        };
        assert_eq!(n(&["caget", "-#", "2", "-#", "3"]), 3);
        assert_eq!(
            n(&["caget", "-#", "3", "-#", "abc"]),
            0,
            "a bad -# resets count to 0"
        );

        let p = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).priority("priority")
        };
        assert_eq!(p(&["caget", "-p", "1", "-p", "2"]), 2);
        assert_eq!(
            p(&["caget", "-p", "5", "-p", "abc"]),
            DEFAULT_CA_PRIORITY,
            "a bad -p resets to DEFAULT_CA_PRIORITY"
        );

        let m = matches_of(&["caget", "-F", ",", "-F", ";"]);
        assert_eq!(CAGET.scan(&m).field_separator("field_separator"), Some(';'));

        let lvl = |argv: &[&str]| {
            let m = matches_of(argv);
            CAGET.scan(&m).stat_level("stat_level")
        };
        assert_eq!(lvl(&["caget", "-s", "1", "-s", "2"]), 2);
        assert_eq!(lvl(&["caget", "-s", "1", "-s", "abc"]), 0);
    }

    /// R13-26. C's getopt loop prints each warning as it reaches the option,
    /// so the order on stderr is the COMMAND-LINE order — not the order the
    /// program happens to resolve its options in. `warnings_of` calls the
    /// resolvers in `caget-rs::main`'s order (`-w` first, `-#` last), and the
    /// output still comes out in argv order, because each warning carries the
    /// position of the occurrence that raised it.
    #[test]
    fn warnings_come_out_in_getopt_order_not_resolver_order() {
        assert_eq!(
            warnings_of(&["caget", "-#", "abc", "-p", "zz", "-w", "qq"]),
            vec![
                "'abc' is not a valid array element count - ignored. ('caget -h' for help.)",
                "'zz' is not a valid CA priority - ignored. ('caget -h' for help.)",
                "'qq' is not a valid timeout value - ignored, using '1.0'. ('caget -h' for help.)",
            ]
        );
        // Reversed on the command line → reversed on stderr.
        assert_eq!(
            warnings_of(&["caget", "-w", "qq", "-p", "zz", "-#", "abc"]),
            vec![
                "'qq' is not a valid timeout value - ignored, using '1.0'. ('caget -h' for help.)",
                "'zz' is not a valid CA priority - ignored. ('caget -h' for help.)",
                "'abc' is not a valid array element count - ignored. ('caget -h' for help.)",
            ]
        );
    }

    /// R13-26, the other half: `-h`/`-V` end C's loop where they stand, so an
    /// option AFTER one of them is never scanned and never warns, while one
    /// BEFORE it has already printed. `finish` is what applies the cutoff;
    /// this pins the position it uses.
    #[test]
    fn a_terminal_option_cuts_the_loop_where_it_stands() {
        let m = matches_of(&["caget", "-w", "abc", "-h", "-p", "zz"]);
        let scan = CAGET.scan(&m);
        let terminal =
            scan.terminal(&[("help", Terminal::Usage(0)), ("version", Terminal::Version)]);
        let (at, kind) = terminal.expect("-h is a terminal option");
        assert_eq!(kind, Terminal::Usage(0));

        let m2 = matches_of(&["caget", "-w", "abc", "-h", "-p", "zz"]);
        let mut s2 = CAGET.scan(&m2);
        let _ = s2.timeout("timeout", 1.0);
        let _ = s2.priority("priority");
        let raised: Vec<usize> = {
            let mut w = s2.warnings.clone();
            w.sort_by_key(|&(i, _)| i);
            w.into_iter().map(|(i, _)| i).collect()
        };
        assert_eq!(raised.len(), 2, "both options were scanned by clap");
        assert!(raised[0] < at, "-w abc precedes -h: it prints");
        assert!(raised[1] > at, "-p zz follows -h: C never reaches it");
    }

    /// R14-18. A getopt error is an arm of the LOOP, so the options before the
    /// offending token were all scanned (and warned) and the ones after it
    /// never were. clap fails the whole parse instead, which is why the parse
    /// is cut at the offending token: what [`CTool::get_matches_from`] returns
    /// is the prefix, and the resolvers see exactly the options C's loop
    /// reached.
    #[test]
    fn a_getopt_error_leaves_the_options_before_it_scanned() {
        let warnings = |argv: &[&str]| {
            let p = CAGET.get_matches_from(caget_spec(), argv);
            let mut s = p.scan();
            let _ = s.timeout("timeout", 1.0);
            let _ = s.priority("priority");
            s.ordered_warnings()
        };

        assert_eq!(
            warnings(&["caget", "-w", "abc", "-X"]),
            vec![
                "'abc' is not a valid timeout value - ignored, using '1.0'. ('caget -h' for help.)"
            ],
            "the '-w' before the unknown option was scanned and warned"
        );
        assert!(
            warnings(&["caget", "-X", "-w", "abc"]).is_empty(),
            "the '-w' after it is never reached"
        );
        assert_eq!(
            warnings(&["caget", "-w", "abc", "-p", "zz", "-X", "PV"]).len(),
            2,
            "every option before the offending token warns, in order"
        );
        // `':'` — the option-argument the last token never got — cuts the loop
        // the same way.
        assert_eq!(
            warnings(&["caget", "-p", "zz", "-w"]),
            vec!["'zz' is not a valid CA priority - ignored. ('caget -h' for help.)"]
        );
    }

    /// `-V` is a plain option now; declaring it (or `-h`) with clap's
    /// terminating action would swallow the warnings C prints first, so the
    /// spec check rejects it — the structural half of R13-26.
    #[test]
    #[should_panic(expected = "C's getopt accepts every option any number of times")]
    fn a_clap_help_action_is_rejected_at_the_spec() {
        let cmd = clap::Command::new("caget").arg(
            clap::Arg::new("help")
                .short('h')
                .action(clap::ArgAction::Help),
        );
        let _ = CTool::new("caget").get_matches(cmd);
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
                .allow_hyphen_values(true)
                .action(clap::ArgAction::Set),
        );
        assert_repeatable(&cmd);
    }

    /// `getopt(3)` never inspects `optarg`: `caget -w -1` gives `case 'w'` the
    /// string `"-1"`. Without `allow_hyphen_values` clap reads the `-1` as an
    /// option of its own and the tool dies on an unrecognized-option error it
    /// should never reach, so the spec must refuse that declaration too.
    #[test]
    #[should_panic(expected = "clap would reject an argument that begins with '-'")]
    fn a_value_option_that_clap_can_type_check_is_rejected_at_the_spec() {
        let cmd = clap::Command::new("caget").arg(
            clap::Arg::new("wait")
                .short('w')
                .action(clap::ArgAction::Append),
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
                    .allow_hyphen_values(true)
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
