//! Regression tests (R14-17): `EPICS_CLI_TIMEOUT` is scanned by C's
//! `epicsScanDouble`, and a value it rejects WARNS.
//!
//! `use_ca_timeout_env` (`tool_lib.c:646-660`) runs before the getopt loop in
//! all four tools (`caget.c:396`, `camonitor.c:222`, `caput.c:288`,
//! `cainfo.c:144`):
//!
//! ```text
//! if (epicsScanDouble(tmoStr, timeout) != 1) {
//!     fprintf(stderr, "'%s' is not a valid timeout value "
//!         "(from 'EPICS_CLI_TIMEOUT' in the environment) - "
//!         "ignored. (use '-h' for help.)\n", tmoStr);
//!     *timeout = DEFAULT_TIMEOUT;
//! }
//! ```
//!
//! The port used to read the variable with a bare `str::parse::<f64>()`: a
//! rejected value defaulted SILENTLY, and a value C accepts — `" -1 "`, which
//! `epicsParseDouble` trims — was rejected, taking the 1 s default where C
//! takes an expired deadline and exits 1. Both halves are pinned here; the
//! value half lives in `epics_ca_rs::cli`'s unit tests, which can read the
//! resolved timeout directly.

use std::process::Command;

const TOOLS: [(&str, &str); 4] = [
    (env!("CARGO_BIN_EXE_caget-rs"), "caget"),
    (env!("CARGO_BIN_EXE_camonitor-rs"), "camonitor"),
    (env!("CARGO_BIN_EXE_caput-rs"), "caput"),
    (env!("CARGO_BIN_EXE_cainfo-rs"), "cainfo"),
];

const ENV_WARNING: &str = "'abc' is not a valid timeout value (from 'EPICS_CLI_TIMEOUT' in the \
                           environment) - ignored. (use '-h' for help.)";

/// stderr of a tool run with `EPICS_CLI_TIMEOUT` set to `value`.
fn stderr_with_env(bin: &str, value: &str, args: &[&str]) -> String {
    let out = Command::new(bin)
        .args(args)
        .env("EPICS_CLI_TIMEOUT", value)
        .output()
        .expect("spawn the CA tool");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every tool calls `use_ca_timeout_env`, so every tool prints C's warning.
#[test]
fn an_unscannable_env_timeout_warns_in_all_four_tools() {
    for (bin, tool) in TOOLS {
        let stderr = stderr_with_env(bin, "abc", &[]);
        let first = stderr.lines().next().unwrap_or_default();
        assert_eq!(first, ENV_WARNING, "{tool}: stderr was:\n{stderr}");
    }
}

/// `use_ca_timeout_env` runs BEFORE the getopt loop, so its warning precedes
/// every option warning the loop raises — whatever the command line's order.
#[test]
fn the_env_warning_precedes_the_getopt_warnings() {
    let stderr = stderr_with_env(env!("CARGO_BIN_EXE_caget-rs"), "abc", &["-p", "zz", "PV"]);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines[0], ENV_WARNING, "stderr was:\n{stderr}");
    assert!(
        lines[1].contains("'zz' is not a valid CA priority"),
        "the getopt loop's warning comes second; stderr was:\n{stderr}"
    );
}

/// Same fact against the other end of the loop: `case 'h'` cannot swallow a
/// warning that was printed before the loop began.
#[test]
fn the_env_warning_precedes_the_help_block() {
    let stderr = stderr_with_env(env!("CARGO_BIN_EXE_caget-rs"), "abc", &["-h"]);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines[0], ENV_WARNING, "stderr was:\n{stderr}");
    assert!(
        stderr.contains("Usage:"),
        "and the usage block still follows; stderr was:\n{stderr}"
    );
}

/// A value C's scanner ACCEPTS raises no warning — including the ones a bare
/// `str::parse` would reject.
#[test]
fn a_scannable_env_timeout_is_silent() {
    for value in ["2.5", " -1 ", "0"] {
        let stderr = stderr_with_env(env!("CARGO_BIN_EXE_caget-rs"), value, &[]);
        assert!(
            !stderr.contains("is not a valid timeout value"),
            "EPICS_CLI_TIMEOUT={value:?} scans in C; stderr was:\n{stderr}"
        );
        assert_eq!(
            stderr.trim(),
            "No pv name specified. ('caget -h' for help.)",
            "EPICS_CLI_TIMEOUT={value:?}: only the post-loop check speaks"
        );
    }
}
