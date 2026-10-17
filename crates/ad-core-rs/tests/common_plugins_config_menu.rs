//! C's EXAMPLE_commonPlugins.cmd:174 loads autosave's configMenu.db with
//! `CONFIG=ADAutoSave`, the records ADCore's ADAutoSaveMenu.req saves.
#![cfg(feature = "ioc")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::config_menu;
use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::sseq::SseqRecord;

const PREFIX: &str = "13SIM1:";

/// The `dbLoadRecords("$(AUTOSAVE)/...", "...")` lines of commonPlugins.cmd,
/// as (file, macros) with `$(PREFIX)` expanded.
fn autosave_loads(autosave: &Path) -> Vec<(PathBuf, HashMap<String, String>)> {
    let path = Path::new(ad_core_rs::AD_CORE_DIR).join("iocBoot/commonPlugins.cmd");
    let text = std::fs::read_to_string(path).unwrap();
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("dbLoadRecords(\"$(AUTOSAVE)/"))
        .map(|l| {
            let mut parts = l.split('"');
            let file = autosave.join(parts.next().unwrap());
            let macros = parts
                .nth(1)
                .unwrap_or("")
                .replace("$(PREFIX)", PREFIX)
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect();
            (file, macros)
        })
        .collect()
}

#[tokio::test]
async fn ad_autosave_menu_saves_records_common_plugins_loads() {
    // SAFETY: nextest gives each test its own process, and this runs before
    // the IOC spawns any thread.
    unsafe { std::env::remove_var("AUTOSAVE") };
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    // Publishes `$(AUTOSAVE)` as an rs IOC's startup does.
    let _app = IocApplication::new().autosave_startup(holder.clone());
    let autosave = PathBuf::from(std::env::var("AUTOSAVE").unwrap());
    // The record types an AD IOC adds to the default set (register_all_plugins).
    let mut b = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("sseq", || Box::new(SseqRecord::default()));
    for (name, func) in config_menu::subroutines(holder) {
        b = b.register_context_subroutine(name, func);
    }
    for (file, macros) in autosave_loads(&autosave) {
        b = b
            .db_file(file.to_str().unwrap(), &macros)
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    }
    let db = b.build().await.unwrap().0;

    // The IOC's "./", holding the auto_settings.req ADAutoSaveMenu.req includes.
    let ioc_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("config_menu_ioc");
    std::fs::create_dir_all(&ioc_dir).unwrap();
    std::fs::write(ioc_dir.join("auto_settings.req"), "").unwrap();
    let paths = [
        ioc_dir,
        Path::new(ad_core_rs::AD_CORE_DIR).join("db"),
        autosave.join("db"),
    ];
    let macros = MacroContext::from_map([("P".to_string(), PREFIX.to_string())].into());
    let list = load_request_file_with_search_paths("ADAutoSaveMenu.req", &paths, &macros)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        !list.include_unopened,
        "an included request file is missing"
    );
    assert!(!list.entries.is_empty());
    for e in &list.entries {
        let record = e.pv_name.split('.').next().unwrap();
        assert!(
            db.get_record(record).is_some(),
            "ADAutoSaveMenu.req saves {record}, which commonPlugins.cmd does not load"
        );
    }
}
