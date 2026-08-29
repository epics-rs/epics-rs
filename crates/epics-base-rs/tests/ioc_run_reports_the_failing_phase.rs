//! `run_phased` names the phase, because C softIoc's exit status is the phase.
//!
//! C runs its whole boot inside one `try` whose `catch` exits 2, and only
//! after that block closes does it start serving — `iocsh(NULL)`, which exits
//! 1 when it fails (`softMain.cpp:247-279`). The same `CaError` therefore
//! means two different statuses depending on where it came from, and a caller
//! reading the message to work out which is guessing at something `run`
//! already knows. These are the two sides of that boundary.

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_app::{IocApplication, IocRunFailure};

/// Past C's `try` block: `softMain.cpp:250-253` exits 1 here.
#[epics_macros_rs::epics_test]
async fn a_protocol_runner_failure_is_the_serving_phase() {
    let failure = IocApplication::new()
        .run_phased(|_config| async move { Err(CaError::Shutdown) })
        .await
        .expect_err("the runner returned an error");
    assert!(
        matches!(failure, IocRunFailure::Serving(CaError::Shutdown)),
        "a runner error is the serving phase, got {failure:?}"
    );
}

/// Inside it: `softMain.cpp:231` throws `Error in <path>` and the catch exits
/// 2. The path rides along so the caller can say it C's way without going back
/// to its own argv.
#[epics_macros_rs::epics_test]
async fn an_unreadable_script_is_the_startup_script_phase() {
    let failure = IocApplication::new()
        .startup_script("/nonexistent/phase-no-such-st.cmd")
        .run_phased(|_config| async move { Ok(()) })
        .await
        .expect_err("the script cannot be opened");
    match failure {
        IocRunFailure::StartupScript { path, .. } => {
            assert_eq!(path, "/nonexistent/phase-no-such-st.cmd");
        }
        other => panic!("a failing st.cmd is the script phase, got {other:?}"),
    }
}

/// A runner that never fails leaves no phase to report.
#[epics_macros_rs::epics_test]
async fn a_clean_run_reports_no_phase() {
    IocApplication::new()
        .run_phased(|_config| async move { Ok(()) })
        .await
        .expect("an IOC whose runner returns Ok must return Ok");
}
