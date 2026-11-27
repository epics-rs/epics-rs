//! Backup rotation must be all-or-nothing.
//!
//! `std::fs::copy` opens its destination CREATE|TRUNCATE before it can
//! fail, so rotating onto an existing backup destroyed that generation
//! the instant the write ran short of space.
//!
//! The failures below are injected with an obstructed destination and a
//! read-only destination rather than a full filesystem, because a test
//! cannot mount one; what they pin is the mechanism the full-filesystem
//! case relies on — the destination is never opened for writing.
//! Correctness properties of this implementation rather than C parity:
//! synApps `save_restore.c` copies backups with `myFileCopy` and has no
//! staging file.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use epics_base_rs::server::autosave::backup::{
    BackupConfig, SavbState, publish_savb, rotate_backups,
};
use epics_base_rs::server::autosave::save_file::{SaveEntry, read_save_file, write_save_file};
use epics_base_rs::server::autosave::settings::SrSettings;

fn entry(name: &str, val: &str) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: val.into(),
        connected: true,
        array_fprintfs: Vec::new(),
    }
}

fn savb_only() -> BackupConfig {
    BackupConfig { enable_savb: true }
}

async fn first_value(path: &std::path::Path) -> String {
    read_save_file(path)
        .await
        .expect("readable")
        .expect("has <END>")
        .entries[0]
        .value
        .clone()
}

/// The publish replaces the destination by rename, so a destination the
/// process may not write to is still replaced — and, the same property
/// read the other way, is never truncated on the way there.
///
/// Under a uid that bypasses file permissions this asserts nothing the
/// pre-fix code did not already do; it is a discriminator only for an
/// ordinary user.
#[epics_macros_rs::epics_test]
async fn a_backup_is_published_without_opening_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("test.sav");
    let savb = sav.with_extension("savB");

    write_save_file(&savb, &[entry("PV1", "old")])
        .await
        .unwrap();
    std::fs::set_permissions(&savb, std::fs::Permissions::from_mode(0o444)).unwrap();
    write_save_file(&sav, &[entry("PV1", "new")]).await.unwrap();

    let config = BackupConfig { enable_savb: true };
    let savb_state = rotate_backups(&sav, &config, &[], &SrSettings::default())
        .await
        .unwrap();
    assert_eq!(
        savb_state,
        SavbState::Ok,
        "a valid .savB is left alone until the new .sav is on disk"
    );
    publish_savb(&sav, savb_state, &Default::default())
        .await
        .unwrap();

    assert_eq!(
        first_value(&savb).await,
        "new",
        "the backup generation must be the one just rotated"
    );
}

/// A save cycle whose rotation failed must not overwrite the `.sav` it
/// could not preserve, and must land in the same bookkeeping a failed
/// write does. The early `?` on the rotation used to return from
/// `save_once` before any of it, leaving the set reading `Saving`
/// forever with `error_count` at zero.
#[epics_macros_rs::epics_test]
async fn a_failed_rotation_fails_and_records_the_save_cycle() {
    use std::sync::atomic::Ordering;

    use epics_base_rs::server::autosave::save_set::{SaveSet, SaveSetConfig, SaveSetStatus};
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::records::ao::AoRecord;

    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");

    let db = PvDatabase::new();
    db.add_record("PV1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    let set = SaveSet::new(SaveSetConfig {
        name: "s".into(),
        save_path: sav.clone(),
        request_file: None,
        request_pvs: vec!["PV1".into()],
        backup: savb_only(),
        macros: Default::default(),
        macro_string: None,
        search_paths: Vec::new(),
    })
    .await
    .unwrap();

    set.save_once(&db).await.unwrap();
    let saved_first = first_value(&sav).await;

    // A `.savB` that cannot be rewritten (save_restore.c:2064-2072).
    let savb = sav.with_extension("savB");
    std::fs::remove_file(&savb).unwrap();
    std::fs::create_dir(&savb).unwrap();
    std::fs::write(savb.join("x"), "").unwrap();
    let failed = set.save_once(&db).await;

    assert!(failed.is_err(), "the cycle must fail with its rotation");
    assert!(
        matches!(set.status().await, SaveSetStatus::Error(_)),
        "a failed cycle must not be left reading Saving"
    );
    assert_eq!(set.stats().error_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        first_value(&sav).await,
        saved_first,
        "the generation that could not be backed up must not be replaced"
    );
}
