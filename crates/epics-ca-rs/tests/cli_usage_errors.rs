//! Regression tests (R12-18): a usage error in a CA tool is C's one-line
//! diagnostic on stderr and status **1** — never clap's help dump and
//! status 2.
//!
//! Every C tool ends its getopt loop the same way (`caget.c:500-531` and
//! the identical blocks in `camonitor.c`, `caput.c`, `cainfo.c`):
//!
//! ```text
//! case '?': "Unrecognized option: '-%c'. ('caget -h' for help.)"   return 1
//! case ':': "Option '-%c' requires an argument. ('caget -h' for help.)" return 1
//! nPvs < 1: "No pv name specified. ('caget -h' for help.)"          return 1
//! ```
//!
//! No C CA tool exits 2, and none has a *required* positional — getopt
//! parses, then `main` validates. Verified against the compiled C tools
//! (EPICS 7 base): the invocations below were run head-to-head and are
//! byte-identical, exit status included.
//!
//! Those three arms are arms of the LOOP, reached in argv order like every
//! other case — so a warning from an option BEFORE the offending token is
//! already on stderr when the diagnostic prints, and an option after it is
//! never scanned at all (R14-18).

use std::process::Command;

/// `(binary, tool-name-as-C-spells-it)`.
const TOOLS: [(&str, &str); 4] = [
    (env!("CARGO_BIN_EXE_caget-rs"), "caget"),
    (env!("CARGO_BIN_EXE_camonitor-rs"), "camonitor"),
    (env!("CARGO_BIN_EXE_caput-rs"), "caput"),
    (env!("CARGO_BIN_EXE_cainfo-rs"), "cainfo"),
];

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

/// C `if (nPvs < 1)`. A tool with no PV name must NOT reach clap's
/// "required arguments were not provided" (exit 2).
#[test]
fn no_pv_name_is_status_1_with_the_c_diagnostic() {
    for (bin, tool) in TOOLS {
        let (code, stdout, stderr) = run(bin, &[]);
        assert_eq!(code, 1, "{tool}: status");
        assert_eq!(
            stderr,
            format!("No pv name specified. ('{tool} -h' for help.)\n"),
            "{tool}: stderr"
        );
        assert!(stdout.is_empty(), "{tool}: stdout must stay empty");
    }
}

/// C getopt `'?'`.
#[test]
fn unknown_option_is_status_1_with_the_c_diagnostic() {
    for (bin, tool) in TOOLS {
        let (code, _, stderr) = run(bin, &["-X", "PV"]);
        assert_eq!(code, 1, "{tool}: status");
        assert_eq!(
            stderr,
            format!("Unrecognized option: '-X'. ('{tool} -h' for help.)\n"),
            "{tool}: stderr"
        );
    }
}

/// C getopt `default:` — the arm an option that IS in the optstring but has no
/// `case` of its own falls into (R13-25).
///
/// `cainfo`'s optstring is `":nhVw:s:p:"` (`cainfo.c:146`) and there is no
/// `case 'n'`, so `cainfo -n` reaches `default: usage(); return 1`
/// (`cainfo.c:194-196`): the full usage block on stderr, status 1. It is NOT
/// `Unrecognized option` — that is `case '?'`, which only a letter *outside* the
/// optstring gets, and the two are distinguishable by their text and by the fact
/// that `-n` prints the whole block.
///
/// (The block's wording is clap's, not C's — a standing divergence of this port,
/// pinned by `help_goes_to_stderr_and_version_to_stdout_both_exit_0` below. What
/// this test pins is the ARM: usage block, status 1, and no `case '?'`
/// diagnostic.)
#[test]
fn cainfo_dash_n_is_the_c_default_arm_not_an_unknown_option() {
    let (code, stdout, stderr) = run(env!("CARGO_BIN_EXE_cainfo-rs"), &["-n", "PV"]);
    assert_eq!(code, 1, "cainfo -n: C's `default:` arm returns 1");
    assert!(
        !stderr.contains("Unrecognized option"),
        "'-n' is in cainfo's optstring, so it is never `case '?'`; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("Usage:"),
        "C's `default:` arm prints the usage block on stderr; stderr was:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "the exit-1 usage block goes to stderr, not stdout; stdout was:\n{stdout}"
    );
}

/// C getopt `':'`. Note the argv: a trailing `PV` would become `-w`'s
/// *argument* (in C too — getopt takes the next token), so the missing-value
/// case is `-w` as the last token.
#[test]
fn option_without_its_argument_is_status_1_with_the_c_diagnostic() {
    for (bin, tool) in TOOLS {
        let (code, _, stderr) = run(bin, &["-w"]);
        assert_eq!(code, 1, "{tool}: status");
        assert_eq!(
            stderr,
            format!("Option '-w' requires an argument. ('{tool} -h' for help.)\n"),
            "{tool}: stderr"
        );
    }
}

/// clap blames the option in its LONG form (`--wait <TIMEOUT>`) however it
/// was typed, so the owner resolves back through the command spec to C's
/// short letter. The long spellings are a Rust-only extension; C would call
/// `--wait` an unrecognized option.
#[test]
fn long_form_missing_value_still_reports_the_c_short_letter() {
    let (code, _, stderr) = run(env!("CARGO_BIN_EXE_caget-rs"), &["--wait"]);
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        "Option '-w' requires an argument. ('caget -h' for help.)\n"
    );
}

/// R14-18. `case '?'` is an arm of the getopt LOOP, not a pre-pass: the
/// options before the offending token have already been scanned, so each of
/// their warnings is on stderr before the diagnostic. `caget -w abc -X` is TWO
/// lines in C — the `-w` warning, then the `-X` line — where the port printed
/// only the second.
#[test]
fn a_warning_before_the_offending_option_prints_first() {
    for (bin, tool) in TOOLS {
        let (code, _, stderr) = run(bin, &["-w", "abc", "-X"]);
        assert_eq!(code, 1, "{tool}: status");
        assert_eq!(
            stderr,
            format!(
                "'abc' is not a valid timeout value - ignored, using '1.0'. \
                 ('{tool} -h' for help.)\n\
                 Unrecognized option: '-X'. ('{tool} -h' for help.)\n"
            ),
            "{tool}: stderr"
        );
    }
}

/// The same fact from the other side: `case '?'` `return`s 1 where it stands,
/// so an option AFTER the offending token is never scanned and never warns.
#[test]
fn an_option_after_the_offending_one_is_never_scanned() {
    for (bin, tool) in TOOLS {
        let (code, _, stderr) = run(bin, &["-X", "-w", "abc"]);
        assert_eq!(code, 1, "{tool}: status");
        assert_eq!(
            stderr,
            format!("Unrecognized option: '-X'. ('{tool} -h' for help.)\n"),
            "{tool}: stderr"
        );
    }
}

/// `case ':'` cuts the loop the same way — a warning raised before the option
/// whose argument is missing still prints.
#[test]
fn a_warning_before_a_missing_argument_prints_first() {
    let (code, _, stderr) = run(env!("CARGO_BIN_EXE_caget-rs"), &["-p", "zz", "-w"]);
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        "'zz' is not a valid CA priority - ignored. ('caget -h' for help.)\n\
         Option '-w' requires an argument. ('caget -h' for help.)\n"
    );
}

/// `-h` and the error are both exits from the one loop, so the EARLIER token
/// wins: `-h` before the offending option returns 0 with the usage block and
/// no diagnostic; after it, the loop never reaches the `-h`.
#[test]
fn the_earlier_of_help_and_the_error_ends_the_loop() {
    let bin = env!("CARGO_BIN_EXE_caget-rs");

    let (code, _, stderr) = run(bin, &["-h", "-X"]);
    assert_eq!(code, 0, "`case 'h'` returns 0 before the loop meets '-X'");
    assert!(
        !stderr.contains("Unrecognized option"),
        "stderr was:\n{stderr}"
    );
    assert!(stderr.contains("Usage:"), "stderr was:\n{stderr}");

    let (code, _, stderr) = run(bin, &["-X", "-h"]);
    assert_eq!(code, 1, "`case '?'` returns 1 before the loop meets '-h'");
    assert_eq!(
        stderr, "Unrecognized option: '-X'. ('caget -h' for help.)\n",
        "no usage block: C's `case '?'` prints one line"
    );
}

/// getopt blames the FIRST unknown option and returns there, so a second one
/// further along the command line is never reported.
#[test]
fn the_first_offending_option_is_the_one_reported() {
    let (code, _, stderr) = run(env!("CARGO_BIN_EXE_caget-rs"), &["-X", "-Y"]);
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        "Unrecognized option: '-X'. ('caget -h' for help.)\n"
    );
}

/// C `caput.c:462-465` `if (nPvs < 2)` — the PV-name check comes first, so
/// a PV with no value is a *value* error, not a PV error.
#[test]
fn caput_without_a_value_reports_no_value_specified() {
    let (code, _, stderr) = run(env!("CARGO_BIN_EXE_caput-rs"), &["PV"]);
    assert_eq!(code, 1);
    assert_eq!(stderr, "No value specified. ('caput -h' for help.)\n");
}

/// `-h`/`-V` are not usage errors: they exit 0, like C's `usage()` / version
/// paths. The exit-1 mapping must not swallow them.
///
/// The STREAMS differ, and C picks them per call: `usage()` is one
/// `fprintf(stderr, ...)` (`caget.c:56-58`, `camonitor.c:45-47`,
/// `caput.c:60-62`, `cainfo.c:37-39`), while `case 'V'` is a `printf`
/// (`caget.c:404`). So `-h` writes stderr and `-V` writes stdout — R14-16;
/// the port used to send the exit-0 help block to stdout because that is
/// clap's stream.
#[test]
fn help_goes_to_stderr_and_version_to_stdout_both_exit_0() {
    for (bin, tool) in TOOLS {
        let (code, stdout, stderr) = run(bin, &["-h"]);
        assert_eq!(code, 0, "{tool} -h: status");
        assert!(
            stderr.contains("Usage:"),
            "{tool} -h: C's usage() writes stderr; stderr was:\n{stderr}"
        );
        assert!(
            stdout.is_empty(),
            "{tool} -h: nothing goes to stdout; stdout was:\n{stdout}"
        );

        let (code, stdout, stderr) = run(bin, &["-V"]);
        assert_eq!(code, 0, "{tool} -V: status");
        assert!(!stdout.is_empty(), "{tool} -V: the banner is a printf");
        assert!(
            stderr.is_empty(),
            "{tool} -V: nothing goes to stderr; stderr was:\n{stderr}"
        );
    }
}

/// The `-V` banner is an interop surface: a deployment script reads the EPICS
/// Base release out of it. Byte-for-byte what the compiled C tools print
/// (`caget -V | od -c`, EPICS 7.0.10.1-DEV):
///
/// ```text
/// \nEPICS Version EPICS 7.0.10.1-DEV, CA Protocol version 4.13\n
/// ```
///
/// The port used to print `EPICS Version epics-rs <crate-version>` — the crate
/// version in the slot that names the base release, so the one field the line
/// exists to carry was the one field it did not carry.
#[test]
fn version_banner_is_the_c_line() {
    let want = format!(
        "\nEPICS Version {}, CA Protocol version {}\n",
        epics_base_rs::runtime::version::EPICS_VERSION_STRING,
        epics_ca_rs::protocol::ca_version(),
    );
    // Pinned against the compiled C, not just against our own consts: a const
    // that drifts from upstream would otherwise agree with itself.
    assert_eq!(
        want,
        "\nEPICS Version EPICS 7.0.10.1-DEV, CA Protocol version 4.13\n"
    );
    for (bin, tool) in TOOLS {
        let (_, stdout, _) = run(bin, &["-V"]);
        assert_eq!(stdout, want, "{tool} -V");
    }
}
