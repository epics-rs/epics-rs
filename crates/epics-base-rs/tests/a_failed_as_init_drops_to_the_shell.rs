//! A failed `asInit` does not terminate the IOC.
//!
//! `iocBuild_2` returns -1 when `asInit()` is non-zero (`iocInit.c:187-191`),
//! so `iocBuild` does, so `iocInit()` does — and C softMain treats a non-zero
//! `iocInit()` as something to REPORT, not to die of: it prints
//! `ERL_ERROR " during iocInit()"` and falls straight through to the tail
//! (`softMain.cpp:239-243`). `iocRun` never ran, so RSRV was never started and
//! the process answers nothing.
//!
//! Measured against `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`
//! R7.0.10.1-DEV, `-a <unreadable>.acf -d test.db`:
//!
//! * interactive, stdin `/dev/null` — reaches `epics> ` and exits **0**
//! * `-S` — `timeout 6` returns 124 (still alive) and `ss -lntp` counts
//!   **0** listeners on the configured CA port
//!
//! This port exited 2 from both.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision};

/// `interactive: true` for the same reason the skipped-arm test uses it:
/// under a test harness stdin is already at EOF, so C's `iocsh(NULL)` returns
/// at once and the process ends the way C's `epicsExit(0)` does. The `-S` arm
/// is `while (true) epicsThreadSleep(1000.0)` and by construction never
/// returns, which is not a thing a test can outlive.
#[epics_macros_rs::epics_test]
async fn a_failed_as_init_reaches_the_shell_and_serves_nothing() {
    let served = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&served);

    let outcome = IocApplication::new()
        .port(0)
        // C `-a /no/such.acf`: `asSetFilename` has no immediate effect, and
        // `iocBuild_2`'s `asInit()` is what fails on it.
        .startup_line(r#"asSetFilename("/no/such/directory/nope.acf")"#)
        .before_ioc_init(|| IocInitDecision::run(true))
        .run(move |_config| async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;

    assert!(
        outcome.is_ok(),
        "a failed asInit must reach the shell and end the way C's \
         epicsExit(0) does, got {outcome:?}"
    );
    assert!(
        !served.load(Ordering::SeqCst),
        "iocRun never runs after a failed iocBuild, so nothing may be served"
    );
}

/// The boundary the arm above must not have moved: an `asInit` that succeeds
/// still builds and still hands off to the protocol runner.
#[epics_macros_rs::epics_test]
async fn a_succeeding_as_init_still_hands_off() {
    let served = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&served);

    IocApplication::new()
        .port(0)
        .before_ioc_init(|| IocInitDecision::run(true))
        .run(move |_config| async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();

    assert!(served.load(Ordering::SeqCst), "the Run arm must hand off");
}
