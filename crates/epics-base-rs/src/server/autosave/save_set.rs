use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::server::database::PvDatabase;
use tokio::sync::RwLock;

use super::backup::{BackupConfig, BackupState, find_best_save_file, publish_copy, rotate_backups};
use super::error::{AutosaveError, AutosaveResult};
use super::format::CompatMode;
use super::macros::MacroContext;
use super::request::{self, RequestEntry};
use super::save_file::{self, MalformedLine, SaveEntry, read_save_file, write_save_file_with_mode};

/// Save strategy for a save set.
#[derive(Debug, Clone)]
pub enum SaveStrategy {
    Periodic {
        interval: Duration,
    },
    Triggered {
        trigger_pv: String,
        mode: TriggerMode,
        poll_interval: Duration,
    },
    OnChange {
        min_interval: Duration,
        float_epsilon: f64,
    },
    Manual,
}

/// Trigger mode for triggered save sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    AnyChange,
    NonZero,
}

/// Configuration for a save set.
#[derive(Debug, Clone)]
pub struct SaveSetConfig {
    pub name: String,
    pub save_path: PathBuf,
    pub strategy: SaveStrategy,
    pub request_file: Option<PathBuf>,
    pub request_pvs: Vec<String>,
    pub backup: BackupConfig,
    pub macros: HashMap<String, String>,
    /// Search paths for resolving `file` includes within .req files.
    pub search_paths: Vec<PathBuf>,
}

/// Runtime status of a save set.
#[derive(Debug, Clone)]
pub enum SaveSetStatus {
    Idle,
    Saving,
    Error(String),
}

/// Runtime statistics for a save set.
pub struct SaveSetStats {
    pub save_count: AtomicU64,
    pub error_count: AtomicU64,
    pub last_save_time: RwLock<Option<String>>,
    pub last_error_time: RwLock<Option<String>>,
    pub last_saved_pv_count: AtomicU64,
}

impl Default for SaveSetStats {
    fn default() -> Self {
        Self {
            save_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            last_save_time: RwLock::new(None),
            last_error_time: RwLock::new(None),
            last_saved_pv_count: AtomicU64::new(0),
        }
    }
}

/// Error detail for a single PV during restore.
#[derive(Debug, Clone)]
pub struct PvRestoreError {
    pub pv_name: String,
    pub error: String,
}

/// Result of a restore operation.
#[derive(Debug)]
pub struct RestoreResult {
    pub source_file: PathBuf,
    pub restored: usize,
    pub failed_puts: Vec<PvRestoreError>,
    /// PV names whose saved value would not parse into the live type.
    pub parse_failed: Vec<String>,
    pub not_found: Vec<String>,
    pub disconnected_skipped: Vec<String>,
    /// Lines of the save file that named no PV at all. Distinct from
    /// `parse_failed`, which is per-PV: these lines never became an
    /// entry, so nothing else in this result can account for them.
    pub malformed_lines: Vec<MalformedLine>,
}

/// A save set: a named group of PVs with save/restore logic.
pub struct SaveSet {
    config: SaveSetConfig,
    entries: Vec<RequestEntry>,
    status: RwLock<SaveSetStatus>,
    stats: SaveSetStats,
    backup_state: RwLock<BackupState>,
    /// On-disk save-file format used by [`SaveSet::save_once`].
    /// [`CompatMode::Native`] by default; [`CompatMode::CRead`]
    /// writes `.sav` files a C IOC can read.
    compat: CompatMode,
}

impl SaveSet {
    /// Create a new save set in the native save-file format,
    /// loading the request file if configured.
    pub async fn new(config: SaveSetConfig) -> AutosaveResult<Self> {
        Self::new_with_mode(config, CompatMode::Native).await
    }

    /// Create a new save set with an explicit on-disk format.
    ///
    /// Use [`CompatMode::CRead`] when the produced `.sav` files must
    /// be readable by a C IOC (mixed Rust + C IOC site).
    pub async fn new_with_mode(config: SaveSetConfig, compat: CompatMode) -> AutosaveResult<Self> {
        let entries = Self::load_entries(&config).await?;
        Ok(Self {
            config,
            entries,
            status: RwLock::new(SaveSetStatus::Idle),
            stats: SaveSetStats::default(),
            backup_state: RwLock::new(BackupState::default()),
            compat,
        })
    }

    /// Build the set's member list - and the one place an empty one is
    /// refused.
    ///
    /// A set with no members cannot save anything, but `save_once` would
    /// still run for it: `rotate_backups` copies the previous `.sav` into
    /// `.savB` and the sequence slots, and `write_save_file_with_mode`
    /// then renames a header-plus-`<END>` file over the `.sav` itself, so
    /// within `num_seq_files` periods every generation of a real save set
    /// is gone and the next boot restores nothing while reporting
    /// success. Refusing the set here keeps it out of `SaveSet` entirely,
    /// where `AutosaveBuilder::build` drops it like any other unbuildable
    /// set and keeps it in `status_all` as an error, so no strategy needs
    /// its own emptiness check before calling `save_once` and
    /// `reload_request` cannot empty a set that is already running.
    async fn load_entries(config: &SaveSetConfig) -> AutosaveResult<Vec<RequestEntry>> {
        let macros = MacroContext::from_map(config.macros.clone());
        let mut entries = Vec::new();

        if let Some(ref req_file) = config.request_file {
            let req_entries = request::load_request_file_with_search_paths(
                &req_file.to_string_lossy(),
                &config.search_paths,
                &macros,
            )
            .await?;
            entries.extend(req_entries);
        }

        // Add inline PVs
        for pv in &config.request_pvs {
            entries.push(RequestEntry {
                pv_name: pv.clone(),
                source_file: PathBuf::from("<inline>"),
                line_no: 0,
                expanded_from: None,
            });
        }

        entries = request::dedup_entries(entries);
        if entries.is_empty() {
            return Err(AutosaveError::EmptySaveSet {
                name: config.name.clone(),
                reason: match &config.request_file {
                    Some(f) => format!("'{}' declared no PVs", f.display()),
                    None => "no request file, and no inline PVs".to_string(),
                },
            });
        }
        Ok(entries)
    }

    /// Perform one save cycle: rotate backups -> collect PV values -> write file.
    pub async fn save_once(&self, db: &PvDatabase) -> AutosaveResult<usize> {
        {
            let mut s = self.status.write().await;
            *s = SaveSetStatus::Saving;
        }

        // Rotate backups. A rotation that could not preserve the current
        // `.sav` aborts the cycle instead of overwriting the generation it
        // failed to copy, and it lands in the same bookkeeping a failed
        // write does — an early `?` here left the set stuck in `Saving`
        // with the error counter untouched.
        let rotated = {
            let mut bs = self.backup_state.write().await;
            rotate_backups(&self.config.save_path, &self.config.backup, &mut bs).await
        };
        if let Err(e) = rotated {
            return Err(self.record_save_failure(e).await);
        }

        // Collect PV values
        let pv_names = self.pv_names();
        let mut save_entries = Vec::with_capacity(pv_names.len());

        for pv in &pv_names {
            match db.get_pv(pv) {
                Ok(val) => {
                    // Encode the value in the configured format so
                    // the `.sav` file matches the header banner the
                    // file is written with.
                    let value = match self.compat {
                        CompatMode::Native => save_file::value_to_save_str(&val),
                        CompatMode::CRead => save_file::value_to_save_str_c(&val),
                    };
                    save_entries.push(SaveEntry {
                        pv_name: pv.clone(),
                        value,
                        connected: true,
                    });
                }
                Err(_) => {
                    save_entries.push(SaveEntry {
                        pv_name: pv.clone(),
                        value: String::new(),
                        connected: false,
                    });
                }
            }
        }

        let saved_count = save_entries.iter().filter(|e| e.connected).count();

        match write_save_file_with_mode(&self.config.save_path, &save_entries, self.compat).await {
            Ok(()) => {
                self.stats.save_count.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_saved_pv_count
                    .store(saved_count as u64, Ordering::Relaxed);
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                *self.stats.last_save_time.write().await = Some(now);
                *self.status.write().await = SaveSetStatus::Idle;

                // M5: seed `.savB` on the FIRST save. `rotate_backups`
                // runs before the write and early-returns when no
                // prior `.sav` exists, so on a fresh IOC the very
                // first cycle leaves only one usable file — a crash
                // during the 2nd-ever write would have no backup.
                // Copy the freshly-written `.sav` to `.savB` when the
                // backup file does not yet exist, so backup depth
                // matches C autosave (one cycle behind) from the
                // first save onward.
                //
                // Through `publish_copy` like every other backup write, and
                // non-fatal: the `.sav` is already on disk, and a seed that
                // did not land simply leaves `.savB` absent for the next
                // cycle's `rotate_backups` to create.
                if self.config.backup.enable_savb {
                    let savb_path = self.config.save_path.with_extension("savB");
                    if !savb_path.exists() {
                        let _ = publish_copy(&self.config.save_path, &savb_path).await;
                    }
                }

                Ok(saved_count)
            }
            Err(e) => Err(self.record_save_failure(e).await),
        }
    }

    /// The one place a failed save cycle is recorded. Both fallible steps
    /// — the backup rotation and the `.sav` write — go through it, so the
    /// status PVs and the error counters cannot disagree about whether the
    /// cycle failed. Returns the error it recorded so callers can `?` it.
    async fn record_save_failure(&self, e: AutosaveError) -> AutosaveError {
        self.stats.error_count.fetch_add(1, Ordering::Relaxed);
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        *self.stats.last_error_time.write().await = Some(now);
        *self.status.write().await = SaveSetStatus::Error(e.to_string());
        e
    }

    /// Perform one restore: find best file -> read -> put_pv_no_process.
    pub async fn restore_once(&self, db: &PvDatabase) -> AutosaveResult<RestoreResult> {
        let source = find_best_save_file(&self.config.save_path, &self.config.backup)
            .await
            .ok_or_else(|| AutosaveError::CorruptSaveFile {
                path: self.config.save_path.display().to_string(),
                message: "no valid save file found".to_string(),
            })?;

        restore_from_entries(db, &source).await
    }

    /// Reload request file.
    pub async fn reload_request(&mut self) -> AutosaveResult<()> {
        self.entries = Self::load_entries(&self.config).await?;
        Ok(())
    }

    pub async fn status(&self) -> SaveSetStatus {
        self.status.read().await.clone()
    }

    pub fn stats(&self) -> &SaveSetStats {
        &self.stats
    }

    pub fn config(&self) -> &SaveSetConfig {
        &self.config
    }

    pub fn pv_names(&self) -> Vec<String> {
        request::pv_names(&self.entries)
    }
}

/// Restore propagation mode — controls whether a restored value is
/// processed (rippling through PP / OUT links / FLNK chains) or
/// written silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestoreMode {
    /// Write each PV with `put_pv_no_process` — the value is set but
    /// the record is NOT processed, OUT links do not fire, FLNK
    /// chains do not run. This is the autosave-rs default and is the
    /// desired behaviour for the common "restore a setpoint AO at
    /// boot" case (it avoids a spurious hardware write at startup).
    ///
    /// This is a deliberate divergence from C-autosave's
    /// `reboot_restore`, which uses `dbPutField` (honours `PP`,
    /// processes the record, lets PINI/FLNK chains run). Restoring a
    /// `.VAL` that must ripple to OUT links, or a `.SCAN`/`.DOL`
    /// that downstream re-evaluation depends on, will NOT propagate
    /// in this mode — use [`RestoreMode::Process`] for those.
    #[default]
    NoProcess,
    /// Write each PV with `put_pv` — the record IS processed after
    /// the value is set, matching C-autosave's `dbPutField` restore:
    /// `PP` fields, OUT links and FLNK chains run. Use this when
    /// restore correctness depends on processing.
    Process,
}

/// Best-effort restore from a save file, in [`RestoreMode::NoProcess`].
/// Each PV is independent. See [`restore_from_entries_with_mode`] for
/// the process-on-restore variant.
pub async fn restore_from_entries(
    db: &PvDatabase,
    path: &std::path::Path,
) -> AutosaveResult<RestoreResult> {
    restore_from_entries_with_mode(db, path, RestoreMode::NoProcess).await
}

/// Best-effort restore from a save file with an explicit
/// [`RestoreMode`]. Each PV is independent — a failed put on one PV
/// does not abort the rest.
pub async fn restore_from_entries_with_mode(
    db: &PvDatabase,
    path: &std::path::Path,
    mode: RestoreMode,
) -> AutosaveResult<RestoreResult> {
    let contents = read_save_file(path)
        .await?
        .ok_or_else(|| AutosaveError::CorruptSaveFile {
            path: path.display().to_string(),
            message: "missing <END> marker".to_string(),
        })?;

    let mut result = RestoreResult {
        source_file: path.to_path_buf(),
        restored: 0,
        failed_puts: Vec::new(),
        parse_failed: Vec::new(),
        not_found: Vec::new(),
        disconnected_skipped: Vec::new(),
        malformed_lines: contents.malformed,
    };

    for entry in &contents.entries {
        if !entry.connected {
            result.disconnected_skipped.push(entry.pv_name.clone());
            continue;
        }

        // Get current value to determine type
        let current = match db.get_pv(&entry.pv_name) {
            Ok(v) => v,
            Err(_) => {
                result.not_found.push(entry.pv_name.clone());
                continue;
            }
        };

        let parsed = match save_file::parse_save_value(&entry.value, &current) {
            Some(v) => v,
            None => {
                result.parse_failed.push(entry.pv_name.clone());
                continue;
            }
        };

        // A DBF link field takes the no-process write in BOTH modes:
        // C `reboot_restore` puts via `dbPutField`, and `dbPutField` on a
        // link field is `dbPutFieldLink` (`dbAccess.c:1261`) — a re-parse
        // write that never processes. The `dbPut`-analogue `put_pv` would
        // refuse it (`S_db_badDbrtype`, dbAccess.c:1340).
        let (base, field) = crate::server::database::parse_pv_name(&entry.pv_name);
        let is_link_field = db.is_dbf_link_field(base, field);
        let put_result = match mode {
            RestoreMode::NoProcess => db.put_pv_no_process(&entry.pv_name, parsed).await,
            RestoreMode::Process if is_link_field => {
                db.put_pv_no_process(&entry.pv_name, parsed).await
            }
            RestoreMode::Process => db.put_pv(&entry.pv_name, parsed).await,
        };
        match put_result {
            Ok(()) => {
                result.restored += 1;
            }
            Err(e) => {
                result.failed_puts.push(PvRestoreError {
                    pv_name: entry.pv_name.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    Ok(result)
}
