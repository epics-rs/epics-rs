//! Regression tests (R13-17): every C option is repeatable, last one wins.
//!
//! `getopt(3)` has no notion of "already used" — it hands the loop one
//! `(opt, optarg)` pair per occurrence and the loop body overwrites its own
//! variable (`caget.c:398`, `camonitor.c:224`, `caput.c:290`, `cainfo.c:146`
//! — the four getopt loops). So a repeat is ordinary input, not an error:
//!
//! ```text
//! caget -w 5 -w 2 TST:LO     C: reads TST:LO with a 2 s timeout
//! caget -t -t     TST:LO     C: warns "Options t,d,a are mutually exclusive",
//!                               then prints terse
//! ```
//!
//! clap's default `Set`/`SetTrue` actions instead abort with a multi-line
//! usage block ("the argument '--wait <TIMEOUT>' cannot be used multiple
//! times"), which no C CA tool can produce — verified head-to-head against
//! the compiled C tools (EPICS 7.0.10.1-DEV) on a live `softIoc`: all four
//! binaries are byte-identical on stdout, stderr and status for every
//! repetition below.
//!
//! These cases carry NO PV name, so each tool runs its whole getopt loop and
//! then falls into C's `if (nPvs < 1)` check — which is the point: reaching
//! that diagnostic proves the repeated option parsed, and its exact text
//! proves clap never got to speak.

use std::process::Command;

fn run(bin: &str, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin)
        .args(args)
        .output()
        .expect("spawn the CA tool");
    (
        out.status.code().expect("the tool exited normally"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A repeated option must land on C's `No pv name specified.` — never on a
/// clap usage block, and never on clap's exit 2.
fn assert_repeat_reaches_the_c_no_pv_check(bin: &str, tool: &str, args: &[&str]) {
    let (code, stdout, stderr) = run(bin, args);
    let no_pv = format!("No pv name specified. ('{tool} -h' for help.)");
    assert_eq!(code, 1, "{tool} {args:?}: status (clap's usage error is 2)");
    assert!(
        stderr.lines().next_back() == Some(no_pv.as_str()),
        "{tool} {args:?}: the getopt loop must run to completion and reach \
         C's nPvs check; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("Usage:") && !stderr.contains("cannot be used"),
        "{tool} {args:?}: clap's usage block must never appear; stderr was:\n{stderr}"
    );
    assert!(stdout.is_empty(), "{tool} {args:?}: stdout must stay empty");
}

/// `caget.c:398` `":taicnhsSVe:f:g:l:#:d:0:w:p:F:"` — every letter, twice.
#[test]
fn caget_accepts_every_option_twice() {
    let bin = env!("CARGO_BIN_EXE_caget-rs");
    for args in [
        &["-w", "5", "-w", "2"][..],
        &["-#", "2", "-#", "3"],
        &["-d", "DBR_LONG", "-d", "DBR_DOUBLE"],
        &["-p", "1", "-p", "2"],
        &["-e", "2", "-e", "6"],
        &["-f", "2", "-f", "6"],
        &["-g", "2", "-g", "6"],
        &["-F", ",", "-F", ";"],
        &["-0", "x", "-0", "b"],
        &["-l", "x", "-l", "b"],
        &["-t", "-t"],
        &["-a", "-a"],
        &["-n", "-n"],
        &["-c", "-c"],
        &["-s", "-s"],
        &["-S", "-S"],
    ] {
        assert_repeat_reaches_the_c_no_pv_check(bin, "caget", args);
    }
}

/// `camonitor.c:224` `":nhm:sSe:f:g:l:#:0:w:t:p:F:V"`.
#[test]
fn camonitor_accepts_every_option_twice() {
    let bin = env!("CARGO_BIN_EXE_camonitor-rs");
    for args in [
        &["-w", "5", "-w", "2"][..],
        &["-m", "v", "-m", "a"],
        &["-#", "2", "-#", "3"],
        &["-p", "1", "-p", "2"],
        &["-t", "s", "-t", "n"],
        &["-e", "2", "-e", "6"],
        &["-f", "2", "-f", "6"],
        &["-g", "2", "-g", "6"],
        &["-F", ",", "-F", ";"],
        &["-0", "x", "-0", "b"],
        &["-l", "x", "-l", "b"],
        &["-n", "-n"],
        &["-s", "-s"],
        &["-S", "-S"],
    ] {
        assert_repeat_reaches_the_c_no_pv_check(bin, "camonitor", args);
    }
}

/// `caput.c:290` `":cnlhatsVS#:w:p:F:"`. The `-n`/`-s` and `-S`/`-a` pairs
/// each clear their partner (`caput.c:298-319`), so they repeat too.
#[test]
fn caput_accepts_every_option_twice() {
    let bin = env!("CARGO_BIN_EXE_caput-rs");
    for args in [
        &["-w", "5", "-w", "2"][..],
        &["-p", "1", "-p", "2"],
        &["-#", "2", "-#", "3"],
        &["-F", ",", "-F", ";"],
        &["-t", "-t"],
        &["-l", "-l"],
        &["-c", "-c"],
        &["-n", "-n"],
        &["-s", "-s"],
        &["-S", "-S"],
        &["-a", "-a"],
        &["-n", "-s", "-n"],
        &["-S", "-a", "-S"],
    ] {
        assert_repeat_reaches_the_c_no_pv_check(bin, "caput", args);
    }
}

/// `cainfo.c:146` `":nhVw:s:p:"`. `-s <non-zero>` and the Rust-only `-d`
/// both exempt the missing-PV check, so this covers the options that do not.
#[test]
fn cainfo_accepts_every_option_twice() {
    let bin = env!("CARGO_BIN_EXE_cainfo-rs");
    for args in [
        &["-w", "5", "-w", "2"][..],
        &["-p", "1", "-p", "2"],
        &["-s", "0", "-s", "0"],
    ] {
        assert_repeat_reaches_the_c_no_pv_check(bin, "cainfo", args);
    }
}

/// The repeat is not merely tolerated — the LAST occurrence has to win, and
/// each C case folds its own way. Only the diagnostics are observable without
/// an IOC, and they pin the fold exactly:
///
/// * `-w 5 -w abc` (`caget.c:437-443`): a bad `epicsScanDouble` leaves
///   `caTimeout` alone, so the warning echoes the SURVIVING 5, not the 1.0
///   default — proof the first `-w` took effect and the second did not clear it.
/// * `-e 3 -e 99` (`:470-484`): the out-of-range repeat warns and never
///   reaches the `sprintf` that rewrites `dblFormatStr`.
/// * `-t -t` (`:369-375` `complainIfNotPlainAndSet`): the second occurrence
///   sees a non-plain format and warns, exactly once.
#[test]
fn the_last_occurrence_wins_the_way_its_c_case_folds() {
    let bin = env!("CARGO_BIN_EXE_caget-rs");

    let (_, _, stderr) = run(bin, &["-w", "5", "-w", "abc"]);
    assert!(
        stderr.contains("'abc' is not a valid timeout value - ignored, using '5.0'."),
        "a bad -w must keep the previous -w's value; stderr was:\n{stderr}"
    );

    let (_, _, stderr) = run(bin, &["-e", "3", "-e", "99"]);
    assert!(
        stderr.contains("Precision 99 for option '-e' out of range - ignored."),
        "each occurrence is scanned and warns on its own; stderr was:\n{stderr}"
    );

    let (_, _, stderr) = run(bin, &["-t", "-t"]);
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.starts_with("Options t,d,a are mutually exclusive."))
            .count(),
        1,
        "C warns once per occurrence past the first; stderr was:\n{stderr}"
    );

    // Three t/a/d occurrences → two warnings, and `-t` (the last) wins.
    let (_, _, stderr) = run(bin, &["-t", "-a", "-t"]);
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.starts_with("Options t,d,a are mutually exclusive."))
            .count(),
        2,
        "stderr was:\n{stderr}"
    );

    // An invalid `-d` reverts format to plain (`caget.c:430-434`), which
    // un-arms the NEXT occurrence's mutual-exclusion warning.
    let (_, _, stderr) = run(bin, &["-d", "BOGUS", "-t"]);
    assert!(
        !stderr.contains("mutually exclusive"),
        "an invalid -d leaves format == plain, so -t must not warn; stderr was:\n{stderr}"
    );
    assert!(stderr.contains("Requested dbr type out of range or invalid - ignored."));
}
