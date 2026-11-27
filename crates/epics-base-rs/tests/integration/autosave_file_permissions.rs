//! C `write_it` opens the save file in place with `file_permissions` and,
//! after `save_restoreSet_FilePermissions`, `fchmod`s it to them
//! (save_restore.c:1771-1786); `myFileCopy`'s `fopen(dest, "wb")` keeps the
//! mode of a backup already there (dbrestore.c:189).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::save_set::{SaveSet, SaveSetConfig};
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

async fn saved_twice(sav: &Path, settings: SrSettings, before: impl Fn()) {
    let db = PvDatabase::new();
    db.add_record("PV1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    let set = SaveSet::new_with_settings(
        SaveSetConfig {
            name: "s".into(),
            save_path: sav.to_path_buf(),
            request_file: None,
            request_pvs: vec!["PV1".into()],
            backup: BackupConfig { enable_savb: true },
            macros: Default::default(),
            macro_string: None,
            search_paths: Vec::new(),
        },
        Arc::new(settings),
    )
    .await
    .unwrap();
    set.save_once(&db).await.unwrap();
    before();
    set.save_once(&db).await.unwrap();
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// 0666, so a umask that strips a write bit from the open's mode does not
/// survive the `fchmod`.
#[epics_macros_rs::epics_test]
async fn set_permissions_are_forced_on_the_save_file() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("s.sav");
    let settings = SrSettings::default();
    settings.set_file_permissions(0o666);
    saved_twice(&sav, settings, || chmod(&sav, 0o644)).await;
    assert_eq!(mode(&sav), 0o666);
}

/// Without the setter C's `open` leaves an existing file's mode alone.
#[epics_macros_rs::epics_test]
async fn an_existing_files_mode_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("s.sav");
    let savb = dir.path().join("s.savB");
    saved_twice(&sav, SrSettings::default(), || {
        chmod(&sav, 0o600);
        chmod(&savb, 0o640);
    })
    .await;
    assert_eq!(mode(&sav), 0o600);
    assert_eq!(mode(&savb), 0o640);
}
