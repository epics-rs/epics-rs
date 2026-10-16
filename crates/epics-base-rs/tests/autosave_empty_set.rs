//! C builds a list from a request file that declares no PVs, and saves it:
//! `get_channel_values` finds nothing unconnected, so `write_save_file`
//! writes the header and `<END>` (save_restore.c:998-1004).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSet, SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;

fn config(dir: &std::path::Path) -> SaveSetConfig {
    SaveSetConfig {
        name: "auto_settings".into(),
        save_path: dir.join("auto_settings.sav"),
        request_file: Some("auto_settings.req".into()),
        request_pvs: Vec::new(),
        backup: BackupConfig::default(),
        macros: HashMap::new(),
        search_paths: vec![dir.to_path_buf()],
    }
}

#[epics_macros_rs::epics_test]
async fn a_request_file_declaring_no_pvs_builds_and_saves_an_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("auto_settings.req"),
        "# every line here is a comment\n#IOC:setpoint\n",
    )
    .unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(
                config(dir.path()),
                SaveStrategy::Periodic {
                    interval: Duration::from_millis(20),
                },
            )
            .build()
            .await,
    );
    assert_eq!(mgr.set_names(), vec!["auto_settings"]);

    let db = Arc::new(PvDatabase::new());
    let reactor = epics_base_rs::runtime::task::Reactor::current().unwrap();
    let handle = mgr.clone().start(&reactor, db);
    let sav = dir.path().join("auto_settings.sav");
    for _ in 0..100 {
        if std::fs::read_to_string(&sav).is_ok_and(|t| t.contains("<END>")) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    mgr.shutdown();
    let _ = handle.await;
    let text = std::fs::read_to_string(&sav).expect("the empty list was saved");
    let body: Vec<_> = text.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(body, ["<END>"], "{text}");
}

/// `reload_request` of an emptied file installs the empty list, as C's
/// remove-and-recreate does.
#[epics_macros_rs::epics_test]
async fn reload_request_installs_an_emptied_list() {
    let dir = tempfile::tempdir().unwrap();
    let req = dir.path().join("auto_settings.req");
    std::fs::write(&req, "IOC:setpoint\n").unwrap();
    let mut set = SaveSet::new(config(dir.path())).await.unwrap();
    assert_eq!(set.pv_names(), vec!["IOC:setpoint"]);
    std::fs::write(&req, "# emptied\n").unwrap();
    set.reload_request().await.unwrap();
    assert!(set.pv_names().is_empty());
}
