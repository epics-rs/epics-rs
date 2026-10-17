//! delayDo_settings.req includes calc's calcout_settings.req and
//! sseq_settings.req, which a C IOC finds on its `$(CALC)` request path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;
use epics_base_rs::server::ioc_app::IocApplication;

#[tokio::test]
async fn delay_do_settings_saves_c_set() {
    // SAFETY: nextest gives each test its own process, and this runs before
    // any thread is spawned.
    unsafe { std::env::remove_var("CALC") };
    let _app =
        IocApplication::new().autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())));
    let calc = PathBuf::from(std::env::var("CALC").expect("autosave_startup publishes CALC"));
    let paths = [
        Path::new(env!("CARGO_MANIFEST_DIR")).join("db"),
        calc.join("db"),
    ];
    let macros = MacroContext::from_map(
        [("P", "x:"), ("R", "dly")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into(),
    );
    let list = load_request_file_with_search_paths("delayDo_settings.req", &paths, &macros)
        .await
        .unwrap();
    assert!(
        !list.include_unopened,
        "an included request file is missing"
    );
    // C: 3 calcouts x 39 fields, one sseq x 67, and delayDo's own 2.
    assert_eq!(list.entries.len(), 3 * 39 + 67 + 2);
}
