use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;

use super::error::AutosaveResult;
use super::save_file::validate_save_file;

/// Backup policy configuration.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Enable .savB backup (default: true)
    pub enable_savb: bool,
    /// Number of sequence files .sav0-.savN (default: 3, 0=disable)
    pub num_seq_files: usize,
    /// Sequence rotation period (default: 60s)
    pub seq_period: Duration,
    /// Enable dated backups .sav_YYMMDD-HHMMSS (default: false)
    pub enable_dated: bool,
    /// Dated backup interval (default: 1h)
    pub dated_interval: Duration,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enable_savb: true,
            num_seq_files: 3,
            seq_period: Duration::from_secs(60),
            enable_dated: false,
            dated_interval: Duration::from_secs(3600),
        }
    }
}

/// State for tracking timed backup operations.
#[derive(Debug)]
pub struct BackupState {
    pub last_seq_time: Option<std::time::Instant>,
    pub last_dated_time: Option<std::time::Instant>,
    pub seq_index: usize,
}

impl Default for BackupState {
    fn default() -> Self {
        Self {
            last_seq_time: None,
            last_dated_time: None,
            seq_index: 0,
        }
    }
}

/// Rotate backups before writing a new .sav file.
/// Order: validate existing .sav -> .sav → .savB copy -> seq rotation -> dated backup
pub async fn rotate_backups(
    sav_path: &Path,
    config: &BackupConfig,
    state: &mut BackupState,
) -> AutosaveResult<()> {
    // Only rotate if the current .sav exists and is valid
    if !sav_path.exists() {
        return Ok(());
    }

    let is_valid = validate_save_file(sav_path).await.unwrap_or(false);
    if !is_valid {
        return Ok(());
    }

    // .sav → .savB
    if config.enable_savb {
        let savb_path = sav_path.with_extension("savB");
        let _ = tokio::fs::copy(sav_path, &savb_path).await;
    }

    // Sequence rotation
    if config.num_seq_files > 0 {
        let should_rotate = state
            .last_seq_time
            .map_or(true, |t| t.elapsed() >= config.seq_period);

        if should_rotate {
            let ext = format!("sav{}", state.seq_index);
            let seq_path = sav_path.with_extension(&ext);
            let _ = tokio::fs::copy(sav_path, &seq_path).await;
            state.seq_index = (state.seq_index + 1) % config.num_seq_files;
            state.last_seq_time = Some(std::time::Instant::now());
        }
    }

    // Dated backup
    if config.enable_dated {
        let should_date = state
            .last_dated_time
            .map_or(true, |t| t.elapsed() >= config.dated_interval);

        if should_date {
            let timestamp = Local::now().format("%y%m%d-%H%M%S");
            let ext = format!("sav_{timestamp}");
            let dated_path = sav_path.with_extension(&ext);
            let _ = tokio::fs::copy(sav_path, &dated_path).await;
            state.last_dated_time = Some(std::time::Instant::now());
        }
    }

    Ok(())
}

/// Find the best available save file for restore.
/// Priority: .sav → .savB → .sav0 → .sav1 → ...
pub async fn find_best_save_file(base_path: &Path, config: &BackupConfig) -> Option<PathBuf> {
    // Try .sav first
    if let Ok(true) = validate_save_file(base_path).await {
        return Some(base_path.to_path_buf());
    }

    // Try .savB
    if config.enable_savb {
        let savb = base_path.with_extension("savB");
        if let Ok(true) = validate_save_file(&savb).await {
            return Some(savb);
        }
    }

    // Try sequence files
    for i in 0..config.num_seq_files {
        let ext = format!("sav{i}");
        let seq_path = base_path.with_extension(&ext);
        if let Ok(true) = validate_save_file(&seq_path).await {
            return Some(seq_path);
        }
    }

    None
}
