use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::server::database::PvDatabase;
use tokio::sync::RwLock;

use super::backup::{BackupConfig, BackupState, find_best_save_file, rotate_backups};
use super::error::{AutosaveError, AutosaveResult};
use super::macros::MacroContext;
use super::request::{self, RequestEntry};
use super::save_file::{self, SaveEntry, read_save_file, write_save_file};

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
    pub parse_failed: Vec<String>,
    pub not_found: Vec<String>,
    pub disconnected_skipped: Vec<String>,
}

/// A save set: a named group of PVs with save/restore logic.
pub struct SaveSet {
    config: SaveSetConfig,
    entries: Vec<RequestEntry>,
    status: RwLock<SaveSetStatus>,
    stats: SaveSetStats,
    backup_state: RwLock<BackupState>,
}

impl SaveSet {
    /// Create a new save set, loading the request file if configured.
    pub async fn new(config: SaveSetConfig) -> AutosaveResult<Self> {
        let entries = Self::load_entries(&config).await?;
        Ok(Self {
            config,
            entries,
            status: RwLock::new(SaveSetStatus::Idle),
            stats: SaveSetStats::default(),
            backup_state: RwLock::new(BackupState::default()),
        })
    }

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
        Ok(entries)
    }

    /// Perform one save cycle: rotate backups -> collect PV values -> write file.
    pub async fn save_once(&self, db: &PvDatabase) -> AutosaveResult<usize> {
        {
            let mut s = self.status.write().await;
            *s = SaveSetStatus::Saving;
        }

        // Rotate backups
        {
            let mut bs = self.backup_state.write().await;
            rotate_backups(&self.config.save_path, &self.config.backup, &mut bs).await?;
        }

        // Collect PV values
        let pv_names = self.pv_names();
        let mut save_entries = Vec::with_capacity(pv_names.len());

        for pv in &pv_names {
            match db.get_pv(pv).await {
                Ok(val) => {
                    save_entries.push(SaveEntry {
                        pv_name: pv.clone(),
                        value: save_file::value_to_save_str(&val),
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

        match write_save_file(&self.config.save_path, &save_entries).await {
            Ok(()) => {
                self.stats.save_count.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_saved_pv_count
                    .store(saved_count as u64, Ordering::Relaxed);
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                *self.stats.last_save_time.write().await = Some(now);
                *self.status.write().await = SaveSetStatus::Idle;
                Ok(saved_count)
            }
            Err(e) => {
                self.stats.error_count.fetch_add(1, Ordering::Relaxed);
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                *self.stats.last_error_time.write().await = Some(now);
                let msg = e.to_string();
                *self.status.write().await = SaveSetStatus::Error(msg);
                Err(e)
            }
        }
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

/// Best-effort restore from a save file. Each PV is independent.
pub async fn restore_from_entries(
    db: &PvDatabase,
    path: &std::path::Path,
) -> AutosaveResult<RestoreResult> {
    let entries = read_save_file(path)
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
    };

    for entry in &entries {
        if !entry.connected {
            result.disconnected_skipped.push(entry.pv_name.clone());
            continue;
        }

        // Get current value to determine type
        let current = match db.get_pv(&entry.pv_name).await {
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

        match db.put_pv_no_process(&entry.pv_name, parsed).await {
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
