//! Which failures `start_pvxs` absorbs. Not an interop test: a shell stands
//! in for the pvxs process, printing what pvxs prints.

// No exec-model census marker: every test here is a plain `#[test]`.

use super::interop_helpers::{PvxsLogs, start_pvxs};
use std::process::Command;

/// `start_pvxs` over `scripts`, one per attempt, ready once a log says `up`.
/// Returns the result and how many attempts were built.
fn start(dir: &std::path::Path, scripts: &[&str]) -> (Result<(), String>, usize) {
    let logs = PvxsLogs::in_dir(dir, "p");
    let mut built = 0;
    let started = start_pvxs(
        &logs,
        || {
            let script = scripts[built.min(scripts.len() - 1)];
            built += 1;
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            (cmd, ())
        },
        |_, text| text.contains("up"),
    );
    (started.map(drop), built)
}

const UDP6_ABORT: &str = "echo \"what():  Unable to bind random UDP6 port\" >&2; kill -ABRT $$";
const UP: &str = "echo up; exec sleep 30";

#[test]
fn a_client_context_bind_abort_is_rerun_and_its_logs_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (started, built) = start(dir.path(), &[UDP6_ABORT, UP]);
    assert_eq!((started, built), (Ok(()), 2));
    let kept = PvxsLogs::in_dir(dir.path(), "p").kept(1).text();
    assert!(kept.contains("Unable to bind random UDP6 port"), "{kept:?}");
}

/// Both keep running, on no port and on the wrong one.
#[test]
fn a_server_port_collision_is_rerun() {
    for lost in [
        "echo 'Error in pvxsBaseRegistrar : Address already in use' >&2; exec sleep 30",
        "echo 'WARN pvxs.tcp.setup Server unable to bind port 41941, falling back to \
         127.0.0.1:44199' >&2; exec sleep 30",
    ] {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(start(dir.path(), &[lost, UP]), (Ok(()), 2), "{lost}");
    }
}

#[test]
fn an_exit_without_a_collision_fails_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let (started, built) = start(dir.path(), &["echo crashed at load >&2; kill -ABRT $$", UP]);
    let err = started.unwrap_err();
    assert_eq!(built, 1, "an exit was rerun: {err}");
    assert!(err.contains("crashed at load"), "{err}");
}

#[test]
fn a_start_that_never_gets_ready_fails_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let (started, built) = start(dir.path(), &["echo still loading; exec sleep 30", UP]);
    let err = started.unwrap_err();
    assert_eq!(built, 1, "a slow start was rerun: {err}");
    assert!(err.contains("still loading"), "{err}");
}

#[test]
fn a_later_failure_names_the_kept_logs() {
    let dir = tempfile::tempdir().unwrap();
    let (started, built) = start(dir.path(), &[UDP6_ABORT, "echo boom >&2; exit 1"]);
    let err = started.unwrap_err();
    assert_eq!(built, 2);
    assert!(err.contains("p.err.1") && err.contains("boom"), "{err}");
}
