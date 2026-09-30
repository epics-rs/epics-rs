//! C `cantProceed` (`misc/cantProceed.c:55-83`) — the single exit taken by a
//! thread that has reached a state it cannot continue from and cannot report.
//!
//! The port had no such exit. Each site that needed one wrote its own
//! `assert_eq!`, which panics: an unwind out of a half-initialised primitive,
//! whose outcome then depends on the panic strategy, on whether the frame is
//! already unwinding, and on whether some caller happens to sit inside a
//! `catch_unwind`. None of those are the behaviour C specifies, and the choice
//! between them was made independently at four sites.
//!
//! Upstream `3206c817c` "libcom: Make EPICS_ABORT_ON_ASSERT affect
//! cantProceed" is the reason the exit has to be one owner rather than a
//! convention: `EPICS_ABORT_ON_ASSERT` existed to kill the whole IOC instead of
//! parking one thread, but only `epicsAssert` honoured it, so the
//! `epicsMustBlah` family kept suspending whatever the operator had set. A
//! knob that is read in one place cannot drift that way.

use crate::runtime::env_table::EPICS_ABORT_ON_ASSERT;
use crate::runtime::log::{errlog_flush, errlog_printf};
use crate::runtime::task;

/// What a thread that cannot proceed does once it has logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalAction {
    /// `abort()` — the whole process dies, and a supervisor sees it.
    Abort,
    /// `epicsThreadSuspendSelf` forever — the process lives on with one
    /// thread parked, which is what `epicsThreadShowAll` is there to show.
    SuspendSelf,
}

/// The `EPICS_ABORT_ON_ASSERT` decision, split out from [`cant_proceed`] so the
/// knob boundaries are testable without killing the test process.
///
/// `None` is C's -1 status from `envGetBoolConfigParam`, which leaves the
/// caller's `shouldAbort` at its `0` initialiser (`cantProceed.c:57`,
/// `osdAssert.c:57`) — so an unresolvable knob suspends, it does not abort.
fn action_for(abort_on_assert: Option<bool>) -> FatalAction {
    match abort_on_assert {
        Some(true) => FatalAction::Abort,
        Some(false) | None => FatalAction::SuspendSelf,
    }
}

/// What [`cant_proceed`] would do right now. Exposed because a caller that
/// wants to say "and this will abort the IOC" in its own diagnostics must not
/// re-read the knob to find out.
pub fn fatal_action() -> FatalAction {
    action_for(EPICS_ABORT_ON_ASSERT.bool())
}

/// C `cantProceed(msg, ...)` — log `msg`, then leave by whichever exit
/// `EPICS_ABORT_ON_ASSERT` names. Never returns.
///
/// The message goes out through `errlog_printf` and is followed by
/// `errlog_flush`, so it reaches the console even on an image with no tracing
/// subscriber: that is what makes this usable from a primitive that is too
/// broken to return a `Result`. C's one-second pause before the exit is kept —
/// it is what gets the bytes onto a slow serial console ahead of `abort()`.
pub fn cant_proceed(msg: &str) -> ! {
    let action = fatal_action();
    errlog_printf(&format!("{msg}\n"));
    errlog_printf(&format!(
        "CRITICAL ERROR Thread {} can't proceed, {}.\n",
        std::thread::current().name().unwrap_or("noname"),
        match action {
            FatalAction::Abort => "aborting",
            FatalAction::SuspendSelf => "suspending",
        },
    ));
    errlog_flush();
    std::thread::sleep(std::time::Duration::from_secs(1));

    match action {
        FatalAction::Abort => {
            errlog_printf("cant_proceed() calling abort()\n");
            errlog_flush();
            std::process::abort()
        }
        // C loops because `epicsThreadResume` can be called on a parked
        // thread: resuming one that cannot proceed must park it again, not let
        // it run on.
        FatalAction::SuspendSelf => loop {
            task::suspend_self();
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_knob_aborts_only_on_the_word_yes() {
        // `EnvParam::bool` is `epicsStrCaseCmp(text, "yes") == 0`, so these are
        // the only two inputs `action_for` can be handed for a resolved knob.
        assert_eq!(action_for(Some(true)), FatalAction::Abort);
        assert_eq!(action_for(Some(false)), FatalAction::SuspendSelf);
    }

    #[test]
    fn an_unresolvable_knob_suspends() {
        assert_eq!(action_for(None), FatalAction::SuspendSelf);
    }

    /// The owner path, both exits, measured on the **process**.
    ///
    /// A test that only asserted "it does not return" would pass against the
    /// defect this closes: the four sites it replaces panicked, and on RTEMS
    /// and VxWorks — both `panic = "unwind"` — a panicked thread leaves the
    /// process running. So the child is re-executed and the parent asserts
    /// what the operator actually observes: `SIGABRT` with the knob set, and a
    /// live process with one parked thread without it.
    ///
    /// The knob is set on the child's environment rather than this process's,
    /// so the assertion does not depend on what the developer exported and
    /// cannot leak into another test.
    ///
    /// Gated off the embedded targets: they have no process to spawn.
    #[cfg(all(unix, not(target_os = "rtems"), not(target_os = "vxworks")))]
    #[test]
    fn the_knob_decides_whether_the_process_or_one_thread_dies() {
        use std::os::unix::process::ExitStatusExt;

        const ROLE: &str = "EPICS_RS_CANT_PROCEED_CHILD";
        const TEST: &str =
            "runtime::cant_proceed::tests::the_knob_decides_whether_the_process_or_one_thread_dies";
        const MSG: &str = "probe: a primitive that cannot report";

        match std::env::var(ROLE).as_deref() {
            Ok("abort") => cant_proceed(MSG),
            Ok("suspend") => {
                let parked = std::thread::Builder::new()
                    .name("cant-proceed-probe".into())
                    .spawn(|| cant_proceed(MSG))
                    .expect("spawn the probe thread");
                // Well past the one-second pause `cant_proceed` takes before
                // it leaves, so "still running" means parked and not merely
                // mid-log.
                std::thread::sleep(std::time::Duration::from_secs(3));
                assert!(
                    !parked.is_finished(),
                    "a suspended thread must never come back"
                );
                eprintln!("parent is alive");
                return;
            }
            _ => {}
        }

        let child = |role: &str| {
            std::process::Command::new(std::env::current_exe().expect("the test binary path"))
                .args(["--exact", TEST, "--nocapture"])
                .env(ROLE, role)
                .env(
                    "EPICS_ABORT_ON_ASSERT",
                    if role == "abort" { "yes" } else { "no" },
                )
                .output()
                .expect("re-exec the test binary")
        };

        let aborted = child("abort");
        let stderr = String::from_utf8_lossy(&aborted.stderr);
        assert_eq!(
            aborted.status.signal(),
            Some(libc::SIGABRT),
            "`EPICS_ABORT_ON_ASSERT=yes` must kill the process, not one thread              — child exited {:?}, stderr: {stderr}",
            aborted.status
        );
        assert!(
            stderr.contains(MSG),
            "the caller's message must reach the console before the exit; got: {stderr}"
        );
        assert!(
            stderr.contains("aborting"),
            "the console must say which exit was taken; got: {stderr}"
        );

        let suspended = child("suspend");
        let stderr = String::from_utf8_lossy(&suspended.stderr);
        assert!(
            suspended.status.success(),
            "without the knob the process must survive one parked thread              — child exited {:?}, stderr: {stderr}",
            suspended.status
        );
        assert!(
            stderr.contains("suspending"),
            "the console must say which exit was taken; got: {stderr}"
        );
    }

    #[test]
    fn the_compiled_default_suspends() {
        // The table default is `"NO"` (`env_table.rs`), so an IOC nobody has
        // configured parks the thread — C's behaviour before the knob existed.
        assert_eq!(
            action_for(Some(
                EPICS_ABORT_ON_ASSERT
                    .default_str()
                    .eq_ignore_ascii_case("yes")
            )),
            FatalAction::SuspendSelf
        );
    }
}
