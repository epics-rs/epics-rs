use std::time::Duration;

use epics_base_rs::server::autosave::backup::{
    BackupConfig, BackupState, find_best_save_file, rotate_backups,
};
use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};

fn make_entry(name: &str, val: &str) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: val.into(),
        connected: true,
    }
}

#[tokio::test]
async fn test_savb_created() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write initial .sav
    write_save_file(&sav_path, &[make_entry("PV1", "1.0")])
        .await
        .unwrap();

    let config = BackupConfig {
        enable_savb: true,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    };
    let mut state = BackupState::default();

    rotate_backups(&sav_path, &config, &mut state)
        .await
        .unwrap();

    let savb_path = sav_path.with_extension("savB");
    assert!(savb_path.exists());
}

#[tokio::test]
async fn test_seq_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    let config = BackupConfig {
        enable_savb: false,
        num_seq_files: 3,
        seq_period: Duration::from_millis(1), // very short for testing
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    };
    let mut state = BackupState::default();

    // Create and rotate 3 times
    for i in 0..3 {
        write_save_file(&sav_path, &[make_entry("PV1", &i.to_string())])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        rotate_backups(&sav_path, &config, &mut state)
            .await
            .unwrap();
    }

    // All 3 seq files should exist
    assert!(sav_path.with_extension("sav0").exists());
    assert!(sav_path.with_extension("sav1").exists());
    assert!(sav_path.with_extension("sav2").exists());
}

#[tokio::test]
async fn test_dated_filename() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    write_save_file(&sav_path, &[make_entry("PV1", "1.0")])
        .await
        .unwrap();

    let config = BackupConfig {
        enable_savb: false,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: true,
        dated_interval: Duration::from_millis(1),
    };
    let mut state = BackupState::default();

    rotate_backups(&sav_path, &config, &mut state)
        .await
        .unwrap();

    // Check that a dated file was created (sav_YYMMDD-HHMMSS)
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("test.sav_"))
        })
        .collect();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn test_corrupt_sav_fallback_to_savb() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write corrupt .sav (no END marker)
    tokio::fs::write(&sav_path, "PV1 1.0\n").await.unwrap();

    // Write valid .savB
    let savb_path = sav_path.with_extension("savB");
    write_save_file(&savb_path, &[make_entry("PV1", "2.0")])
        .await
        .unwrap();

    let config = BackupConfig::default();
    let best = find_best_save_file(&sav_path, &config).await;
    assert_eq!(best.unwrap(), savb_path);
}

#[tokio::test]
async fn test_no_sav_only_savb() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");
    // .sav does not exist

    // Write valid .savB
    let savb_path = sav_path.with_extension("savB");
    write_save_file(&savb_path, &[make_entry("PV1", "2.0")])
        .await
        .unwrap();

    let config = BackupConfig::default();
    let best = find_best_save_file(&sav_path, &config).await;
    assert_eq!(best.unwrap(), savb_path);
}

#[tokio::test]
async fn test_seq_file_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write corrupt .sav and .savB
    tokio::fs::write(&sav_path, "PV1 1.0\n").await.unwrap();
    tokio::fs::write(sav_path.with_extension("savB"), "PV1 1.0\n")
        .await
        .unwrap();

    // Write valid .sav1
    write_save_file(
        &sav_path.with_extension("sav1"),
        &[make_entry("PV1", "3.0")],
    )
    .await
    .unwrap();

    let config = BackupConfig::default();
    let best = find_best_save_file(&sav_path, &config).await;
    assert_eq!(best.unwrap(), sav_path.with_extension("sav1"));
}

#[tokio::test]
async fn test_partial_write_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.sav");

    // Simulate partial write (no END)
    tokio::fs::write(&path, "PV1 1.0\nPV2 2.0\n").await.unwrap();

    let config = BackupConfig::default();
    let best = find_best_save_file(&path, &config).await;
    assert!(best.is_none());
}

#[tokio::test]
async fn test_rotate_preserves_existing() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write initial .sav
    write_save_file(&sav_path, &[make_entry("PV1", "1.0")])
        .await
        .unwrap();

    let config = BackupConfig {
        enable_savb: true,
        num_seq_files: 1,
        seq_period: Duration::from_millis(1),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    };
    let mut state = BackupState::default();

    // Rotate
    rotate_backups(&sav_path, &config, &mut state)
        .await
        .unwrap();

    // Original .sav should still exist
    assert!(sav_path.exists());
    // .savB should exist
    assert!(sav_path.with_extension("savB").exists());
}
