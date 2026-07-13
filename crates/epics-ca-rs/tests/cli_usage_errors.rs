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
//! (EPICS 7 base): all sixteen invocations below were run head-to-head and
//! are byte-identical, exit status included.

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
/// pinned by `help_and_version_still_exit_0` below. What this test pins is the
/// ARM: usage block, status 1, and no `case '?'` diagnostic.)
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

/// C `caput.c:462-465` `if (nPvs < 2)` — the PV-name check comes first, so
/// a PV with no value is a *value* error, not a PV error.
#[test]
fn caput_without_a_value_reports_no_value_specified() {
    let (code, _, stderr) = run(env!("CARGO_BIN_EXE_caput-rs"), &["PV"]);
    assert_eq!(code, 1);
    assert_eq!(stderr, "No value specified. ('caput -h' for help.)\n");
}

/// `-h`/`-V` are not usage errors: clap owns them and they exit 0, like C's
/// `usage()` / version paths. The exit-1 mapping must not swallow them.
#[test]
fn help_and_version_still_exit_0() {
    for (bin, tool) in TOOLS {
        for flag in ["-h", "-V"] {
            let (code, stdout, _) = run(bin, &[flag]);
            assert_eq!(code, 0, "{tool} {flag}: status");
            assert!(!stdout.is_empty(), "{tool} {flag}: stdout");
        }
    }
}
