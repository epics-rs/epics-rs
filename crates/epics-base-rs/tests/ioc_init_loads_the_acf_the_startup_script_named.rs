//! `iocInit` is what reads the ACF, so an `st.cmd` that names its own gets
//! access security.
//!
//! C `iocBuild_2` calls `asInit()` after `scanInit()` (`iocInit.c:186-191`)
//! — the startup script has already run by then, so `asSetFilename` from
//! either argv or the script reaches the same load. This port had only the
//! iocsh command, and `softioc-rs` stood in for the build's call by queuing
//! a literal `asInit` line after the argv-derived ones. That ran BEFORE the
//! script, so a script naming its own ACF booted with access security off
//! and a `-a` file was read at the wrong instant.

use std::io::Write;

use epics_base_rs::server::ioc_app::IocApplication;

#[epics_macros_rs::epics_test]
async fn a_startup_script_s_own_acf_is_live_by_the_time_the_server_starts() {
    let mut acf = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
    writeln!(
        acf,
        "UAG(ops) {{ alice }}\nASG(DEFAULT) {{ RULE(1, READ) }}"
    )
    .unwrap();
    let path = acf.path().to_string_lossy().to_string();

    IocApplication::new()
        .port(0)
        .startup_line(&format!("asSetFilename(\"{path}\")"))
        .run(|config| async move {
            assert!(
                config.acf.load().is_some(),
                "iocInit did not read the ACF the startup phase named"
            );
            Ok(())
        })
        .await
        .unwrap();
}

/// C `asDbLib.c:127`: a first `asInit` with no `asSetFilename` returns 0
/// without a word, so an IOC that never mentions access security still
/// builds. The boundary that keeps the call above from failing every boot.
#[epics_macros_rs::epics_test]
async fn an_ioc_that_names_no_acf_still_builds() {
    IocApplication::new()
        .port(0)
        .run(|config| async move {
            assert!(config.acf.load().is_none(), "no ACF was ever named");
            Ok(())
        })
        .await
        .unwrap();
}
