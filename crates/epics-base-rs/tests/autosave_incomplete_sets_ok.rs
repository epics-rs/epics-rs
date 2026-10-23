//! C `save_restoreIncompleteSetsOk`, cleared: the save task does not write
//! a set it could not fully fetch (save_restore.c:1004, :1221), and a
//! restore stops at the file's `!` line (dbrestore.c:1003-1012,
//! save_restore.c:3430-3442).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{
    ManualSource, SaveSetConfig, SaveStrategy, SrStatus, do_manual_restore,
};
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::autosave::startup::RestoreDef;
use epics_base_rs::server::autosave::{BootRestore, RestorePass, reboot_restore};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const INCOMPLETE: &str =
    "# h\nB 2\n! 1 channel(s) not connected - or not all gets were successful\nA 3\n<END>\n";

fn settings(ok: i32) -> Arc<SrSettings> {
    let s = SrSettings::default();
    s.set_incomplete_sets_ok(ok);
    Arc::new(s)
}

async fn db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    for name in ["A", "B", "TRIG"] {
        db.add_record(name, Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
    }
    db
}

fn value(db: &PvDatabase, name: &str) -> f64 {
    match db.get_pv(name).unwrap() {
        EpicsValue::Double(v) => v,
        v => panic!("{v:?}"),
    }
}

/// `A` and a member that is not there.
fn config(dir: &std::path::Path) -> SaveSetConfig {
    SaveSetConfig {
        name: "s".into(),
        save_path: dir.join("s.sav"),
        request_file: None,
        request_pvs: vec!["A".into(), "NOPE".into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    }
}

/// Whether a triggered save of the incomplete set wrote its file.
async fn saved_by_the_task(ok: i32) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .settings(settings(ok))
            .add_set(
                config(dir.path()),
                SaveStrategy::Triggered {
                    trigger_pv: "TRIG".into(),
                },
            )
            .build()
            .await,
    );
    drop(mgr.clone().start(db.clone()));
    let path = dir.path().join("s.sav");
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    mgr.shutdown();
    path.exists()
}

#[epics_macros_rs::epics_test]
async fn the_task_writes_an_incomplete_set_only_when_ok() {
    assert!(saved_by_the_task(1).await);
    assert!(!saved_by_the_task(0).await);
}

/// `manual_save`'s write_save_file is skipped, so C's status stays -1
/// (:1220-1221).
#[epics_macros_rs::epics_test]
async fn a_manual_save_to_a_file_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.req"), "A\nNOPE\n").unwrap();
    let db = db().await;
    let path = dir.path().join("m.sav");
    for (ok, status) in [(0, -1), (1, 1)] {
        let mgr = AutosaveBuilder::new()
            .settings(settings(ok))
            .add_set(
                SaveSetConfig {
                    request_file: Some("s.req".into()),
                    request_pvs: Vec::new(),
                    search_paths: vec![dir.path().to_path_buf()],
                    ..config(dir.path())
                },
                SaveStrategy::Manual,
            )
            .build()
            .await;
        assert_eq!(mgr.save_file_op(&db, "s.req", "m.sav", &path).await, status);
        assert_eq!(path.exists(), ok == 1);
    }
}

async fn boot(ok: i32) -> (Arc<PvDatabase>, BootRestore, bool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), INCOMPLETE).unwrap();
    let db = db().await;
    let item = BootRestore {
        path: dir.path().join("s.sav"),
        status: RestoreDef::new("s.sav".into(), String::new()).status,
        filename: "s.sav".into(),
        macrostring: String::new(),
        write_backup: false,
        settings: settings(ok),
    };
    let restored = reboot_restore(&db, &item, RestorePass::One).await.is_ok();
    (db, item, restored)
}

/// The PV before the `!` line is restored and the one after it is not.
#[epics_macros_rs::epics_test]
async fn a_boot_restore_stops_at_the_error_line() {
    let (db, item, restored) = boot(0).await;
    assert!(!restored);
    assert_eq!((value(&db, "B"), value(&db, "A")), (2.0, 0.0));
    let status = item.status.lock().unwrap().clone();
    assert_eq!(
        (status.code, status.text.as_str()),
        (SrStatus::Fail, "restore aborted")
    );

    let (db, item, restored) = boot(1).await;
    assert!(restored);
    assert_eq!((value(&db, "B"), value(&db, "A")), (2.0, 3.0));
    assert_eq!(item.status.lock().unwrap().code, SrStatus::Warn);
}

/// C returns ERROR before the `.bu` copy (:3436-3442).
#[epics_macros_rs::epics_test]
async fn a_manual_restore_stops_at_the_error_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sav");
    std::fs::write(&path, INCOMPLETE).unwrap();
    let db = db().await;
    let mgr = AutosaveBuilder::new().settings(settings(0)).build().await;
    let status = do_manual_restore(&db, &mgr, "s.sav", &path, ManualSource::SaveFile, "").await;
    assert_eq!(status, -1);
    assert_eq!((value(&db, "B"), value(&db, "A")), (2.0, 0.0));
    assert!(!dir.path().join("s.sav.bu").exists());
}
