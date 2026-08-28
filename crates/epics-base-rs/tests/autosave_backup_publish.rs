//! Backup rotation must be all-or-nothing, and a rotation watermark must
//! not move past a copy that did not land.
//!
//! `std::fs::copy` opens its destination CREATE|TRUNCATE before it can
//! fail, so rotating onto an existing backup destroyed that generation
//! the instant the write ran short of space — and `rotate_backups` then
//! advanced `seq_index` anyway, so the slot it had just emptied was not
//! rewritten for a whole `seq_period`. Backup depth silently dropped from
//! three generations to one.
//!
//! The failures below are injected with an obstructed destination and a
//! read-only destination rather than a full filesystem, because a test
//! cannot mount one; what they pin is the mechanism the full-filesystem
//! case relies on — the destination is never opened for writing, and the
//! watermark advances only after the rename. Correctness properties of
//! this implementation rather than C parity: synApps `save_restore.c`
//! copies backups with `myFileCopy` and has neither the staging file nor
//! the watermarks.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::{
    BackupConfig, BackupState, SavbState, publish_savb, rotate_backups,
};
use epics_base_rs::server::autosave::format::CompatMode;
use epics_base_rs::server::autosave::save_file::{
    SaveEntry, read_save_file, validate_save_file, write_save_file,
};

fn entry(name: &str, val: &str) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: val.into(),
        connected: true,
    }
}

fn seq_only() -> BackupConfig {
    BackupConfig {
        enable_savb: false,
        num_seq_files: 3,
        seq_period: Duration::from_millis(1),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    }
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

/// A sequence copy that cannot be published leaves its slot for the next
/// cycle instead of stepping over it.
#[epics_macros_rs::epics_test]
async fn a_failed_seq_publish_leaves_the_slot_for_the_next_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("test.sav");
    write_save_file(&sav, &[entry("PV1", "1.0")]).await.unwrap();

    // Obstruct slot 0 so the rename onto it cannot succeed.
    let slot0 = sav.with_extension("sav0");
    std::fs::create_dir(&slot0).unwrap();

    let config = seq_only();
    let mut state = BackupState::default();
    let failed = rotate_backups(&sav, &config, &mut state, &[], CompatMode::Native).await;
    assert!(
        failed.is_err(),
        "a rotation that could not publish must say so"
    );
    assert!(
        !sav.with_extension("sav0.tmp").exists(),
        "the unpublished temp copy must not be left behind"
    );

    // Clear the obstruction: the next cycle must retry slot 0, not skip
    // to slot 1 as if slot 0 already held this generation.
    std::fs::remove_dir(&slot0).unwrap();
    rotate_backups(&sav, &config, &mut state, &[], CompatMode::Native)
        .await
        .unwrap();
    assert!(
        validate_save_file(&slot0).await.unwrap_or(false),
        "slot 0 must be retried"
    );
    assert!(
        !sav.with_extension("sav1").exists(),
        "the watermark must not have advanced past the failed copy"
    );
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

    let config = BackupConfig {
        enable_savb: true,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    };
    let mut state = BackupState::default();
    let savb_state = rotate_backups(&sav, &config, &mut state, &[], CompatMode::Native)
        .await
        .unwrap();
    assert_eq!(
        savb_state,
        SavbState::Ok,
        "a valid .savB is left alone until the new .sav is on disk"
    );
    publish_savb(&sav, savb_state).await.unwrap();

    assert_eq!(
        first_value(&savb).await,
        "new",
        "the backup generation must be the one just rotated"
    );
}

/// The dated watermark obeys the same rule as the sequence one.
#[epics_macros_rs::epics_test]
async fn a_failed_dated_publish_leaves_its_watermark_alone() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("test.sav");
    write_save_file(&sav, &[entry("PV1", "1.0")]).await.unwrap();

    // The dated name is built from the wall clock, so the destination
    // cannot be obstructed by name. Obstruct the whole directory
    // instead: no new name can be created in it.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    let config = BackupConfig {
        enable_savb: false,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: true,
        dated_interval: Duration::from_secs(3600),
    };
    let mut state = BackupState::default();
    let failed = rotate_backups(&sav, &config, &mut state, &[], CompatMode::Native).await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        failed.is_err(),
        "an unpublished dated copy must be reported"
    );

    // The interval is an hour, so a watermark that moved would suppress
    // the retry for an hour. It must not have moved: the very next
    // rotation, with the directory writable again, must produce the file.
    rotate_backups(&sav, &config, &mut state, &[], CompatMode::Native)
        .await
        .unwrap();
    let dated: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("test.sav_"))
        })
        .collect();
    assert_eq!(dated.len(), 1, "the dated backup must be retried at once");
}

/// A save cycle whose rotation failed must not overwrite the `.sav` it
/// could not preserve, and must land in the same bookkeeping a failed
/// write does. The early `?` on the rotation used to return from
/// `save_once` before any of it, leaving the set reading `Saving`
/// forever with `error_count` at zero.
#[epics_macros_rs::epics_test]
async fn a_failed_rotation_fails_and_records_the_save_cycle() {
    use std::sync::atomic::Ordering;

    use epics_base_rs::server::autosave::save_set::{
        SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy,
    };
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
        strategy: SaveStrategy::Periodic {
            interval: Duration::from_secs(60),
        },
        request_file: None,
        request_pvs: vec!["PV1".into()],
        backup: seq_only(),
        macros: Default::default(),
        search_paths: Vec::new(),
    })
    .await
    .unwrap();

    // First cycle: no prior `.sav`, so nothing to rotate.
    set.save_once(&db).await.unwrap();
    let saved_first = first_value(&sav).await;

    std::fs::create_dir(sav.with_extension("sav0")).unwrap();
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
