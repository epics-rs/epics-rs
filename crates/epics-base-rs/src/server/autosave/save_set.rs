use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::server::database::PvDatabase;
use tokio::sync::RwLock;

use super::backup::{
    BackupConfig, BackupState, BackupStep, SavbState, find_best_save_file, publish_savb,
    rotate_backups,
};
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
    /// The count the file's own `!` line declared, which for a C-written
    /// `.sav` is the only record that the save set was incomplete —
    /// `disconnected_skipped` can only list the PVs a per-PV marker
    /// named. 0 when the file carried no such line.
    pub not_connected_at_save: usize,
    /// Lines of the save file that named no PV at all. Distinct from
    /// `parse_failed`, which is per-PV: these lines never became an
    /// entry, so nothing else in this result can account for them.
    pub malformed_lines: Vec<MalformedLine>,
}

/// C's list and overall status codes (save_restore.h:38-42), the values the
/// SR_status and `SR_<i>_Status` mbbo records index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SrStatus {
    Init = 0,
    Fail = 1,
    Warn = 2,
    SeqWarn = 3,
    Ok = 4,
}

/// A list's status code and the text C writes beside it to `SR_<i>_StatusStr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListStatus {
    pub code: SrStatus,
    pub text: String,
}

impl ListStatus {
    fn new(code: SrStatus, text: &str) -> Self {
        Self {
            code,
            text: text.to_string(),
        }
    }
}

/// A save set: a named group of PVs with save/restore logic.
pub struct SaveSet {
    config: SaveSetConfig,
    entries: Vec<RequestEntry>,
    status: RwLock<SaveSetStatus>,
    stats: SaveSetStats,
    backup_state: RwLock<BackupState>,
    list_status: std::sync::Mutex<ListStatus>,
    save_time: std::sync::Mutex<chrono::DateTime<chrono::Local>>,
    recent: std::sync::Mutex<Option<(std::time::Instant, String)>>,
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
            list_status: std::sync::Mutex::new(ListStatus::new(
                SrStatus::Init,
                "Initializing list",
            )),
            save_time: std::sync::Mutex::new(chrono::Local::now()),
            recent: std::sync::Mutex::new(None),
            compat,
        })
    }

    /// Build the set's member list - and the one place an empty one is
    /// refused.
    ///
    /// A set with no members cannot save anything, but `save_once` would
    /// still run for it: the backup rotation copies the previous `.sav`
    /// into the sequence slots and then on to `.savB`, and
    /// `write_save_file_with_mode` renames a header-plus-`<END>` file
    /// over the `.sav` itself, so
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

    /// Every member's current value in this set's file encoding, an
    /// unreachable member as `connected: false`.
    fn collect_entries(&self, db: &PvDatabase) -> Vec<SaveEntry> {
        self.pv_names()
            .into_iter()
            .map(|pv| match db.get_pv(&pv) {
                Ok(val) => SaveEntry {
                    value: match self.compat {
                        CompatMode::Native => save_file::value_to_save_str(&val),
                        CompatMode::CRead => save_file::value_to_save_str_c(&val),
                    },
                    pv_name: pv,
                    connected: true,
                },
                Err(_) => SaveEntry {
                    pv_name: pv,
                    value: String::new(),
                    connected: false,
                },
            })
            .collect()
    }

    /// C `write_save_file(plist, configName, ...)` with a `configName`:
    /// this set's values written to `path` alone, with no backups and no
    /// change to the set's own file or status. Returns how many members
    /// could not be read.
    pub async fn save_to(&self, db: &PvDatabase, path: &std::path::Path) -> AutosaveResult<usize> {
        let entries = self.collect_entries(db);
        let missing = entries.iter().filter(|e| !e.connected).count();
        write_save_file_with_mode(path, &entries, self.compat).await?;
        Ok(missing)
    }

    /// Perform one save cycle: collect PV values -> guarantee `.savB` ->
    /// write `.sav` -> advance `.savB`.
    pub async fn save_once(&self, db: &PvDatabase) -> AutosaveResult<usize> {
        {
            let mut s = self.status.write().await;
            *s = SaveSetStatus::Saving;
        }

        // Collect PV values. Read before the rotation because the `.savB`
        // that C writes when the old one is unusable is written from the
        // values about to be saved, not copied from a file.
        let save_entries = self.collect_entries(db);

        let saved_count = save_entries.iter().filter(|e| e.connected).count();

        // Rotate backups. A rotation that could not preserve the current
        // `.sav`, or could not put a usable `.savB` in place first, aborts
        // the cycle instead of overwriting the generation it failed to
        // copy, and it lands in the same bookkeeping a failed write does —
        // an early `?` here left the set stuck in `Saving` with the error
        // counter untouched.
        let rotated = {
            let mut bs = self.backup_state.write().await;
            rotate_backups(
                &self.config.save_path,
                &self.config.backup,
                &mut bs,
                &save_entries,
                self.compat,
            )
            .await
        };
        let savb_state = match rotated {
            Ok(state) => state,
            Err(e) => {
                let status = match e.step {
                    BackupStep::Savb => ListStatus::new(SrStatus::Fail, "Can't write .savB file"),
                    BackupStep::Seq => {
                        ListStatus::new(SrStatus::SeqWarn, "Can't write sequence file")
                    }
                    BackupStep::Dated => {
                        ListStatus::new(SrStatus::Fail, "Can't write dated backup file")
                    }
                };
                return Err(self.record_save_failure(e.error, status).await);
            }
        };

        match write_save_file_with_mode(&self.config.save_path, &save_entries, self.compat).await {
            Ok(()) => {
                self.stats.save_count.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_saved_pv_count
                    .store(saved_count as u64, Ordering::Relaxed);
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                *self.stats.last_save_time.write().await = Some(now);
                *self.save_time.lock().unwrap() = chrono::Local::now();

                // The other half of the backup rule: `.savB` follows the
                // `.sav` that just landed unless the rotation already
                // wrote this generation into it. C reports a backup it
                // could not make as a failed cycle even though the `.sav`
                // is on disk, so the save stats above stand and the cycle
                // still fails.
                if let Err(e) = publish_savb(&self.config.save_path, savb_state).await {
                    self.note_recent(format!("Can't write '{}B'", self.save_file()));
                    return Err(self
                        .record_save_failure(
                            e,
                            ListStatus::new(SrStatus::Warn, "Can't copy .sav to .savB file"),
                        )
                        .await);
                }
                *self.status.write().await = SaveSetStatus::Idle;
                // C warns for a `.savB` it had to write anew and for values
                // it could not fetch (save_restore.c:2074, :2129).
                let not_saved = save_entries.len() - saved_count;
                *self.list_status.lock().unwrap() = if not_saved > 0 {
                    let noun = if not_saved == 1 { "value" } else { "values" };
                    ListStatus::new(SrStatus::Warn, &format!("{not_saved} {noun} not saved"))
                } else if savb_state == SavbState::Rewritten {
                    ListStatus::new(SrStatus::Warn, ".savB file was bad")
                } else {
                    ListStatus::new(SrStatus::Ok, "Ok")
                };
                self.note_recent(format!("Wrote '{}'", self.save_file()));

                Ok(saved_count)
            }
            Err(e) => {
                self.note_recent(format!("Can't write '{}'", self.save_file()));
                Err(self
                    .record_save_failure(
                        e,
                        ListStatus::new(SrStatus::Fail, "Can't write .sav file"),
                    )
                    .await)
            }
        }
    }

    /// C's `plist->status` and `statusStr` after this set's last save cycle.
    pub fn list_status(&self) -> ListStatus {
        self.list_status.lock().unwrap().clone()
    }

    /// This set's latest SR_recentlyStr event (save_restore.c:2104, :2121,
    /// :2134), stamped so the reporter can take the newest of all owners.
    pub fn recent(&self) -> Option<(std::time::Instant, String)> {
        self.recent.lock().unwrap().clone()
    }

    fn note_recent(&self, text: String) {
        *self.recent.lock().unwrap() = Some((std::time::Instant::now(), text));
    }

    /// C's `plist->save_file`, the name SR_recentlyStr and `SR_<i>_Name` use.
    pub fn save_file(&self) -> String {
        self.config
            .save_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    /// C's `plist->save_time`: the last `.sav` written, or the set's creation
    /// before any (save_restore.c:2437).
    pub fn save_time(&self) -> chrono::DateTime<chrono::Local> {
        *self.save_time.lock().unwrap()
    }

    /// The one place a failed save cycle is recorded. Both fallible steps
    /// — the backup rotation and the `.sav` write — go through it, so the
    /// status PVs and the error counters cannot disagree about whether the
    /// cycle failed. Returns the error it recorded so callers can `?` it.
    async fn record_save_failure(&self, e: AutosaveError, status: ListStatus) -> AutosaveError {
        *self.list_status.lock().unwrap() = status;
        self.stats.error_count.fetch_add(1, Ordering::Relaxed);
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        *self.stats.last_error_time.write().await = Some(now);
        *self.status.write().await = SaveSetStatus::Error(e.to_string());
        e
    }

    /// Perform one restore: find best file -> read -> put_pv_no_process.
    pub async fn restore_once(&self, db: &PvDatabase) -> AutosaveResult<RestoreResult> {
        self.restore_once_with_mode(db, RestoreMode::NoProcess)
            .await
    }

    /// [`Self::restore_once`] with an explicit [`RestoreMode`].
    pub async fn restore_once_with_mode(
        &self,
        db: &PvDatabase,
        mode: RestoreMode,
    ) -> AutosaveResult<RestoreResult> {
        let source = find_best_save_file(&self.config.save_path, &self.config.backup)
            .await
            .ok_or_else(|| AutosaveError::CorruptSaveFile {
                path: self.config.save_path.display().to_string(),
                message: "no valid save file found".to_string(),
            })?;

        restore_from_entries_with_mode(db, &source, mode).await
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
    /// chains do not run.
    ///
    /// This is C's behaviour, not a divergence from it: C-autosave
    /// restores with `dbPut` (`dbrestore.c:299-301`, pass 1) and with
    /// `dbPutString` / `dbPutMenuIndex` through static database access
    /// (pass 0) at `R6-0-20-g186f467`. `dbPutField` — the entry point
    /// that takes the lock, honours `PP` and calls `dbProcess`
    /// (`dbAccess.c` at `R7.0.10`) — appears nowhere in the module, and
    /// neither does `dbPutLink`. So a restored `.VAL` does not ripple
    /// to OUT links in C either, which is also what the common "restore
    /// a setpoint AO at boot" case wants: no spurious hardware write at
    /// startup.
    ///
    /// [`RestoreMode::Process`] is the port's own extension for the
    /// cases where restore correctness depends on processing.
    #[default]
    NoProcess,
    /// Write each PV with `put_record_field_from_ca_no_notify`, the
    /// `dbPutField` that C's `do_manual_restore` reaches through `ca_put`
    /// (save_restore.c:3245): `PP` fields, OUT links and FLNK chains run.
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
    Ok(restore_contents(db, path, &contents, mode).await)
}

/// C `fdbrestoreX`'s `do_manual_restore(file, FROM_ASCII_FILE, macros)`
/// (save_restore.c:3195): no header to skip and no `<END>` required, each
/// line macro-expanded with the set's macros before it is parsed, every
/// value written with `dbPutField`.
pub async fn restore_ascii_file(
    db: &PvDatabase,
    path: &std::path::Path,
    macros: &HashMap<String, String>,
) -> AutosaveResult<RestoreResult> {
    let text = crate::runtime::fs::read_to_string(path).await?;
    // Line by line, as C's `fgets` + `macExpandString`: an unterminated
    // reference must not swallow the lines after it. An undefined one stays
    // as text, and the put to the name it leaves fails and counts.
    let text = if macros.is_empty() {
        text
    } else {
        text.split_inclusive('\n')
            .map(|line| {
                crate::server::db_loader::expand_macros(line, macros, Default::default()).text
            })
            .collect()
    };
    let contents = save_file::parse_save_content(&text);
    Ok(restore_contents(db, path, &contents, RestoreMode::Process).await)
}

/// Write every entry of a parsed file into the database; one failed put
/// does not stop the rest.
async fn restore_contents(
    db: &PvDatabase,
    path: &std::path::Path,
    contents: &save_file::SaveFileContents,
    mode: RestoreMode,
) -> RestoreResult {
    let mut result = RestoreResult {
        source_file: path.to_path_buf(),
        restored: 0,
        failed_puts: Vec::new(),
        parse_failed: Vec::new(),
        not_found: Vec::new(),
        disconnected_skipped: Vec::new(),
        not_connected_at_save: contents.not_connected,
        malformed_lines: contents.malformed.clone(),
    };
    if result.not_connected_at_save > 0 {
        // C logs this and marks the set WARN
        // (`dbrestore.c:998-1001`); it aborts only when the site has
        // cleared `save_restoreIncompleteSetsOk`, which defaults to 1.
        let n = result.not_connected_at_save;
        crate::runtime::log::errlog_vprintf(format_args!(
            "autosave: {n} {} had no saved value.\n",
            if n == 1 { "PV" } else { "PVs" }
        ));
    }

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

        let put_result = match mode {
            RestoreMode::NoProcess => db.put_pv_no_process(&entry.pv_name, parsed).await,
            RestoreMode::Process => {
                let (base, field) = crate::server::database::parse_pv_name(&entry.pv_name);
                db.put_record_field_from_ca_no_notify(base, field, parsed)
                    .await
            }
        };
        match put_result {
            Ok(()) => {
                result.restored += 1;
            }
            Err(e) => {
                // Name the record and the status here, at the one site every
                // restore route reaches. `put_pv_no_process` now adopts the
                // after-put `special()` status the way C's `dbPut` does
                // (`dbAccess.c:1398-1404`), so a restore of an unresolvable
                // SNAM or an illegal CALC FAILS where it used to be accepted
                // silently — that must not become a silently DROPPED restore.
                // The iocsh summary prints only a count.
                eprintln!("autosave restore: {} put failed: {e}", entry.pv_name);
                result.failed_puts.push(PvRestoreError {
                    pv_name: entry.pv_name.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    result
}
