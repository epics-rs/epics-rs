use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// Publish a copy of `src` at `dst`, all or nothing.
///
/// **The single owner of every backup artefact write.** `std::fs::copy`
/// opens the destination CREATE|TRUNCATE before it can fail, so copying
/// onto an existing backup destroys that generation the moment the write
/// runs short of space — the previous good `.savB` is left zero-length and
/// `find_best_save_file` then rejects it for the missing `<END>`. Copying
/// to a sibling temp path and renaming makes that intermediate state
/// unconstructible: `dst` holds its previous complete content or the new
/// copy, never a truncation.
///
/// The temp name is the destination plus `.tmp`, so it is in the same
/// directory (`rename` is only atomic within a filesystem) and cannot
/// collide with the `<base>.tmp` that [`super::save_file`] uses for the
/// `.sav` write itself.
///
/// Power-loss durability is unchanged and deliberately not claimed here:
/// a backup is derived data, and the `.sav` write owns the `fsync`
/// sequence that makes the original durable.
pub(super) async fn publish_copy(src: &Path, dst: &Path) -> AutosaveResult<()> {
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let src = src.to_path_buf();
    let dst = dst.to_path_buf();
    let published = crate::runtime::fs::blocking({
        let (src, dst, tmp) = (src.clone(), dst.clone(), tmp.clone());
        move || {
            let published = std::fs::copy(&src, &tmp).and_then(|_| std::fs::rename(&tmp, &dst));
            if published.is_err() {
                // The rename never happened, so this temp file is the only
                // trace of the attempt; leaving it would accumulate one per
                // failed cycle next to the save set.
                let _ = std::fs::remove_file(&tmp);
            }
            published
        }
    })
    .await;
    // Announced here rather than by the callers: this is the only place a
    // backup artefact is written, and one caller (the first-cycle `.savB`
    // seed) treats the failure as non-fatal and keeps no other trace of it,
    // so a backup silently stopped being written while the set went on
    // reporting successful saves.
    if let Err(ref e) = published {
        crate::runtime::log::errlog_printf(&format!(
            "autosave: backup {} -> {} not written: {e}",
            src.display(),
            dst.display()
        ));
    }
    published?;
    Ok(())
}

/// State for tracking timed backup operations.
///
/// Each field is a watermark meaning "the artefact it governs is on
/// disk". They are private, and the two transitions that move them
/// ([`Self::rotate_seq`] and [`Self::write_dated`]) publish first and
/// advance after, so a watermark standing ahead of the file it names
/// cannot be constructed. Advancing `seq_index` past a copy that did not
/// happen is what used to leave a destroyed slot unrewritten for a whole
/// `seq_period`.
#[derive(Debug, Default)]
pub struct BackupState {
    last_seq_time: Option<std::time::Instant>,
    last_dated_time: Option<std::time::Instant>,
    seq_index: usize,
}

impl BackupState {
    /// Copy `.sav` into the current sequence slot and take the next one.
    async fn rotate_seq(&mut self, sav_path: &Path, config: &BackupConfig) -> AutosaveResult<()> {
        if config.num_seq_files == 0 {
            return Ok(());
        }
        if self
            .last_seq_time
            .is_some_and(|t| t.elapsed() < config.seq_period)
        {
            return Ok(());
        }
        let ext = format!("sav{}", self.seq_index);
        publish_copy(sav_path, &sav_path.with_extension(&ext)).await?;
        self.seq_index = (self.seq_index + 1) % config.num_seq_files;
        self.last_seq_time = Some(std::time::Instant::now());
        Ok(())
    }

    /// Copy `.sav` to a timestamped name once per `dated_interval`.
    async fn write_dated(&mut self, sav_path: &Path, config: &BackupConfig) -> AutosaveResult<()> {
        if !config.enable_dated {
            return Ok(());
        }
        if self
            .last_dated_time
            .is_some_and(|t| t.elapsed() < config.dated_interval)
        {
            return Ok(());
        }
        let timestamp = Local::now().format("%y%m%d-%H%M%S");
        let ext = format!("sav_{timestamp}");
        publish_copy(sav_path, &sav_path.with_extension(&ext)).await?;
        self.last_dated_time = Some(std::time::Instant::now());
        Ok(())
    }
}

/// Rotate backups before writing a new .sav file.
/// Order: validate existing .sav -> .sav → .savB copy -> seq rotation -> dated backup
///
/// A failed rotation is reported rather than swallowed, and the caller
/// must not overwrite the `.sav` it could not preserve: the whole point
/// of the rotation is that the generation about to be replaced survives
/// somewhere else first.
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

    if config.enable_savb {
        publish_copy(sav_path, &sav_path.with_extension("savB")).await?;
    }
    state.rotate_seq(sav_path, config).await?;
    state.write_dated(sav_path, config).await?;

    Ok(())
}

/// Modification time of `path`, or `None` when the filesystem will not
/// say. A candidate whose age is unknown sorts oldest — `None < Some(_)`
/// — so it is still selectable when it is the only one left.
async fn modified_time(path: &Path) -> Option<SystemTime> {
    let path = path.to_path_buf();
    crate::runtime::fs::blocking(move || std::fs::metadata(&path)?.modified())
        .await
        .ok()
}

/// Find the best available save file for restore.
///
/// Priority: `.sav`, then `.savB`, then the NEWEST valid sequence slot.
///
/// The first two are a real ordering: `.savB` is refreshed from `.sav` on
/// every rotation cycle, so it is never older than a sequence slot, which
/// is refreshed only once per `seq_period`. The slots are not an ordering.
/// [`BackupState::rotate_seq`] fills them round-robin, so once the index
/// has wrapped, slot 0 is as likely to hold the oldest generation as the
/// newest — reading them by index restored a file up to
/// `num_seq_files - 1` rotation periods stale and named it in
/// `RestoreResult.source_file` with no warning, silently reverting every
/// setpoint changed in that window.
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

    // Sequence slots, newest first.
    let mut newest: Option<(Option<SystemTime>, PathBuf)> = None;
    for i in 0..config.num_seq_files {
        let ext = format!("sav{i}");
        let seq_path = base_path.with_extension(&ext);
        if !matches!(validate_save_file(&seq_path).await, Ok(true)) {
            continue;
        }
        let mtime = modified_time(&seq_path).await;
        if newest.as_ref().is_none_or(|(best, _)| mtime > *best) {
            newest = Some((mtime, seq_path));
        }
    }

    newest.map(|(_, path)| path)
}
