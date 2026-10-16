//! C's save task connects a list before enabling it, `connect_list(plist, 1)`
//! (save_restore.c:1276-1374), and says on the console how it went:
//!
//! ```text
//! save_restore:connect_list: not all searches successful
//! save_restore: connect failed for channel 'GONE1'
//! auto_settings.sav: 1 of 2 PV's connected
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_CONNECT_LIST_CHILD";

/// Run the `child_*` test `name` in a fresh process and return its stdout's
/// autosave lines, which an in-process listener cannot hear.
fn console_of(name: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {out:?}");
    let prefix = format!("test {name} ... ");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        // libtest's own line runs on into the first print.
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l))
        .filter(|l| l.contains("save_restore") || l.contains("PV's connected"))
        .map(str::to_string)
        .collect()
}

/// Start a manual set of `pvs` on a database holding only `TEMP`, and stop
/// after the first pass.
async fn first_pass(pvs: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let cfg = SaveSetConfig {
        name: "auto_settings".into(),
        save_path: dir.path().join("auto_settings.sav"),
        request_file: None,
        request_pvs: pvs.iter().map(|p| p.to_string()).collect(),
        backup: BackupConfig::default(),
        macros: HashMap::new(),
        search_paths: Vec::new(),
    };
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(cfg, SaveStrategy::Manual)
            .build()
            .await,
    );
    let reactor = epics_base_rs::runtime::task::Reactor::current().unwrap();
    // Detached; `shutdown` ends the task.
    drop(mgr.clone().start(&reactor, db.clone()));
    let set = mgr.sets()[0].0.clone();
    for _ in 0..100 {
        if set.list_status().text == "Enabling list..." {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    mgr.shutdown();
}

#[epics_macros_rs::epics_test]
async fn child_some_members_missing() {
    if std::env::var_os(CHILD).is_some() {
        first_pass(&["TEMP", "GONE1", "GONE2"]).await;
    }
}

#[epics_macros_rs::epics_test]
async fn child_every_member_present() {
    if std::env::var_os(CHILD).is_some() {
        first_pass(&["TEMP"]).await;
    }
}

#[test]
fn a_list_with_missing_members_names_each_one() {
    assert_eq!(
        console_of("child_some_members_missing"),
        [
            "save_restore:connect_list: not all searches successful",
            "save_restore: connect failed for channel 'GONE1'",
            "save_restore: connect failed for channel 'GONE2'",
            "auto_settings.sav: 1 of 3 PV's connected",
        ]
    );
}

#[test]
fn a_list_that_connects_says_only_how_many() {
    assert_eq!(
        console_of("child_every_member_present"),
        ["auto_settings.sav: 1 of 1 PV's connected"]
    );
}
