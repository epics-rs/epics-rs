//! C runs one save_restore task per IOC (save_restore.c:782). A runner that
//! serves CA and PVA hands the manager to both servers, and each starts it:
//! the list must still be served by one task, or a second pass saves it
//! again a moment after the first, rotating `.sav0` and clearing C's
//! ".savB file was bad".

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use epics_base_rs::runtime::task::{Reactor, sleep};
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy, SrStatus};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

#[epics_macros_rs::epics_test]
async fn a_manager_started_twice_saves_its_list_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(
                SaveSetConfig {
                    name: "s".into(),
                    save_path: dir.path().join("s.sav"),
                    request_file: None,
                    request_pvs: vec!["TEMP".into()],
                    backup: BackupConfig::default(),
                    macros: HashMap::new(),
                    macro_string: String::new(),
                    search_paths: Vec::new(),
                },
                SaveStrategy::OnChange {
                    min_interval: Duration::from_millis(500),
                },
            )
            .build()
            .await,
    );
    let reactor = Reactor::current().unwrap();
    let first = mgr.clone().start(&reactor, db.clone());
    let second = mgr.clone().start(&reactor, db.clone());
    let set = mgr.sets()[0].0.clone();
    for _ in 0..150 {
        if set.stats().save_count.load(Ordering::Relaxed) > 0 {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    // Inside the next window: the member events of enable are spent.
    sleep(Duration::from_millis(300)).await;
    assert_eq!(set.stats().save_count.load(Ordering::Relaxed), 1);
    let status = set.list_status();
    assert_eq!(
        (status.code, status.text.as_str()),
        (SrStatus::Warn, ".savB file was bad")
    );
    assert!(!dir.path().join("s.sav0").exists());
    mgr.shutdown();
    drop((first, second));
}
