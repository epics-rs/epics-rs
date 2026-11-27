//! A sequence slot index is not an age.
//!
//! `SaveSet::do_seq` fills `.sav0`..`.savN` round-robin, so once
//! the index has wrapped the slot number carries no ordering at all.
//! The boot restore read them in index order, so after a crash that
//! left both `.sav` and `.savB` without an `<END>` marker the IOC restored
//! whichever slot happened to be numbered lowest — up to `num_seq_files -
//! 1` rotation periods stale — and reported it as the source with no
//! warning. Every setpoint changed inside that window was silently
//! reverted.
//!
//! Cases are the boundaries of "newest wins": the newest slot high, the
//! newest slot low, and the newest slot unusable. Mtimes are stamped
//! rather than slept for, so the ordering under test is exact.
//!
//! C `fopen_and_check` (dbrestore.c:1258-1305) agrees on the first two;
//! on the third it tries the slot after the newest, the oldest, which is
//! the C defect `backup::fopen_and_check` names.

use std::path::Path;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::autosave::backup::fopen_and_check;
use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::settings::SrSettings;

/// Write a valid slot and stamp it `secs_ago` seconds old.
async fn slot(sav: &Path, index: usize, secs_ago: u64) {
    let path = sav.with_extension(format!("sav{index}"));
    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "PV1".into(),
            value: format!("{index}"),
            connected: true,
            array_fprintfs: Vec::new(),
        }],
    )
    .await
    .unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn the_newest_slot_wins_when_the_index_has_wrapped() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("test.sav");

    // Round-robin has wrapped: slot 1 holds the newest generation.
    slot(&sav, 0, 120).await;
    slot(&sav, 1, 1).await;
    slot(&sav, 2, 60).await;

    assert_eq!(
        fopen_and_check(&sav, &undated()).await.unwrap().0,
        sav.with_extension("sav1")
    );
}

#[epics_macros_rs::epics_test]
async fn the_newest_slot_wins_when_it_is_the_lowest_index() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("test.sav");

    slot(&sav, 0, 1).await;
    slot(&sav, 1, 60).await;
    slot(&sav, 2, 120).await;

    assert_eq!(
        fopen_and_check(&sav, &undated()).await.unwrap().0,
        sav.with_extension("sav0")
    );
}

#[epics_macros_rs::epics_test]
async fn an_unusable_newest_slot_falls_back_to_the_newest_valid_one() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("test.sav");

    slot(&sav, 0, 120).await;
    slot(&sav, 1, 60).await;
    // Slot 2 is the newest on disk but was truncated mid-write.
    let torn = sav.with_extension("sav2");
    epics_base_rs::runtime::fs::write(
        &torn,
        "# Autosave release 6.1, compiled Sun Oct 18 23:48:36 2026\nPV1 2\n",
    )
    .await
    .unwrap();
    let file = std::fs::File::options().write(true).open(&torn).unwrap();
    file.set_modified(SystemTime::now()).unwrap();

    assert_eq!(
        fopen_and_check(&sav, &undated()).await.unwrap().0,
        sav.with_extension("sav1")
    );
}

/// The default settings with `save_restoreDatedBackupFiles` cleared.
fn undated() -> SrSettings {
    let settings = SrSettings::default();
    settings.set_dated_backup_files(0);
    settings
}
