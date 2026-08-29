//! MQ4: `IocApplication::run` runs the process's shutdown callbacks on every
//! way out, so anything with teardown to do finally gets to do it.
//!
//! There was no owner. A driver whose `Drop` sends its device a goodbye — an
//! MQTT DISCONNECT — had that `Drop` written and never reached: nothing at IOC
//! exit dropped it. The gap was general, not MQTT's; every subsystem with
//! teardown had it.
//!
//! C's arrangement is the one ported here: `softIoc`'s `main` reaches all six
//! of its exits through `epicsExit(status)` (`softMain.cpp:167`, `:172`,
//! `:251`, `:265`, `:270`, `:277`), and `epicsExit` runs the registered list
//! before `exit()` (`epicsExit.c:172-177`). Subsystems register at the point
//! they are created and stay ignorant of shutdown; `run` is where the list is
//! run.
//!
//! Both of `run`'s exit shapes are pinned below — the runner returning, and a
//! failure that returns long before there is a runner — because a finalizer
//! that only covers the happy path is the defect wearing a fix.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::runtime::exit::at_exit;
use epics_base_rs::server::ioc_app::IocApplication;

#[epics_macros_rs::epics_test]
async fn every_way_out_of_run_runs_the_shutdown_callbacks() {
    let ran = Arc::new(AtomicUsize::new(0));

    // ---- the runner finishes ------------------------------------------
    let counter = ran.clone();
    at_exit("mq4-normal-return", move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    IocApplication::new()
        .run(|_config| async move { Ok(()) })
        .await
        .expect("an IOC with a runner that returns Ok must return Ok");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "run must run the shutdown callbacks when its runner returns"
    );

    // ---- a failure long before the runner ------------------------------
    //
    // A missing startup script fails inside the lifecycle, at a `?` roughly
    // four hundred lines above the runner. The callbacks must still run: an
    // IOC that dies during boot has as much to tear down as one that dies
    // after it, and this is the exit shape a finalizer is easiest to forget.
    let counter = ran.clone();
    at_exit("mq4-boot-failure", move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let booted = IocApplication::new()
        .startup_script("/nonexistent/mq4-no-such-st.cmd")
        .run(|_config| async move { Ok(()) })
        .await;
    assert!(
        booted.is_err(),
        "a missing startup script must fail the boot, or this case tests \
         nothing about the failure path"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        2,
        "run must run the shutdown callbacks when the lifecycle fails too"
    );
}
