//! ADCore's NDStats_settings.req saves the three Reset sseq records
//! NDStats.template loads, through calc's sseq_settings.req.
#![cfg(feature = "ioc")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;
use epics_base_rs::server::ioc_app::IocApplication;

#[tokio::test]
async fn nd_stats_settings_saves_reset_sseqs() {
    // SAFETY: nextest gives each test its own process, and this runs before
    // any thread is spawned.
    unsafe { std::env::remove_var("CALC") };
    let _app =
        IocApplication::new().autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())));
    let calc = PathBuf::from(std::env::var("CALC").unwrap());
    let paths = [
        Path::new(ad_core_rs::AD_CORE_DIR).join("db"),
        calc.join("db"),
    ];
    let macros = MacroContext::from_map(
        [("P", "13SIM1:"), ("R", "Stats1:")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into(),
    );
    let list = load_request_file_with_search_paths("NDStats_settings.req", &paths, &macros)
        .await
        .unwrap();
    assert!(
        !list.include_unopened,
        "an included request file is missing"
    );
    for s in ["Reset", "Reset1", "Reset2"] {
        let prefix = format!("13SIM1:Stats1:{s}.");
        let n = list
            .entries
            .iter()
            .filter(|e| e.pv_name.starts_with(&prefix))
            .count();
        assert_eq!(n, 67, "{prefix}: sseq_settings.req saves 67 fields");
    }
}
