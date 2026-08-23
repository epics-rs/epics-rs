//! `asVerify` must account for everything the save file contained.
//!
//! A PV that was unreachable when the file was written is recorded with
//! `connected: false` by the save side and surfaced by the restore side
//! on `RestoreResult::disconnected_skipped`. Verify was the only
//! consumer that dropped it: the entry reached neither a per-PV line nor
//! any counter, so the summary counts summed to less than the file's
//! entry count with nothing saying so. A 24-entry file reported
//! `12 match, 0 mismatch, 0 not found, 0 parse errors` and read as a
//! fully verified save set while half of it had never been compared.
//!
//! Lines that declared no entry at all were dropped by the same
//! omission — `read_save_file` returns them and verify discarded them.
//!
//! The invariant, and what these cases pin, is the sum: every entry the
//! file declares lands in exactly one bucket, and the malformed lines
//! are listed under them.
//!
//! Correctness properties of this implementation; no synApps autosave
//! source exists on this machine, so nothing here asserts C parity.

use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::verify::{MatchResult, format_verify_report, verify};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

fn entry(name: &str, value: &str, connected: bool) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: value.into(),
        connected,
    }
}

async fn db_with(names: &[&str]) -> PvDatabase {
    let db = PvDatabase::new();
    for name in names {
        db.add_record(name, Box::new(AoRecord::new(10.0)))
            .await
            .unwrap();
    }
    db
}

/// Half the set was unreachable at save time: the report must still have
/// one bucket per entry, and the counts must sum to the entry count.
#[epics_macros_rs::epics_test]
async fn every_entry_lands_in_exactly_one_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("half.sav");
    let db = db_with(&["PV1", "PV2"]).await;

    write_save_file(
        &path,
        &[
            entry("PV1", "10", true),
            entry("PV2", "10", true),
            entry("PV3", "", false),
            entry("PV4", "", false),
        ],
    )
    .await
    .unwrap();

    let report = verify(&db, &path).await.unwrap();
    assert_eq!(
        report.entries.len(),
        4,
        "one verify entry per entry in the file"
    );
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|e| matches!(e.result, MatchResult::DisconnectedAtSave))
            .count(),
        2
    );

    let text = format_verify_report(&report);
    assert!(text.contains("NOT_CHECKED: PV3 (disconnected at save)"));
    assert!(text.contains("NOT_CHECKED: PV4 (disconnected at save)"));
    assert!(
        text.contains("2 match")
            && text.contains("0 mismatch")
            && text.contains("0 not found")
            && text.contains("0 parse errors")
            && text.contains("2 not checked (disconnected at save)"),
        "the counts must sum to the file's 4 entries:\n{text}"
    );
}

/// The all-disconnected boundary: nothing was compared, and the report
/// must not read as a clean verify.
#[epics_macros_rs::epics_test]
async fn a_file_that_was_never_compared_does_not_read_as_verified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("none.sav");
    let db = db_with(&[]).await;

    write_save_file(&path, &[entry("PV1", "", false), entry("PV2", "", false)])
        .await
        .unwrap();

    let report = verify(&db, &path).await.unwrap();
    assert_eq!(report.entries.len(), 2);
    let text = format_verify_report(&report);
    assert!(
        text.contains("2 not checked (disconnected at save)"),
        "{text}"
    );
}

/// The other half of "accounts for the whole file": a line that declared
/// no entry is reported rather than discarded, the way
/// `RestoreResult::malformed_lines` reports it on the restore side.
#[epics_macros_rs::epics_test]
async fn a_line_that_declared_no_entry_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.sav");
    let db = db_with(&["PV1"]).await;

    epics_base_rs::runtime::fs::write(
        &path,
        "# autosave-rs V1.0\nPV1 10\nPV2 @array@ { \"a\"\n<END>\n",
    )
    .await
    .unwrap();

    let report = verify(&db, &path).await.unwrap();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.malformed.len(), 1, "the unclosed array line");
    let text = format_verify_report(&report);
    assert!(text.contains("MALFORMED: line 3"), "{text}");
    assert!(text.contains("1 malformed lines"), "{text}");
}
