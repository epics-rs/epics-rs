//! An IOC whose own `main` runs `iocsh(st.cmd)` then `iocsh(NULL)` — every
//! IOC that is not softIoc — is initialised by the script's `iocInit` and by
//! nothing else. A script that never says it leaves the IOC unbuilt at the
//! prompt; one that says only `iocBuild` leaves it built and not running.
//! The application with no script at all is the programmatic `main` that
//! calls `iocInit()` itself, so `run` builds it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};

/// How `run_outcome` ended for an IOC whose startup script is `lines`, or
/// which has none, checked against whether the protocol runner — the server
/// start — was reached. Under a test harness stdin is at EOF, so the unbuilt
/// IOC's `iocsh(NULL)` returns at once.
async fn served(lines: Option<&[&str]>) -> RunOutcome {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = IocApplication::new().port(0);
    if let Some(lines) = lines {
        let script = dir.path().join("st.cmd");
        std::fs::write(&script, lines.join("\n") + "\n").expect("write script");
        app = app.startup_script(script.to_str().unwrap());
    }
    let served = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&served);
    let outcome = app
        .run_outcome(move |_config| async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        served.load(Ordering::SeqCst),
        outcome == RunOutcome::Served,
        "{outcome:?}"
    );
    outcome
}

#[epics_macros_rs::epics_test]
async fn a_script_that_never_says_ioc_init_serves_nothing() {
    assert_eq!(
        served(Some(&["epicsEnvSet A 1"])).await,
        RunOutcome::NeverInitialised
    );
}

#[epics_macros_rs::epics_test]
async fn a_script_that_only_builds_serves_nothing() {
    assert_eq!(
        served(Some(&["iocBuild"])).await,
        RunOutcome::NeverInitialised
    );
}

#[epics_macros_rs::epics_test]
async fn a_script_that_says_ioc_init_serves() {
    assert_eq!(served(Some(&["iocInit"])).await, RunOutcome::Served);
}

#[epics_macros_rs::epics_test]
async fn an_application_with_no_script_is_built_by_run() {
    assert_eq!(served(None).await, RunOutcome::Served);
}
