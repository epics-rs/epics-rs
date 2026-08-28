use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::Local;

use super::error::AutosaveResult;
use super::format::CompatMode;
use super::save_file::{SaveEntry, validate_save_file, write_save_file_with_mode};

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
    // backup artefact is written, and one caller (the `_SBAD_` copy of a
    // corrupt `.savB`) treats the failure as non-fatal and keeps no other
    // trace of it, so a backup silently stopped being written while the
    // set went on reporting successful saves.
    if let Err(ref e) = published {
        crate::runtime::log::errlog_printf(&format!(
            "autosave: backup {} -> {} not written: {e}\n",
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
/// (`Self::rotate_seq` and `Self::write_dated`) publish first and
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

/// Whether the `.savB` backup still holds the *outgoing* generation and
/// so has to be refreshed once the new `.sav` is on disk.
///
/// [`rotate_backups`] produces it before the `.sav` write and
/// [`publish_savb`] consumes it after, which is how C splits the same
/// rule across `write_save_file` (`save_restore.c` at `R6-0-20-g186f467`):
/// the pre-write half guarantees a usable `.savB`, the post-write half
/// advances it, and the post-write half is skipped exactly when the
/// pre-write half already wrote this generation (C's `BS_NEW`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavbState {
    /// `.savB` backups are switched off for this set.
    Disabled,
    /// A valid `.savB` was already on disk, so it still holds the
    /// generation about to be replaced.
    Ok,
    /// It was missing or corrupt and has just been written from the values
    /// about to be saved, so it already holds the new generation.
    Rewritten,
}

/// Make sure a valid `.savB` exists *before* the `.sav` is overwritten.
///
/// C pivots this decision on the `.savB` itself, not on the `.sav`
/// ("Ensure that backup is ok before we overwrite .sav file."): a `.savB`
/// that is missing or does not end in `<END>` is rewritten from the values
/// about to be saved, and a corrupt one is kept aside as
/// `<name>.savB_SBAD_<yymmdd-HHMMSS>` first. Pivoting on the `.sav`
/// instead — which is what this module used to do — skips all of it
/// whenever the `.sav` is the file that is missing or corrupt, so the one
/// cycle that most needs a backup is the one that leaves a single usable
/// file behind.
///
/// A `.savB` that cannot be written aborts the cycle with the `.sav`
/// untouched, so a set never trades its last good generation for a
/// backup it failed to make.
async fn ensure_savb(
    sav_path: &Path,
    config: &BackupConfig,
    entries: &[SaveEntry],
    compat: CompatMode,
) -> AutosaveResult<SavbState> {
    if !config.enable_savb {
        return Ok(SavbState::Disabled);
    }
    let savb = sav_path.with_extension("savB");
    let state = validate_save_file(&savb).await;
    if matches!(state, Ok(true)) {
        return Ok(SavbState::Ok);
    }

    crate::runtime::log::errlog_printf(&format!(
        "autosave: backup file ({}) bad or not found. Writing a new one.\n",
        savb.display()
    ));
    // `validate_save_file` reports an unopenable file as an error and a
    // file without the `<END>` marker as `Ok(false)`, which is C's
    // `BS_NONE` / `BS_BAD` split. Only the second one has content worth
    // keeping for diagnosis, and losing that copy is not worth failing
    // the cycle over — `publish_copy` already reports it.
    if matches!(state, Ok(false)) {
        let mut aside = savb.as_os_str().to_os_string();
        aside.push(format!("_SBAD_{}", Local::now().format("%y%m%d-%H%M%S")));
        let _ = publish_copy(&savb, Path::new(&aside)).await;
    }
    if let Err(e) = write_save_file_with_mode(&savb, entries, compat).await {
        // Announced here for the same reason `publish_copy` announces its
        // own failures: this is the one backup write that does not go
        // through it, and C prints the equivalent line before returning.
        crate::runtime::log::errlog_printf(&format!(
            "autosave: backup {} not written: {e}\n",
            savb.display()
        ));
        return Err(e);
    }
    Ok(SavbState::Rewritten)
}

/// Rotate backups before writing a new `.sav` file.
///
/// Order: guarantee `.savB` -> sequence rotation -> dated backup.
///
/// A failed rotation is reported rather than swallowed, and the caller
/// must not overwrite the `.sav` it could not preserve: the whole point
/// of the rotation is that the generation about to be replaced survives
/// somewhere else first.
///
/// The returned [`SavbState`] must be handed to [`publish_savb`] after the
/// `.sav` write; together they hold the invariant that a completed save
/// cycle always ends with a valid `.savB` alongside the new `.sav`.
pub async fn rotate_backups(
    sav_path: &Path,
    config: &BackupConfig,
    state: &mut BackupState,
    entries: &[SaveEntry],
    compat: CompatMode,
) -> AutosaveResult<SavbState> {
    let savb_state = ensure_savb(sav_path, config, entries, compat).await?;

    // The sequenced and dated copies are made from the generation about
    // to be replaced, so unlike `.savB` they have nothing to copy when
    // that generation is missing or corrupt.
    if sav_path.exists() && validate_save_file(sav_path).await.unwrap_or(false) {
        state.rotate_seq(sav_path, config).await?;
        state.write_dated(sav_path, config).await?;
    }

    Ok(savb_state)
}

/// Advance `.savB` to the generation just written, which is the other
/// half of the rule [`rotate_backups`] starts.
///
/// Skipped precisely when the pre-write half already wrote this
/// generation into `.savB` (C's `backup_state != BS_NEW` guard), so the
/// file is never written twice in one cycle.
pub async fn publish_savb(sav_path: &Path, savb_state: SavbState) -> AutosaveResult<()> {
    if savb_state != SavbState::Ok {
        return Ok(());
    }
    publish_copy(sav_path, &sav_path.with_extension("savB")).await
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
/// `BackupState::rotate_seq` fills them round-robin, so once the index
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
