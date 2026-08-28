//! What C softIoc puts in the database before the script's first line.
//!
//! C `softMain.cpp:161-222` reads argv and acts on it as it goes: `-d` is
//! `dbLoadRecords(optarg, macros)` called there and then, guarded by
//! `errIf(..., "")`, and only afterwards does `:225-233` hand the positional
//! `st.cmd` to `iocsh`. Measured on R7.0.10: `softIoc -S -d good.db st.cmd`
//! runs, and a `dbl` on the script's second line lists `good.db`'s record.
//!
//! `IocApplication` is where that order lives in this port, so these are its
//! tests rather than the IOC binary's.

use epics_base_rs::server::ioc_app::{IocApplication, IocRunFailure};
use epics_base_rs::types::EpicsValue;

/// The `-d` shape: a queued line runs, and it runs BEFORE the script.
#[epics_macros_rs::epics_test]
async fn a_queued_line_runs_before_the_startup_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("order.txt");
    let script = dir.path().join("st.cmd");
    std::fs::write(
        &script,
        format!("epicsEnvShow EPICS_RS_ARGV_ORDER > {}\n", out.display()),
    )
    .expect("write script");

    IocApplication::new()
        .startup_line("epicsEnvSet EPICS_RS_ARGV_ORDER before-the-script")
        .startup_script(script.to_str().unwrap())
        .run_phased(|_config| async move { Ok(()) })
        .await
        .expect("the queued line and the script both succeed");

    let shown = std::fs::read_to_string(&out).expect("the script wrote its redirect");
    assert!(
        shown.contains("before-the-script"),
        "the script saw the queued line's effect, got {shown:?}"
    );
}

/// C `errIf(dbLoadRecords(...), "")` throws, so the boot stops at the failing
/// line and the script never runs. The line rides along for the same reason
/// the script path does: the caller should not have to re-derive it.
#[epics_macros_rs::epics_test]
async fn a_failing_queued_line_stops_the_boot_before_the_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ran = dir.path().join("ran.txt");
    let script = dir.path().join("st.cmd");
    std::fs::write(&script, format!("epicsEnvShow PATH > {}\n", ran.display()))
        .expect("write script");

    let failure = IocApplication::new()
        .startup_line("dbLoadRecords(\"/nonexistent/argv-no-such.db\")")
        .startup_script(script.to_str().unwrap())
        .run_phased(|_config| async move { Ok(()) })
        .await
        .expect_err("a failing pre-script line ends the boot");
    match failure {
        IocRunFailure::StartupCommand { line, .. } => {
            assert_eq!(line, "dbLoadRecords(\"/nonexistent/argv-no-such.db\")");
        }
        other => panic!("a failing queued line is its own phase, got {other:?}"),
    }
    assert!(
        !ran.exists(),
        "the script must not run after the queued line failed"
    );
}

/// A queued line is NOT macro-expanded: C calls the command directly with
/// argv's bytes, so a `$(` in a filename reaches it as typed instead of
/// being resolved — or rejected — by macLib first.
#[epics_macros_rs::epics_test]
async fn a_queued_line_is_not_macro_expanded() {
    let failure = IocApplication::new()
        .startup_line("dbLoadRecords(\"/nonexistent/$(UNDEFINED_MACRO).db\")")
        .run_phased(|_config| async move { Ok(()) })
        .await
        .expect_err("the file does not exist");
    match failure {
        IocRunFailure::StartupCommand { line, .. } => {
            assert!(
                line.contains("$(UNDEFINED_MACRO)"),
                "the line reached the command unexpanded, got {line:?}"
            );
        }
        other => panic!("expected the queued-line phase, got {other:?}"),
    }
}

/// `--pv`'s half of the same instant: a simple PV is in the database when
/// the protocol runner receives it, on the script route as on the builder's.
#[epics_macros_rs::epics_test]
async fn an_inline_pv_reaches_the_protocol_runner() {
    IocApplication::new()
        .pv("ARGV:INLINE:PV", EpicsValue::Double(2.5))
        .run_phased(|config| async move {
            assert!(
                config.db.find_pv("ARGV:INLINE:PV").await.is_some(),
                "the inline PV was created before the runner started"
            );
            Ok(())
        })
        .await
        .expect("the IOC ran");
}
