use epics_base_rs::server::autosave::backup::{BackupConfig, fopen_and_check, rotate_backups};
use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::settings::SrSettings;

fn make_entry(name: &str, val: &str) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: val.into(),
        connected: true,
    }
}

#[epics_macros_rs::epics_test]
async fn test_savb_created() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write initial .sav
    write_save_file(&sav_path, &[make_entry("PV1", "1.0")])
        .await
        .unwrap();

    let config = BackupConfig { enable_savb: true };

    rotate_backups(
        &sav_path,
        &config,
        &[make_entry("PV1", "1.0")],
        &SrSettings::default(),
    )
    .await
    .unwrap();

    let savb_path = sav_path.with_extension("savB");
    assert!(savb_path.exists());
}

#[epics_macros_rs::epics_test]
async fn test_corrupt_sav_fallback_to_savb() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write corrupt .sav (no END marker)
    epics_base_rs::runtime::fs::write(&sav_path, "PV1 1.0\n")
        .await
        .unwrap();

    // Write valid .savB
    let savb_path = sav_path.with_extension("savB");
    write_save_file(&savb_path, &[make_entry("PV1", "2.0")])
        .await
        .unwrap();

    let best = fopen_and_check(&sav_path, &undated()).await.map(|(p, _)| p);
    assert_eq!(best.unwrap(), savb_path);
}

#[epics_macros_rs::epics_test]
async fn test_no_sav_only_savb() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");
    // .sav does not exist

    // Write valid .savB
    let savb_path = sav_path.with_extension("savB");
    write_save_file(&savb_path, &[make_entry("PV1", "2.0")])
        .await
        .unwrap();

    let best = fopen_and_check(&sav_path, &undated()).await.map(|(p, _)| p);
    assert_eq!(best.unwrap(), savb_path);
}

#[epics_macros_rs::epics_test]
async fn test_seq_file_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write corrupt .sav and .savB
    epics_base_rs::runtime::fs::write(&sav_path, "PV1 1.0\n")
        .await
        .unwrap();
    epics_base_rs::runtime::fs::write(sav_path.with_extension("savB"), "PV1 1.0\n")
        .await
        .unwrap();

    // Write valid .sav1
    write_save_file(
        &sav_path.with_extension("sav1"),
        &[make_entry("PV1", "3.0")],
    )
    .await
    .unwrap();

    let best = fopen_and_check(&sav_path, &undated()).await.map(|(p, _)| p);
    assert_eq!(best.unwrap(), sav_path.with_extension("sav1"));
}

#[epics_macros_rs::epics_test]
async fn test_partial_write_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.sav");

    // Simulate partial write (no END)
    epics_base_rs::runtime::fs::write(&path, "PV1 1.0\nPV2 2.0\n")
        .await
        .unwrap();

    let best = fopen_and_check(&path, &undated()).await.map(|(p, _)| p);
    assert!(best.is_none());
}

#[epics_macros_rs::epics_test]
async fn test_rotate_preserves_existing() {
    let dir = tempfile::tempdir().unwrap();
    let sav_path = dir.path().join("test.sav");

    // Write initial .sav
    write_save_file(&sav_path, &[make_entry("PV1", "1.0")])
        .await
        .unwrap();

    let config = BackupConfig { enable_savb: true };

    // Rotate
    rotate_backups(
        &sav_path,
        &config,
        &[make_entry("PV1", "1.0")],
        &SrSettings::default(),
    )
    .await
    .unwrap();

    // Original .sav should still exist
    assert!(sav_path.exists());
    // .savB should exist
    assert!(sav_path.with_extension("savB").exists());
}

/// The default settings with `save_restoreDatedBackupFiles` cleared.
fn undated() -> SrSettings {
    let settings = SrSettings::default();
    settings.set_dated_backup_files(0);
    settings
}
