//! **A shell with no `IocApplication` still runs C's `iocBuild_1`.**
//!
//! C has no "bare" shell: `iocInit()` is `iocBuild() || iocRun()`
//! (`iocInit.c:111-113`) for everyone, so `iocBuild_1` always runs and always
//! does four visible things (`iocInit.c:117-148`,
//! R7.0.10-146-g8f5015b663d764ad75df):
//!
//! ```c
//!     if (iocState != iocVoid) {
//!         errlogPrintf("iocBuild: " ERL_ERROR " IOC can only be initialized "
//!                      "from uninitialized or stopped state\n");
//!         return -1;
//!     }
//!     ...
//!     errlogPrintf("Starting iocInit\n");
//!     ...
//!     coreRelease();
//!     iocState = iocBuilding;
//! ```
//!
//! and `iocRun` closes with `errlogPrintf("iocRun: %s\n", ...)` (`:273`).
//!
//! The port's `NotOurs` arm — every `CaServerBuilder` binary and every bare
//! `PvDatabase` shell — closed the record load and returned, leaving
//! `iocState` at `iocVoid`. Measured against
//! `~/work/epics-base/bin/linux-x86_64/softIoc`, that lost the whole
//! lifecycle: `iocBuild` then `iocRun` answered `iocRun: WARNING IOC not
//! paused` where C answers `iocRun: All initialization complete`; a second
//! `iocBuild` was accepted where C refuses it; `iocPause` after `iocInit`
//! said `WARNING IOC not running`; and the `coreRelease` banner C
//! puts on stdout between the two command echoes was absent.
//!
//! The sink rule this also pins is uniform and countable: `iocInit.c` holds
//! twenty-four `errlogPrintf` calls and zero plain `printf` calls, so
//! `coreRelease()`'s banner (`misc/epicsRelease.c:23-27`) is the only stdout
//! write on the whole path.
//!
//! Every case here changes process-global IOC state, so each is its own test
//! and therefore its own process.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Run one script through a fresh shell; give back (stdout, stderr).
fn run(script_body: &str) -> (String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("t.cmd");
    std::fs::write(&script, script_body).expect("write the script");

    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the shell writes through fds 1 and 2 for the length of one
    // script and this test owns the process, so nothing else holds either.
    let saved = unsafe {
        let saved = (libc::dup(1), libc::dup(2));
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };

    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.display().to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: restoring the two descriptors this call replaced.
    unsafe {
        libc::dup2(saved.0, 1);
        libc::dup2(saved.1, 2);
        libc::close(saved.0);
        libc::close(saved.1);
    }
    assert!(ran.is_ok(), "the shell panicked");
    (
        strip_ansi(&std::fs::read_to_string(out_sink.path()).expect("stdout capture")),
        strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture")),
    )
}

/// The banner's two `Rev.` lines carry this tree's own VCS stamp, as C's carry
/// C's, so its shape is what can be asserted. The port drops C's
/// `epicsReleaseVersion` line — epics-rs's release is not the base version —
/// so four lines print where C prints five.
fn assert_banner(lines: &[&str], out: &str) {
    assert_eq!(lines.len(), 4, "stdout was {out:?}");
    assert!(lines[0].starts_with("###"), "stdout was {out:?}");
    assert!(lines[1].starts_with("## Rev. "), "stdout was {out:?}");
    assert!(lines[2].starts_with("## Rev. Date "), "stdout was {out:?}");
    assert!(lines[3].starts_with("###"), "stdout was {out:?}");
    assert!(
        !lines.iter().any(|l| l.contains("epics-rs")),
        "the base-version identity line is dropped: {out:?}"
    );
}

/// `iocBuild` then `iocRun`: C's banner lands between the two echoes and the
/// run reaches `iocBuilt`, so `iocRun` reports the completion, not a warning.
#[epics_macros_rs::epics_test]
async fn build_then_run_completes_instead_of_warning() {
    let (out, err) = run("iocBuild\niocRun\n");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first(), Some(&"iocBuild"), "stdout was {out:?}");
    assert_eq!(lines.last(), Some(&"iocRun"), "stdout was {out:?}");
    assert_banner(&lines[1..lines.len() - 1], &out);
    assert_eq!(
        err,
        "Starting iocInit\niocRun: All initialization complete\n"
    );
}

/// C `iocBuild_1` refuses from any state but `iocVoid`, and says so under the
/// name `iocBuild` whichever command called it.
#[epics_macros_rs::epics_test]
async fn a_second_build_is_refused() {
    let (_out, err) = run("iocBuild\niocBuild\n");
    assert_eq!(
        err,
        "Starting iocInit\niocBuild: ERROR IOC can only be initialized from \
         uninitialized or stopped state\n"
    );
}

/// The same refusal reached through `iocInit`, which is where C's `iocBuild:`
/// prefix looks surprising and is nonetheless what C prints.
#[epics_macros_rs::epics_test]
async fn a_second_init_is_refused_under_the_build_name() {
    let (_out, err) = run("iocInit\niocInit\n");
    assert_eq!(
        err,
        "Starting iocInit\niocRun: All initialization complete\niocBuild: ERROR \
         IOC can only be initialized from uninitialized or stopped state\n"
    );
}

/// `iocPause` and a resuming `iocRun` only work because the build advanced the
/// state cell; before the fix both answered a warning.
#[epics_macros_rs::epics_test]
async fn init_pause_run_is_the_full_c_sequence() {
    let (_out, err) = run("iocInit\niocPause\niocRun\n");
    assert_eq!(
        err,
        "Starting iocInit\niocRun: All initialization complete\niocPause: IOC \
         suspended\niocRun: IOC restarted\n"
    );
}

/// `eltc 0` owns the console, and the reason every boot line must go through
/// the errlog rather than a bare `eprintln!` is that this is the only switch
/// C gives an operator for them. Measured: `eltc 0` then `iocInit` left C's
/// stderr empty while the port kept writing — through raw `eprintln!` — the
/// record-count line (`ioc_app.rs`) and the CA server's port line
/// (`epics-ca-rs`'s `ca_server.rs`), both now routed here.
#[epics_macros_rs::epics_test]
async fn eltc_zero_silences_everything_the_build_says() {
    let (_out, err) = run("eltc 0\niocInit\n");
    assert_eq!(err, "", "eltc 0 leaves C's console empty");
}

/// `coreRelease` on its own still prints the same banner, so the assertion
/// above is about where the build prints and not about a mute shell.
#[epics_macros_rs::epics_test]
async fn core_release_still_prints_its_banner_on_stdout() {
    let (out, _err) = run("coreRelease\n");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first(), Some(&"coreRelease"), "stdout was {out:?}");
    assert_banner(&lines[1..], &out);
}
