// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::path::Path;

use crate::server::database::PvDatabase;

use super::error::{AutosaveError, AutosaveResult};
use super::save_file::{self, read_save_file};

/// Result of comparing one PV.
#[derive(Debug, Clone)]
pub enum MatchResult {
    Match,
    Mismatch { saved: String, live: String },
    PvNotFound,
    ParseError,
}

/// A single verify entry.
#[derive(Debug, Clone)]
pub struct VerifyEntry {
    pub pv_name: String,
    pub saved_value: String,
    pub live_value: Option<String>,
    pub result: MatchResult,
}

/// Compare saved values against live PV values.
///
/// A corrupt/truncated save file (no `<END>` marker) is surfaced as
/// [`AutosaveError::CorruptSaveFile`] — NOT collapsed into an empty
/// entry list. Collapsing it would make `format_verify_report` print
/// `0 match, 0 mismatch` and tell an operator everything is fine,
/// hiding exactly the corruption `asVerify` exists to catch.
/// Mirrors `restore_from_entries`'s handling of the same `None`.
pub async fn verify(db: &PvDatabase, save_file_path: &Path) -> AutosaveResult<Vec<VerifyEntry>> {
    let entries =
        read_save_file(save_file_path)
            .await?
            .ok_or_else(|| AutosaveError::CorruptSaveFile {
                path: save_file_path.display().to_string(),
                message: "missing <END> marker (truncated or corrupt save file)".to_string(),
            })?;

    let mut results = Vec::new();

    for entry in &entries {
        if !entry.connected {
            continue;
        }

        let live = match db.get_pv(&entry.pv_name) {
            Ok(val) => val,
            Err(_) => {
                results.push(VerifyEntry {
                    pv_name: entry.pv_name.clone(),
                    saved_value: entry.value.clone(),
                    live_value: None,
                    result: MatchResult::PvNotFound,
                });
                continue;
            }
        };

        let live_str = save_file::value_to_save_str(&live);

        // Try parsing saved value using live type as template
        let parsed = save_file::parse_save_value(&entry.value, &live);
        if parsed.is_none() {
            results.push(VerifyEntry {
                pv_name: entry.pv_name.clone(),
                saved_value: entry.value.clone(),
                live_value: Some(live_str),
                result: MatchResult::ParseError,
            });
            continue;
        }

        let parsed = parsed.unwrap();
        let result = if parsed == live {
            MatchResult::Match
        } else {
            MatchResult::Mismatch {
                saved: entry.value.clone(),
                live: live_str.clone(),
            }
        };

        results.push(VerifyEntry {
            pv_name: entry.pv_name.clone(),
            saved_value: entry.value.clone(),
            live_value: Some(live_str),
            result,
        });
    }

    Ok(results)
}

/// Format a human-readable verify report.
pub fn format_verify_report(entries: &[VerifyEntry]) -> String {
    let mut report = String::new();
    let mut match_count = 0;
    let mut mismatch_count = 0;
    let mut not_found_count = 0;
    let mut parse_error_count = 0;

    for entry in entries {
        match &entry.result {
            MatchResult::Match => {
                match_count += 1;
            }
            MatchResult::Mismatch { saved, live } => {
                mismatch_count += 1;
                report.push_str(&format!(
                    "MISMATCH: {} saved={} live={}\n",
                    entry.pv_name, saved, live
                ));
            }
            MatchResult::PvNotFound => {
                not_found_count += 1;
                report.push_str(&format!("NOT_FOUND: {}\n", entry.pv_name));
            }
            MatchResult::ParseError => {
                parse_error_count += 1;
                report.push_str(&format!(
                    "PARSE_ERROR: {} saved={}\n",
                    entry.pv_name, entry.saved_value
                ));
            }
        }
    }

    report.push_str(&format!(
        "\nSummary: {} match, {} mismatch, {} not found, {} parse errors\n",
        match_count, mismatch_count, not_found_count, parse_error_count
    ));

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::autosave::error::AutosaveError;
    use crate::server::database::PvDatabase;
    use crate::server::records::ao::AoRecord;

    /// H4 regression: `verify` against a corrupt save file (no
    /// `<END>` marker) must surface an error, NOT collapse the file
    /// into zero entries and report "all match". Pre-fix,
    /// `read_save_file(...).unwrap_or_default()` turned a truncated
    /// file into an empty `Vec` and the report claimed everything
    /// was fine — hiding exactly the corruption `asVerify` exists to
    /// catch.
    #[tokio::test]
    async fn verify_on_corrupt_save_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.sav");
        // A file with content but NO `<END>` marker — truncated save.
        crate::runtime::fs::write(&path, "# autosave-rs V1.0\nPV1 1.0\nPV2 2.0\n")
            .await
            .unwrap();

        let db = PvDatabase::new();
        db.add_record("PV1", Box::new(AoRecord::new(1.0)))
            .await
            .unwrap();

        let result = verify(&db, &path).await;
        match result {
            Err(AutosaveError::CorruptSaveFile { .. }) => {}
            other => panic!("expected CorruptSaveFile error, got {other:?}"),
        }
    }

    /// H4: a well-formed save file (with `<END>`) still verifies
    /// normally — the corruption guard does not break the happy path.
    #[tokio::test]
    async fn verify_on_valid_save_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.sav");
        crate::runtime::fs::write(&path, "# autosave-rs V1.0\nPV1 1.0\n<END>\n")
            .await
            .unwrap();

        let db = PvDatabase::new();
        db.add_record("PV1", Box::new(AoRecord::new(1.0)))
            .await
            .unwrap();

        let entries = verify(&db, &path).await.expect("valid file must verify");
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].result, MatchResult::Match));
    }
}
