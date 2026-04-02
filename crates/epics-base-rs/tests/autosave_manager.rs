use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::EpicsValue;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_file::{SaveEntry, read_save_file, write_save_file};
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};

fn quick_backup() -> BackupConfig {
    BackupConfig {
        enable_savb: false,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    }
}

async fn setup_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5))).await;
    db.add_record("PRESS", Box::new(AoRecord::new(1013.0)))
        .await;
    db.add_record("MSG", Box::new(StringinRecord::new("hello")))
        .await;
    db
}

#[tokio::test]
async fn test_periodic_save_set() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("periodic.sav");
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "test_periodic".into(),
            save_path: sav_path.clone(),
            strategy: SaveStrategy::Periodic {
                interval: Duration::from_millis(50),
            },
            request_file: None,
            request_pvs: vec!["TEMP".into(), "PRESS".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let mgr = Arc::new(mgr);
    let handle = mgr.clone().start(db.clone());

    // Wait for at least one save cycle
    tokio::time::sleep(Duration::from_millis(150)).await;
    mgr.shutdown();
    let _ = handle.await;

    // Verify file was created
    assert!(sav_path.exists());
    let entries = read_save_file(&sav_path).await.unwrap().unwrap();
    assert_eq!(entries.len(), 2);
}

#[tokio::test]
async fn test_manual_save() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("manual.sav");
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "test_manual".into(),
            save_path: sav_path.clone(),
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let count = mgr.manual_save("test_manual", &db).await.unwrap();
    assert_eq!(count, 1);
    assert!(sav_path.exists());
}

#[tokio::test]
async fn test_multiple_sets_independent() {
    let dir = tempfile::tempdir().unwrap();
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "set_a".into(),
            save_path: dir.path().join("a.sav"),
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .add_set(SaveSetConfig {
            name: "set_b".into(),
            save_path: dir.path().join("b.sav"),
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["PRESS".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    mgr.manual_save("set_a", &db).await.unwrap();
    mgr.manual_save("set_b", &db).await.unwrap();

    assert!(dir.path().join("a.sav").exists());
    assert!(dir.path().join("b.sav").exists());
}

#[tokio::test]
async fn test_restore_all() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("restore_all.sav");
    let db = setup_db().await;

    // Write a save file to restore from
    write_save_file(
        &sav_path,
        &[SaveEntry {
            pv_name: "TEMP".into(),
            value: "99.9".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "restore_test".into(),
            save_path: sav_path,
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let results = mgr.restore_all(&db).await;
    assert_eq!(results.len(), 1);
    let (name, result) = &results[0];
    assert_eq!(name, "restore_test");
    let result = result.as_ref().unwrap();
    assert_eq!(result.restored, 1);

    // Verify value was restored
    match db.get_pv("TEMP").await.unwrap() {
        EpicsValue::Double(v) => assert!((v - 99.9).abs() < 1e-10),
        other => panic!("expected Double, got {:?}", other),
    }
}

#[tokio::test]
async fn test_one_set_failure_no_impact() {
    let dir = tempfile::tempdir().unwrap();
    let db = setup_db().await;

    // set_bad has nonexistent save file -> restore fails
    // set_good has valid file -> restore succeeds
    let good_path = dir.path().join("good.sav");
    write_save_file(
        &good_path,
        &[SaveEntry {
            pv_name: "TEMP".into(),
            value: "77.0".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "set_bad".into(),
            save_path: dir.path().join("nonexistent.sav"),
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["PRESS".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .add_set(SaveSetConfig {
            name: "set_good".into(),
            save_path: good_path,
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let results = mgr.restore_all(&db).await;
    assert!(results[0].1.is_err()); // set_bad fails
    assert!(results[1].1.is_ok()); // set_good succeeds
    assert_eq!(results[1].1.as_ref().unwrap().restored, 1);
}

#[tokio::test]
async fn test_concurrent_manual_periodic_serialized() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("concurrent.sav");
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "concurrent".into(),
            save_path: sav_path.clone(),
            strategy: SaveStrategy::Periodic {
                interval: Duration::from_millis(30),
            },
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let mgr = Arc::new(mgr);
    let handle = mgr.clone().start(db.clone());

    // Manual saves while periodic is running
    for _ in 0..5 {
        let _ = mgr.manual_save("concurrent", &db).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    mgr.shutdown();
    let _ = handle.await;

    // Just verify no crash/corruption
    assert!(sav_path.exists());
    let entries = read_save_file(&sav_path).await.unwrap().unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn test_shutdown_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "shutdown_test".into(),
            save_path: dir.path().join("shutdown.sav"),
            strategy: SaveStrategy::Periodic {
                interval: Duration::from_millis(10),
            },
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let mgr = Arc::new(mgr);
    let handle = mgr.clone().start(db.clone());

    tokio::time::sleep(Duration::from_millis(50)).await;
    mgr.shutdown();

    // Should complete within a reasonable time
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("shutdown timed out")
        .unwrap();
}

#[tokio::test]
async fn test_save_once_failure_updates_stats() {
    let dir = tempfile::tempdir().unwrap();
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "stats_test".into(),
            // Path to a directory that doesn't exist -> write fails
            save_path: dir.path().join("nonexistent_dir/test.sav"),
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec!["TEMP".into()],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let result = mgr.manual_save("stats_test", &db).await;
    assert!(result.is_err());

    let (set, _) = &mgr.sets()[0];
    assert_eq!(set.stats().error_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn test_empty_pv_list_noop() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("empty.sav");
    let db = setup_db().await;

    let mgr = AutosaveBuilder::new()
        .add_set(SaveSetConfig {
            name: "empty".into(),
            save_path: sav_path.clone(),
            strategy: SaveStrategy::Manual,
            request_file: None,
            request_pvs: vec![],
            backup: quick_backup(),
            macros: HashMap::new(),
            search_paths: Vec::new(),
        })
        .build()
        .await
        .unwrap();

    let count = mgr.manual_save("empty", &db).await.unwrap();
    assert_eq!(count, 0);
    assert!(sav_path.exists()); // File created (even if empty)
}
