use std::path::Path;

use crate::server::database::PvDatabase;

use super::error::AutosaveResult;
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
pub async fn verify(db: &PvDatabase, save_file_path: &Path) -> AutosaveResult<Vec<VerifyEntry>> {
    let entries = read_save_file(save_file_path).await?.unwrap_or_default();

    let mut results = Vec::new();

    for entry in &entries {
        if !entry.connected {
            continue;
        }

        let live = match db.get_pv(&entry.pv_name).await {
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
