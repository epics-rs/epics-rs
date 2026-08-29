//! A completed save cycle always leaves a valid `.savB` beside the
//! `.sav`, and it holds the generation that was just written.
//!
//! C decides this on the `.savB` itself — "Ensure that backup is ok
//! before we overwrite .sav file." — writing a fresh one from the values
//! about to be saved whenever the existing one is missing or does not end
//! in `<END>`, and copying `.sav` over it afterwards otherwise
//! (`save_restore.c` `write_save_file` at `R6-0-20-g186f467`). This port
//! used to decide it on the `.sav` instead, which skipped every backup
//! step in exactly the cycles that needed one: a missing or corrupt
//! `.sav` left the corrupt `.savB` in place, and the backup otherwise
//! trailed the `.sav` by a whole generation.
//!
//! One case per boundary of that rule rather than one per story: `.savB`
//! absent / bad / good, crossed with a `.sav` that is absent, bad or
//! good, plus the two ways the rule can be switched off or fail.

use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::save_file::{read_save_file, validate_save_file};
use epics_base_rs::server::autosave::save_set::{SaveSet, SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

async fn one_pv_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("PV1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db
}

async fn one_pv_set(sav: &std::path::Path, backup: BackupConfig) -> SaveSet {
    SaveSet::new(SaveSetConfig {
        name: "s".into(),
        save_path: sav.to_path_buf(),
        strategy: SaveStrategy::Periodic {
            interval: Duration::from_secs(60),
        },
        request_file: None,
        request_pvs: vec!["PV1".into()],
        backup,
        macros: Default::default(),
        search_paths: Vec::new(),
    })
    .await
    .unwrap()
}

fn savb_only() -> BackupConfig {
    BackupConfig {
        enable_savb: true,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
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

/// `.savB` absent, `.sav` absent: the first save of a fresh IOC. The
/// rotation has nothing to copy, so the backup can only come from the
/// values being saved — which is what C does.
#[epics_macros_rs::epics_test]
async fn the_first_ever_save_leaves_two_usable_files() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");
    let savb = sav.with_extension("savB");

    let db = one_pv_db().await;
    let set = one_pv_set(&sav, savb_only()).await;
    set.save_once(&db).await.unwrap();

    assert!(validate_save_file(&sav).await.unwrap_or(false));
    assert!(
        validate_save_file(&savb).await.unwrap_or(false),
        "a crash during the second-ever write must still have a backup"
    );
    assert_eq!(first_value(&savb).await, first_value(&sav).await);
}

/// `.savB` good, `.sav` good: the steady state. The backup follows the
/// generation just written, it does not trail it.
#[epics_macros_rs::epics_test]
async fn the_backup_holds_the_generation_just_written() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");
    let savb = sav.with_extension("savB");

    let db = one_pv_db().await;
    let set = one_pv_set(&sav, savb_only()).await;
    set.save_once(&db).await.unwrap();

    db.put_pv("PV1", EpicsValue::Double(2.0)).await.unwrap();
    set.save_once(&db).await.unwrap();

    let saved = first_value(&sav).await;
    assert!(saved.starts_with('2'), "the .sav must hold the new value");
    assert_eq!(
        first_value(&savb).await,
        saved,
        "the backup must not be a generation behind at rest"
    );
}

/// `.savB` bad, `.sav` bad: the cycle that used to skip every backup
/// step. Both files must be usable when it ends.
#[epics_macros_rs::epics_test]
async fn a_corrupt_backup_is_replaced_even_when_the_sav_is_corrupt_too() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");
    let savb = sav.with_extension("savB");
    epics_base_rs::runtime::fs::write(&sav, "PV1 9.9\n")
        .await
        .unwrap();
    epics_base_rs::runtime::fs::write(&savb, "PV1 9.9\n")
        .await
        .unwrap();

    let db = one_pv_db().await;
    let set = one_pv_set(&sav, savb_only()).await;
    set.save_once(&db).await.unwrap();

    assert!(validate_save_file(&sav).await.unwrap_or(false));
    assert!(
        validate_save_file(&savb).await.unwrap_or(false),
        "the corrupt backup must not survive the cycle"
    );
}

/// A backup that cannot be written aborts the cycle before the `.sav` is
/// overwritten, so a set never trades its last good generation for a
/// backup it failed to make.
#[cfg(unix)]
#[epics_macros_rs::epics_test]
async fn a_backup_that_cannot_be_written_leaves_the_sav_alone() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");
    epics_base_rs::runtime::fs::write(&sav, "# autosave-rs V1.0\nPV1 9.9\n<END>\n")
        .await
        .unwrap();
    // A directory at the backup's name: the rename onto it always fails.
    std::fs::create_dir(sav.with_extension("savB")).unwrap();

    let db = one_pv_db().await;
    let set = one_pv_set(&sav, savb_only()).await;
    assert!(set.save_once(&db).await.is_err());
    assert_eq!(
        first_value(&sav).await,
        "9.9",
        "the generation that could not be backed up must not be replaced"
    );
}

/// Switched off, the rule writes nothing: `.savB` is a policy, not a
/// file the port assumes is there.
#[epics_macros_rs::epics_test]
async fn a_set_without_savb_backups_writes_none() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");

    let db = one_pv_db().await;
    let set = one_pv_set(
        &sav,
        BackupConfig {
            enable_savb: false,
            ..savb_only()
        },
    )
    .await;
    set.save_once(&db).await.unwrap();
    set.save_once(&db).await.unwrap();

    assert!(!sav.with_extension("savB").exists());
}
