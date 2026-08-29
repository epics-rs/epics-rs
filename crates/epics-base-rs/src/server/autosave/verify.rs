use std::path::Path;

use crate::server::database::PvDatabase;

use super::error::AutosaveResult;
use super::save_file::{self, MalformedLine, SaveEntry, read_partial_save_file};

/// Result of comparing one PV.
#[derive(Debug, Clone)]
pub enum MatchResult {
    Match,
    Mismatch {
        saved: String,
        live: String,
    },
    PvNotFound,
    ParseError,
    /// The PV was unreachable when the file was written, so the file
    /// carries no value to compare. Distinct from every other variant:
    /// nothing was checked, and nothing can be.
    DisconnectedAtSave,
}

/// A single verify entry.
#[derive(Debug, Clone)]
pub struct VerifyEntry {
    pub pv_name: String,
    pub saved_value: String,
    pub live_value: Option<String>,
    pub result: MatchResult,
}

/// Everything a verify pass has to say about one save file.
///
/// The two halves together account for the whole file: `entries` has one
/// element per entry the file declared, in file order, and `malformed`
/// carries the lines that declared no entry at all. A report built from
/// either half alone under-counts, which is what `asVerify` exists to
/// prevent.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub entries: Vec<VerifyEntry>,
    pub malformed: Vec<MalformedLine>,
    /// The file did not end in `<END>`, so what it holds is whatever had
    /// been written when it was truncated. The comparison below still
    /// stands for the entries that are there.
    pub truncated: bool,
}

/// Compare saved values against live PV values.
///
/// A truncated save file (no `<END>` marker) is reported through
/// [`VerifyReport::truncated`] and still compared, which is what C's
/// `do_asVerify_fp` does (`verify.c` at `R6-0-20-g186f467`): it warns
/// that the marker is missing and then verifies every PV line in the
/// file, because the operator looking at a corrupt save wants to know
/// which PVs it holds and whether they match. What must never happen is
/// the third thing — the truncation vanishing into an empty entry list,
/// which had `format_verify_report` print `0 match, 0 mismatch` and tell
/// the operator everything was fine.
///
/// Every entry the file declares produces exactly one [`VerifyEntry`],
/// including the ones written while the PV was unreachable. Skipping
/// those made the summary counts sum to less than the file's entry
/// count with nothing saying so — a 24-entry file reported `12 match, 0
/// mismatch, 0 not found, 0 parse errors` and read as a fully verified
/// save set while half of it had never been compared. The save side
/// records them (`connected: false`) and the restore side surfaces them
/// (`RestoreResult::disconnected_skipped`); verify was the only consumer
/// that dropped the information.
pub async fn verify(db: &PvDatabase, save_file_path: &Path) -> AutosaveResult<VerifyReport> {
    let read = read_partial_save_file(save_file_path).await?;

    // One push per entry, unconditionally: the classification decides
    // which bucket, never whether there is one.
    let entries = read
        .contents
        .entries
        .iter()
        .map(|entry| {
            let (result, live_value) = classify(db, entry);
            VerifyEntry {
                pv_name: entry.pv_name.clone(),
                saved_value: entry.value.clone(),
                live_value,
                result,
            }
        })
        .collect();

    Ok(VerifyReport {
        entries,
        malformed: read.contents.malformed,
        truncated: !read.complete,
    })
}

/// Decide one entry's bucket, and the live text to show beside it.
fn classify(db: &PvDatabase, entry: &SaveEntry) -> (MatchResult, Option<String>) {
    if !entry.connected {
        return (MatchResult::DisconnectedAtSave, None);
    }

    let Ok(live) = db.get_pv(&entry.pv_name) else {
        return (MatchResult::PvNotFound, None);
    };
    let live_str = save_file::value_to_save_str(&live);

    // Try parsing saved value using live type as template
    let Some(parsed) = save_file::parse_save_value(&entry.value, &live) else {
        return (MatchResult::ParseError, Some(live_str));
    };

    if parsed == live {
        (MatchResult::Match, Some(live_str))
    } else {
        (
            MatchResult::Mismatch {
                saved: entry.value.clone(),
                live: live_str.clone(),
            },
            Some(live_str),
        )
    }
}

/// Format a human-readable verify report.
///
/// The five counts sum to the number of entries in the file, and the
/// malformed lines are listed under it, so an operator can see that the
/// report covers everything the file contained.
pub fn format_verify_report(report: &VerifyReport) -> String {
    let mut out = String::new();
    if report.truncated {
        // First line and repeated in the summary: a truncation an
        // operator scrolls past is the same false all-clear as one that
        // was never reported.
        out.push_str("asVerify: Can't find <END> marker.  File may be bad.\n");
    }
    let mut match_count = 0;
    let mut mismatch_count = 0;
    let mut not_found_count = 0;
    let mut parse_error_count = 0;
    let mut disconnected_count = 0;

    for entry in &report.entries {
        match &entry.result {
            MatchResult::Match => {
                match_count += 1;
            }
            MatchResult::Mismatch { saved, live } => {
                mismatch_count += 1;
                out.push_str(&format!(
                    "MISMATCH: {} saved={} live={}\n",
                    entry.pv_name, saved, live
                ));
            }
            MatchResult::PvNotFound => {
                not_found_count += 1;
                out.push_str(&format!("NOT_FOUND: {}\n", entry.pv_name));
            }
            MatchResult::ParseError => {
                parse_error_count += 1;
                out.push_str(&format!(
                    "PARSE_ERROR: {} saved={}\n",
                    entry.pv_name, entry.saved_value
                ));
            }
            MatchResult::DisconnectedAtSave => {
                disconnected_count += 1;
                out.push_str(&format!(
                    "NOT_CHECKED: {} (disconnected at save)\n",
                    entry.pv_name
                ));
            }
        }
    }

    for line in &report.malformed {
        out.push_str(&format!(
            "MALFORMED: line {}: {}\n",
            line.line_no, line.text
        ));
    }

    out.push_str(&format!(
        "\nSummary: {} match, {} mismatch, {} not found, {} parse errors, \
         {} not checked (disconnected at save), {} malformed lines{}\n",
        match_count,
        mismatch_count,
        not_found_count,
        parse_error_count,
        disconnected_count,
        report.malformed.len(),
        if report.truncated {
            " -- INCOMPLETE FILE, no <END> marker"
        } else {
            ""
        }
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use crate::server::records::ao::AoRecord;

    /// H4 regression, and the C shape of it: `verify` against a
    /// truncated save file (no `<END>` marker) must report the
    /// truncation AND compare the entries the file does hold. The
    /// original defect was `read_save_file(...).unwrap_or_default()`
    /// collapsing the file into zero entries so the report claimed
    /// everything was fine; refusing the file outright hid the same
    /// thing the other way, since C's `do_asVerify_fp` warns and then
    /// verifies every line.
    #[epics_macros_rs::epics_test]
    async fn verify_on_corrupt_save_file_reports_it_and_still_compares() {
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

        let report = verify(&db, &path)
            .await
            .expect("a truncated file is verified, not refused");
        assert!(report.truncated, "the missing marker must be reported");
        assert_eq!(report.entries.len(), 2, "both lines must be compared");
        assert!(matches!(report.entries[0].result, MatchResult::Match));
        assert!(matches!(report.entries[1].result, MatchResult::PvNotFound));

        let text = format_verify_report(&report);
        assert!(
            text.contains("Can't find <END> marker"),
            "the report must open with the truncation; got: {text:?}"
        );
        assert!(
            text.contains("INCOMPLETE FILE"),
            "the summary line must carry it too; got: {text:?}"
        );
    }

    /// H4: a well-formed save file (with `<END>`) still verifies
    /// normally — the corruption guard does not break the happy path.
    #[epics_macros_rs::epics_test]
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

        let report = verify(&db, &path).await.expect("valid file must verify");
        assert_eq!(report.entries.len(), 1);
        assert!(matches!(report.entries[0].result, MatchResult::Match));
    }
}
