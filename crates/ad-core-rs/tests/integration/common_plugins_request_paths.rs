//! An AD IOC's auto_settings.req names `commonPlugin_settings.req`, which C
//! finds through the request paths EXAMPLE_commonPlugins.cmd sets
//! (`$(ADCORE)/iocBoot` for the deployed copy, `$(ADCORE)/db` for the
//! plugin files it includes, :165-168). NDStats' sseq_settings.req is on
//! `$(CALC)/db`, which C's file leaves commented (:211) and ours sets.
#![cfg(feature = "ioc")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;

/// The `set_requestfile_path` directories commonPlugins.cmd sets, in order,
/// each `$(NAME)` as the IOC defines it.
fn request_paths() -> Vec<PathBuf> {
    let dir = Path::new(ad_core_rs::AD_CORE_DIR);
    let text = std::fs::read_to_string(dir.join("iocBoot/commonPlugins.cmd")).unwrap();
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("set_requestfile_path(\""))
        .filter_map(|l| l.split('"').next())
        .map(
            |p| match p.strip_prefix("$(").and_then(|p| p.split_once(')')) {
                Some(("ADCORE", rest)) => dir.join(rest.trim_start_matches('/')),
                Some((name, rest)) => {
                    PathBuf::from(std::env::var(name).unwrap()).join(rest.trim_start_matches('/'))
                }
                None => PathBuf::from(p),
            },
        )
        .collect()
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// ad-core-rs --all-features`); it builds no CA server, and its reactor is
// `#[tokio::test]`'s own, which the backend does not remove.
#[tokio::test]
async fn common_plugin_settings_resolves_through_common_plugins_paths() {
    // SAFETY: nextest gives each test its own process, and this runs before
    // any thread is spawned.
    unsafe { std::env::remove_var("CALC") };
    let _app =
        IocApplication::new().autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())));
    let paths = request_paths();
    let macros = MacroContext::from_map([("P".to_string(), "13SIM1:".to_string())].into());
    let list = load_request_file_with_search_paths("commonPlugin_settings.req", &paths, &macros)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        !list.include_unopened,
        "an included request file is missing"
    );
    assert!(!list.entries.is_empty());
}

// RTEMS-EXEC-MODEL-ALLOW(1): as above — no CA server, `#[tokio::test]`'s own
// reactor.
/// The examples' `commonPlugins_settings.req` names each include quoted, as
/// C EXAMPLE_commonPlugin_settings.req does: C `readReqFile` keeps a comma
/// after an unquoted name in the name (save_restore.c:3630-3634).
#[tokio::test]
async fn examples_common_plugins_settings_opens_every_include() {
    // SAFETY: nextest gives each test its own process, and this runs before
    // any thread is spawned.
    unsafe { std::env::remove_var("CALC") };
    let _app =
        IocApplication::new().autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())));
    let paths = request_paths();
    let macros = MacroContext::from_map([("P".to_string(), "13SIM1:".to_string())].into());
    let list = load_request_file_with_search_paths("commonPlugins_settings.req", &paths, &macros)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        !list.include_unopened,
        "an included request file is missing"
    );
}
