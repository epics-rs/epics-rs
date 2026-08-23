//! CA4-2: every line of a `.sav` file must end up somewhere — as an
//! entry, or in a bucket the restore result reports. A line that is
//! neither is a PV silently not restored.
//!
//! Correctness only: no synApps autosave source exists on this machine,
//! so nothing here is a claim about what C's `dbrestore.c` does.

use std::sync::Arc;

use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::restore_from_entries;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::EpicsValue;

/// Write a `.sav` body verbatim, header and `<END>` included.
async fn write_sav(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("t.sav");
    let content = format!("# save/restore V1.7\t2026-08-22 00:00:00\n{body}<END>\n");
    std::fs::write(&path, content).unwrap();
    path
}

async fn setup_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("SR:STR", Box::new(StringinRecord::new("before")))
        .await
        .unwrap();
    db.add_record("SR:NUM", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db
}

/// Boundary: the value is the empty string. The separator is still
/// there, so the line declares an entry whose value is empty.
#[epics_macros_rs::epics_test]
async fn an_empty_value_is_an_entry_not_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sav(dir.path(), "SR:STR \nSR:NUM 42\n").await;

    let contents = read_save_file(&path).await.unwrap().unwrap();

    assert_eq!(contents.entries.len(), 2, "{:?}", contents.entries);
    assert_eq!(contents.entries[0].pv_name, "SR:STR");
    assert_eq!(contents.entries[0].value, "");
    assert_eq!(contents.entries[1].value, "42");
    assert!(contents.malformed.is_empty());
}

/// Boundary: the value is whitespace. It is the value, not padding.
#[epics_macros_rs::epics_test]
async fn a_whitespace_value_survives_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sav(dir.path(), "SR:STR   \n").await;

    let contents = read_save_file(&path).await.unwrap().unwrap();

    assert_eq!(contents.entries.len(), 1, "{:?}", contents.entries);
    assert_eq!(contents.entries[0].value, "  ");
}

/// Boundary: no separator at all. The line names no PV, so it cannot
/// become an entry — it must be reported instead of vanishing.
#[epics_macros_rs::epics_test]
async fn a_line_without_a_separator_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sav(dir.path(), "SR:NUM 42\nTRUNCATED\n").await;

    let contents = read_save_file(&path).await.unwrap().unwrap();

    assert_eq!(contents.entries.len(), 1);
    assert_eq!(contents.malformed.len(), 1, "{:?}", contents.malformed);
    assert_eq!(contents.malformed[0].text, "TRUNCATED");
    assert_eq!(
        contents.malformed[0].line_no, 3,
        "1-based, counting the header"
    );
}

/// Boundary: an `@array@` line whose braces do not close. The marker
/// settles what the line is, so it is a malformed array line — not a
/// scalar whose value happens to start with `@array@`.
#[epics_macros_rs::epics_test]
async fn a_truncated_array_line_is_reported_not_misparsed() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sav(dir.path(), "SR:STR @array@ { \"a\" \"b\"\n").await;

    let contents = read_save_file(&path).await.unwrap().unwrap();

    assert!(
        contents.entries.is_empty(),
        "a broken array line must not become a scalar entry: {:?}",
        contents.entries
    );
    assert_eq!(contents.malformed.len(), 1);
}

/// The reason the buckets exist: a restore over a partially written
/// file must say what it could not use, instead of reporting only the
/// PVs it did write.
#[epics_macros_rs::epics_test]
async fn restore_reports_the_lines_it_could_not_use() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sav(dir.path(), "SR:NUM 42\nTRUNCA\n").await;
    let db = setup_db().await;

    let result = restore_from_entries(&db, &path).await.unwrap();

    assert_eq!(result.restored, 1);
    assert_eq!(
        result.malformed_lines.len(),
        1,
        "the unusable line must be accounted for, not silently skipped"
    );
    assert_eq!(result.malformed_lines[0].text, "TRUNCA");
    assert!(result.parse_failed.is_empty());
    assert!(result.not_found.is_empty());
    assert!(result.failed_puts.is_empty());
    assert!(result.disconnected_skipped.is_empty());
}

/// The empty value reaches the PV, rather than the PV keeping its old
/// value while the restore reports success.
#[epics_macros_rs::epics_test]
async fn an_empty_value_restores_onto_the_pv() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_sav(dir.path(), "SR:STR \n").await;
    let db = setup_db().await;

    let result = restore_from_entries(&db, &path).await.unwrap();

    assert_eq!(result.restored, 1);
    match db.get_pv("SR:STR").unwrap() {
        EpicsValue::String(s) => assert_eq!(s.as_str_lossy(), ""),
        other => panic!("expected String, got {other:?}"),
    }
}
