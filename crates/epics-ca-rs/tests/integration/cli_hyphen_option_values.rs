//! Regression tests: an option-argument that begins with `-` belongs to the
//! option, not to the parser.
//!
//! `getopt(3)` never inspects `optarg`. Whatever `argv` entry follows an option
//! that takes a value is handed to the `case` arm verbatim, `-` and all, and
//! the arm alone decides what it means:
//!
//! ```text
//! cainfo  -s -1     C: sscanf("%u") -> 4294967295, a status level (cainfo.c:167-172)
//! caget   -d -1     C: "Invalid data type '-1' - ignored." (caget.c:415-428)
//! caget   -0 -x     C: dec + "Invalid argument '-x' ..."   (caget.c:486-497)
//! camonitor -m -1   C: "Invalid argument '-1' ..."         (camonitor.c:285-300)
//! ```
//!
//! clap instead reads the `-1` as an unknown option and exits 2 with a usage
//! block, before the tool's own resolver ever sees the string — a diagnostic no
//! C CA tool can produce. `allow_hyphen_values` on every value option is what
//! keeps the argument with its option; `copt::assert_repeatable` panics at spec
//! time if one is ever declared without it.
//!
//! Every case below carries NO PV name, so the tool runs its whole getopt loop
//! and lands on C's `if (nPvs < 1)` check (`caget.c:520`, `camonitor.c:342`,
//! `cainfo.c:200`) — reaching that diagnostic is the proof that the hyphen
//! value was consumed as an argument.

use std::process::Command;

fn run(bin: &str, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    // On `exec_backend` (this crate built with
    // `EPICS_RS_BUILD_EXEC_BACKEND=thread`, or for the RTEMS target) the
    // client has no UDP SEARCH transport, so `CaClient::new` refuses an empty
    // `EPICS_CA_NAME_SERVERS` rather than spawning an engine that could reach
    // nothing — see `search::SearchTransport::name_servers_only`. These tests
    // are about getopt argument consumption, not about reaching a server, so
    // give the tool a syntactically valid name server it will never connect
    // to. Port 1 is reserved (`tcpmux`) and nothing in this workspace binds
    // it.
    #[cfg(exec_backend)]
    cmd.env("EPICS_CA_NAME_SERVERS", "127.0.0.1:1");
    let out = cmd.output().expect("spawn the CA tool");
    (
        out.status.code().expect("the tool exited normally"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The hyphen value must be consumed by its option and the loop must run on to
/// C's missing-PV check — never clap's exit 2, never a usage block.
fn assert_hyphen_value_reaches_the_c_no_pv_check(bin: &str, tool: &str, args: &[&str]) {
    let (code, stdout, stderr) = run(bin, args);
    let no_pv = format!("No pv name specified. ('{tool} -h' for help.)");
    assert_eq!(
        code, 1,
        "{tool} {args:?}: status (clap's usage error is 2); stderr was:\n{stderr}"
    );
    assert!(
        stderr.lines().next_back() == Some(no_pv.as_str()),
        "{tool} {args:?}: the getopt loop must consume '-...' as the option's argument and \
         reach C's nPvs check; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("Usage:") && !stderr.contains("unexpected argument"),
        "{tool} {args:?}: clap must never reject the option's argument; stderr was:\n{stderr}"
    );
    assert!(stdout.is_empty(), "{tool} {args:?}: stdout must stay empty");
}

/// `caget.c:398` — `-d`, `-0` and `-l` all take an argument that C scans and
/// warns about; a leading `-` makes it invalid input, not a parse error.
#[test]
fn caget_keeps_a_hyphen_value_with_its_option() {
    let bin = env!("CARGO_BIN_EXE_caget-rs");
    for args in [
        &["-d", "-1"][..],
        &["-0", "-x"],
        &["-l", "-x"],
        &["-w", "-1"],
        &["-#", "-1"],
        &["-p", "-1"],
        &["-e", "-1"],
    ] {
        assert_hyphen_value_reaches_the_c_no_pv_check(bin, "caget", args);
    }
}

/// `camonitor.c:224` — `-m` and `-t` scan the argument character by character
/// (`camonitor.c:285-300`, `camonitor.c:240-252`); '-' is simply one more
/// character they reject with a warning.
#[test]
fn camonitor_keeps_a_hyphen_value_with_its_option() {
    let bin = env!("CARGO_BIN_EXE_camonitor-rs");
    for args in [
        &["-m", "-1"][..],
        &["-t", "-1"],
        &["-0", "-x"],
        &["-l", "-x"],
        &["-w", "-1"],
    ] {
        assert_hyphen_value_reaches_the_c_no_pv_check(bin, "camonitor", args);
    }
}

/// `cainfo.c:167-172` — an interest level C cannot scan warns and falls back to
/// 0, which then hits the missing-PV check.
#[test]
fn cainfo_keeps_a_hyphen_value_with_its_option() {
    let bin = env!("CARGO_BIN_EXE_cainfo-rs");
    for args in [&["-s", "-x"][..], &["-w", "-1"], &["-p", "-1"]] {
        assert_hyphen_value_reaches_the_c_no_pv_check(bin, "cainfo", args);
    }
}

/// `cainfo.c:200` `if (!statLevel && nPvs < 1)` — a *scannable* hyphen value is
/// the load-bearing case, and the one where losing the argument changes the mode
/// the tool runs in rather than just a warning: `sscanf("-1", "%u")` succeeds
/// (it wraps to 4294967295), and a non-zero interest level skips the missing-PV
/// check to print the client status dump instead (`cainfo.c:77-78`, `:202-205`).
///
/// When clap owned the `-1` it never reached `case 's'` at all — the tool
/// reported `Unrecognized option: '-1'.` and exited 1, C's `case '?'` arm, which
/// C reaches only for an option letter that is not in the spec.
#[test]
fn cainfo_scans_a_negative_interest_level_as_unsigned() {
    let bin = env!("CARGO_BIN_EXE_cainfo-rs");
    let (code, stdout, stderr) = run(bin, &["-s", "-1"]);
    assert!(
        !stderr.contains("Unrecognized option"),
        "'-1' is the argument of '-s', not an option of its own; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("is not a valid interest level"),
        "C's sscanf(\"%u\") scans '-1' successfully; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("No pv name specified"),
        "a non-zero interest level exempts the missing-PV check (cainfo.c:200); \
         stderr was:\n{stderr}"
    );
    assert_eq!(code, 0, "status mode exits 0; stderr was:\n{stderr}");
    assert!(
        stdout.contains("Client Diagnostics"),
        "a non-zero '-s' selects the client status dump; stdout was:\n{stdout}"
    );
}
