//! C's `loadedDb` gate: when `iocInit()` does not run, nothing is served.
//!
//! C `softMain.cpp:239` calls `iocInit()` only when `-d`/`-x` loaded a
//! database, and RSRV is started by `iocRun` — so a `softIoc` on the other
//! arm reaches `iocsh(NULL)` having built nothing and having opened no
//! port. This port ran the whole lifecycle unconditionally.
//!
//! The gate is therefore not a flag the protocol runner is asked to obey:
//! on [`IocInitDecision::skip`] the runner is never called, and the build
//! never announces its first hook. Both halves are asserted here, plus the
//! [`IocInitDecision::run`] boundary that must still do exactly what it
//! always did.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use epics_base_rs::server::ioc_app::init_hooks::init_hook_register;
use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision};

/// `interactive: true` rather than the spin: under a test harness stdin is
/// already at EOF, so `iocsh(NULL)` returns at once. The spin arm is C's
/// `while (true) epicsThreadSleep(1000.0)` and by construction never
/// returns, which is not a thing a test can outlive.
#[epics_macros_rs::epics_test]
async fn a_skipped_ioc_init_serves_nothing_and_builds_nothing() {
    let announced = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&announced);
    init_hook_register(Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    let served = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&served);
    IocApplication::new()
        .port(0)
        .before_ioc_init(|| IocInitDecision::skip(true))
        .run(move |_config| async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();

    assert!(
        !served.load(Ordering::SeqCst),
        "the protocol runner ran for an IOC that never called iocInit"
    );
    assert_eq!(
        announced.load(Ordering::SeqCst),
        0,
        "the build announced an init hook on the arm that does not build"
    );
}

/// The boundary: the default arm is unchanged, so every application that
/// never had C's question still boots and still hands off.
#[epics_macros_rs::epics_test]
async fn the_run_arm_still_builds_and_hands_off() {
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
