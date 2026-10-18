//! C `doPeriodicDatedBackup` (save_restore.c:2218-2262) is the save task's:
//! once the dated-backup period has passed since the task started
//! (:800, :1016-1022), a pass writes `<save_file>_b_<yymmdd-hhmmss>` from
//! the list's PVs, whether or not the list has saved.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use epics_base_rs::runtime::task::{Reactor, sleep};
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy, SrStatus};
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

fn config(name: &str, save_path: &Path) -> SaveSetConfig {
    SaveSetConfig {
        name: name.into(),
        save_path: save_path.to_path_buf(),
        request_file: None,
        request_pvs: vec!["TEMP".into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: String::new(),
        search_paths: Vec::new(),
    }
}

/// `save_restoreDatedBackupPeriod` at 1 s, which the command, in minutes,
/// cannot set.
fn dated_every_second() -> Arc<SrSettings> {
    let settings = Arc::new(SrSettings::default());
    settings.periodic_dated_backups.store(1, Ordering::Relaxed);
    settings.dated_backup_period.store(1, Ordering::Relaxed);
    settings
}

async fn db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(7.0)))
        .await
        .unwrap();
    db
}

fn dated(dir: &Path, stem: &str) -> Vec<std::path::PathBuf> {
    let prefix = format!("{stem}.sav_b_");
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix(&prefix))
                .is_some_and(|ts| ts.len() == 13 && ts.as_bytes()[6] == b'-')
        })
        .collect();
    found.sort();
    found
}

/// Both lists get theirs: C moves the one timer inside the per-list loop
/// (:1020), so only its first list ever did.
#[epics_macros_rs::epics_test]
async fn every_list_gets_a_dated_backup_from_its_pvs() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .settings(dated_every_second())
            .add_set(config("a", &dir.path().join("a.sav")), SaveStrategy::Manual)
            .add_set(config("b", &dir.path().join("b.sav")), SaveStrategy::Manual)
            .build()
            .await,
    );
    let task = mgr.clone().start(&Reactor::current().unwrap(), db.clone());
    sleep(Duration::from_millis(500)).await;
    assert!(dated(dir.path(), "a").is_empty(), "inside the period");
    for _ in 0..40 {
        if !dated(dir.path(), "a").is_empty() && !dated(dir.path(), "b").is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    mgr.shutdown();
    drop(task);
    for stem in ["a", "b"] {
        let files = dated(dir.path(), stem);
        assert!(
            !files.is_empty(),
            "{stem}: {:?}",
            std::fs::read_dir(dir.path())
        );
        let contents = read_save_file(&files[0]).await.unwrap().unwrap();
        assert_eq!(contents.entries[0].pv_name, "TEMP");
        assert_eq!(contents.entries[0].value.parse::<f64>().unwrap(), 7.0);
    }
    assert!(!dir.path().join("a.sav").exists(), "no save ran");
}

/// :2253-2258.
#[epics_macros_rs::epics_test]
async fn a_dated_backup_that_cannot_be_written_fails_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .settings(dated_every_second())
            .add_set(
                config("a", &dir.path().join("none").join("a.sav")),
                SaveStrategy::Manual,
            )
            .build()
            .await,
    );
    let task = mgr.clone().start(&Reactor::current().unwrap(), db.clone());
    let set = mgr.sets()[0].0.clone();
    for _ in 0..40 {
        if set.list_status().code == SrStatus::Fail {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    mgr.shutdown();
    drop(task);
    let status = set.list_status();
    assert_eq!(
        (status.code, status.text.as_str()),
        (SrStatus::Fail, "Can't write dated backup file")
    );
}
