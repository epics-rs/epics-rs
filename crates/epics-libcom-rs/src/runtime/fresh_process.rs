//! Running one test in a process of its own.
//!
//! Some of this crate's subjects are process one-shots — state that can be
//! set once per image and never put back, because that is what the C original
//! does too:
//!
//! - `tracing`'s global max level, a one-way latch raised by the first
//!   subscriber installed (a scoped `with_default` counts) and never lowered,
//!   which is what [`log::nothing_is_listening`](super::log) reads.
//! - [`log_client::ioc_log_init`](super::log_client)'s `CLIENT`, C's
//!   `iocLogInit` calling `logClientCreate` once per IOC.
//!
//! A test whose subject is the *first* such call cannot share a process with
//! another test that makes one: the second call is correctly refused, and the
//! test then asserts against a refusal rather than against its subject. Under
//! `cargo nextest` — this workspace's runner — every test has a process of its
//! own and the question never comes up; under `cargo test`, which shares one,
//! these tests failed for that reason and no other, and one of them hung
//! waiting on a connection the refused init never made.
//!
//! The fix is to stop depending on the runner: the test re-executes the test
//! binary for itself.

/// Run `body` in a process of its own, and report there what it did.
///
/// `test` is the full path of the calling test — `module::tests::name` — since
/// that is what the child is given to run. Pass the same string the test is
/// declared with; a typo makes the child run nothing and pass, so the caller
/// is the only one who can get this right.
///
/// The child is told which test it is through the environment and compares the
/// value, so a child never re-executes a second time and two of these tests
/// cannot be confused for one another.
#[cfg(not(any(target_os = "rtems", target_os = "vxworks")))]
pub(crate) fn with_a_process_of_its_own(test: &str, body: impl FnOnce()) {
    const ROLE: &str = "EPICS_RS_FRESH_PROCESS";
    if std::env::var(ROLE).as_deref() == Ok(test) {
        body();
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", test, "--nocapture"])
        .env(ROLE, test)
        .output()
        .expect("re-exec the test binary");
    assert!(
        out.status.success(),
        "{test} failed in a process of its own: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The embedded targets have no process to spawn, so there a test is only as
/// isolated as its runner makes it.
#[cfg(any(target_os = "rtems", target_os = "vxworks"))]
pub(crate) fn with_a_process_of_its_own(_test: &str, body: impl FnOnce()) {
    body();
}
