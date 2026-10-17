//! An AD IOC's auto_settings.req names `commonPlugin_settings.req`, which C
//! finds through the request paths EXAMPLE_commonPlugins.cmd sets
//! (`$(ADCORE)/iocBoot` for the deployed copy, `$(ADCORE)/db` for the
//! plugin files it includes, :165-168), plus the `$(CALC)` path the IOC's
//! own saveRestore.cmd adds for NDStats' sseq_settings.req (quadEM
//! saveRestore.cmd:4).
#![cfg(feature = "ioc")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;

/// The `set_requestfile_path` directories commonPlugins.cmd sets that
/// ad-core-rs owns, in order.
fn request_paths() -> Vec<PathBuf> {
    let dir = Path::new(ad_core_rs::AD_CORE_DIR);
    let text = std::fs::read_to_string(dir.join("iocBoot/commonPlugins.cmd")).unwrap();
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("set_requestfile_path(\""))
        .filter_map(|l| l.split('"').next())
        .filter_map(|p| p.strip_prefix("$(ADCORE)"))
        .map(|p| dir.join(p.trim_start_matches('/')))
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
    let mut paths = request_paths();
    paths.push(PathBuf::from(std::env::var("CALC").unwrap()).join("db"));
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
