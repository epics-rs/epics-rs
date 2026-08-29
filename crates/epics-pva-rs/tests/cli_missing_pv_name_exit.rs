//! Exit codes and diagnostics when a PV name is missing.
//!
//! clap exits 2 on a usage error. No pvAccess tool does — and the three
//! that check for a missing PV name check it themselves rather than
//! leaving it to `getopt`, so the message is theirs too. Captured from
//! `/home/stevek/work/epics-base/bin/linux-x86_64`:
//!
//! ```text
//! $ pvget      ; echo $?     ->            0
//! $ pvmonitor  ; echo $?     ->            0
//! $ pvput      ; echo $?     -> No pv name specified. ('pvput -h' for help.)      1
//! $ pvcall     ; echo $?     -> No pv name specified. ('pvput -h' for help.)      1
//! $ pvinfo     ; echo $?     -> No pv name(s) specified. ('pvinfo -h' for help.)  1
//! $ pvget -Z   ; echo $?     -> Unrecognized option: 'Z'. ('pvget -h' for help.)  1
//! ```
//!
//! `pvget`/`pvmonitor` have no check at all: the connect loop runs from
//! `optind` to `argc` (pvget.cpp:400) and an empty range leaves
//! `haderror` clear. `pvcall` prints the *pvput* wording, `pvput -h` and
//! all (pvcall.cpp:172-174) — a C quirk, transcribed rather than tidied.

// Host-only: drives the built CLI binaries out of process, and those are
// the tokio-backend builds.
#![cfg(tokio_backend)]

use std::process::Command;

fn run(exe: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(exe).args(args).output().expect("run CLI");
    (
        out.status.code().expect("exited normally"),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn pvget_and_pvmonitor_treat_no_pv_name_as_no_work() {
    for exe in [
        env!("CARGO_BIN_EXE_pvget-rs"),
        env!("CARGO_BIN_EXE_pvmonitor-rs"),
    ] {
        let (code, stderr) = run(exe, &[]);
        assert_eq!(code, 0, "{exe} with no PV name; stderr: {stderr:?}");
        assert_eq!(stderr, "", "{exe}");
    }
}

#[test]
fn pvput_pvcall_and_pvinfo_print_cs_own_missing_name_diagnostic() {
    for (exe, want) in [
        (
            env!("CARGO_BIN_EXE_pvput-rs"),
            "No pv name specified. ('pvput -h' for help.)\n",
        ),
        (
            env!("CARGO_BIN_EXE_pvcall-rs"),
            "No pv name specified. ('pvput -h' for help.)\n",
        ),
        (
            env!("CARGO_BIN_EXE_pvinfo-rs"),
            "No pv name(s) specified. ('pvinfo -h' for help.)\n",
        ),
    ] {
        let (code, stderr) = run(exe, &[]);
        assert_eq!(code, 1, "{exe}; stderr: {stderr:?}");
        assert_eq!(stderr, want, "{exe}");
    }
}

/// pvput.cpp:372-377, the check that follows the name check.
#[test]
fn pvput_reports_missing_values_with_cs_wording() {
    let (code, stderr) = run(env!("CARGO_BIN_EXE_pvput-rs"), &["TST:NOSUCHPV"]);
    assert_eq!(code, 1, "stderr: {stderr:?}");
    assert_eq!(stderr, "No value(s) specified. ('pvput -h' for help.)\n");
}

/// Every other usage error is C's `getopt` arm, which also returns 1
/// (pvget.cpp:357). Only the code is pinned here — clap's wording is not
/// C's, and that difference is not what a shell script reads.
#[test]
fn an_unknown_option_exits_one_not_clap_two() {
    for exe in [
        env!("CARGO_BIN_EXE_pvget-rs"),
        env!("CARGO_BIN_EXE_pvmonitor-rs"),
        env!("CARGO_BIN_EXE_pvput-rs"),
        env!("CARGO_BIN_EXE_pvcall-rs"),
        env!("CARGO_BIN_EXE_pvinfo-rs"),
        env!("CARGO_BIN_EXE_pvlist-rs"),
    ] {
        let (code, stderr) = run(exe, &["-Z"]);
        assert_eq!(code, 1, "{exe}; stderr: {stderr:?}");
    }
}

/// `-h` and `-V` are the two clap errors C returns 0 for
/// (pvget.cpp:283,300).
#[test]
fn help_still_exits_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_pvget-rs"))
        .arg("--help")
        .output()
        .expect("run CLI");
    assert_eq!(out.status.code(), Some(0));
    assert!(!out.stdout.is_empty());
}
