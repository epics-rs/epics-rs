//! C `readReqFile` (save_restore.c:3553-3559, :3649) skips a `file` include
//! that does not open: it prints
//!
//! ```text
//! save_restore:readReqFile: unable to open file commonPlugin_settings.req. Exiting.
//! ```
//!
//! leaves the list's status FAIL (its text then overwritten by
//! `create_data_set`'s "Ready to connect...", :2481), and the list is
//! built with every other PV. Refusing the whole set lost every quadEM
//! IOC's `auto_settings` set, whose include of
//! `commonPlugin_settings.req` is not on the request path in C either.

use std::collections::HashMap;

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;
use epics_base_rs::server::autosave::save_set::{SaveSet, SrStatus};
use epics_base_rs::server::autosave::{BackupConfig, SaveSetConfig};

fn config(dir: &std::path::Path) -> SaveSetConfig {
    SaveSetConfig {
        name: "auto_settings".into(),
        save_path: dir.join("auto_settings.sav"),
        request_file: Some("auto_settings.req".into()),
        request_pvs: Vec::new(),
        backup: BackupConfig::default(),
        macros: HashMap::from([("P".into(), "IOC:".into())]),
        search_paths: vec![dir.to_path_buf()],
    }
}

#[epics_macros_rs::epics_test]
async fn a_set_whose_include_does_not_open_is_built_with_its_other_pvs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inner.req"), "$(P)inner\n").unwrap();
    std::fs::write(
        dir.path().join("auto_settings.req"),
        "$(P)first\nfile missing.req P=$(P)\nfile inner.req P=$(P)\n$(P)last\n",
    )
    .unwrap();
    let set = SaveSet::new(config(dir.path()))
        .await
        .expect("C builds the list");
    assert_eq!(set.pv_names(), vec!["IOC:first", "IOC:inner", "IOC:last"]);
    let status = set.list_status();
    assert_eq!(status.code, SrStatus::Fail);
    assert_eq!(status.text, "Ready to connect...");
}

#[epics_macros_rs::epics_test]
async fn a_set_whose_includes_all_open_is_ready_to_connect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inner.req"), "$(P)inner\n").unwrap();
    std::fs::write(
        dir.path().join("auto_settings.req"),
        "file inner.req P=$(P)\n",
    )
    .unwrap();
    let set = SaveSet::new(config(dir.path())).await.unwrap();
    let status = set.list_status();
    assert_eq!(status.code, SrStatus::Init);
    assert_eq!(status.text, "Ready to connect...");
}

/// A nested include that does not open is skipped at its own level only.
#[epics_macros_rs::epics_test]
async fn an_unopened_include_two_levels_down_keeps_both_outer_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mid.req"), "MID\nfile gone.req\nMID2\n").unwrap();
    std::fs::write(dir.path().join("top.req"), "TOP\nfile mid.req\nTOP2\n").unwrap();
    let entries = load_request_file_with_search_paths(
        "top.req",
        &[dir.path().to_path_buf()],
        &MacroContext::new(),
    )
    .await
    .unwrap()
    .entries;
    let names: Vec<_> = entries.iter().map(|e| e.pv_name.as_str()).collect();
    assert_eq!(names, ["TOP", "MID", "MID2", "TOP2"]);
}

/// The top-level file is still C's create failure.
#[epics_macros_rs::epics_test]
async fn a_top_level_file_that_does_not_open_still_fails() {
    let dir = tempfile::tempdir().unwrap();
    assert!(SaveSet::new(config(dir.path())).await.is_err());
}
