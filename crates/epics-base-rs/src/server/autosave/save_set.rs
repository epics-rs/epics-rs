use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::server::database::PvDatabase;
use crate::types::EpicsValue;
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
    /// C TRIGGERED: a save on each monitor event of `trigger_pv`, the
    /// first of which is its value when the list is enabled.
    Triggered {
        trigger_pv: String,
    },
    /// C MONITORED: a save once `min_interval` has run out and a member
    /// posted an event, the first being each member's value at enable.
    OnChange {
        min_interval: Duration,
    },
    Manual,
}

impl SaveStrategy {
    /// C's `save_method` bits (save_restore.c:205-210).
    pub const PERIODIC: u32 = 0x01;
    pub const TRIGGERED: u32 = 0x02;
    /// C `MONITORED`, `TIMER | CHANGE`.
    pub const MONITORED: u32 = 0x0c;
    pub const MANUAL: u32 = 0x10;

    /// This method's `save_method` bits.
    pub fn method_bit(&self) -> u32 {
        match self {
            Self::Periodic { .. } => Self::PERIODIC,
            Self::Triggered { .. } => Self::TRIGGERED,
            Self::OnChange { .. } => Self::MONITORED,
            Self::Manual => Self::MANUAL,
        }
    }

    /// The method that makes a list, as C `create_data_set` stores it:
    /// `MAX(period, MIN_PERIOD)` (save_restore.c:394, 2422, 2433). A method
    /// added to an existing list keeps its period raw (:2384-2389).
    pub(crate) fn creating_list(self) -> Self {
        const MIN_PERIOD: Duration = Duration::from_secs(4);
        match self {
            Self::Periodic { interval } => Self::Periodic {
                interval: interval.max(MIN_PERIOD),
            },
            Self::OnChange { min_interval } => Self::OnChange {
                min_interval: min_interval.max(MIN_PERIOD),
            },
            m => m,
        }
    }
}

/// Configuration for a save set: what C's `chlist` is built from. How the
/// set is saved is not part of it — a set gains its methods one
/// `create_*_set` at a time ([`SaveSet::methods`]).
#[derive(Debug, Clone)]
pub struct SaveSetConfig {
    pub name: String,
    pub save_path: PathBuf,
    pub request_file: Option<PathBuf>,
    pub request_pvs: Vec<String>,
    pub backup: BackupConfig,
    pub macros: HashMap<String, String>,
    /// Search paths for resolving `file` includes within .req files.
    pub search_paths: Vec<PathBuf>,
}

impl SaveSetConfig {
    /// What identifies the set: C matches `plist->reqFile` against the
    /// filename (save_restore.c:2369). A set of inline PVs has only its name.
    pub fn key(&self) -> String {
        match &self.request_file {
            Some(f) => f.to_string_lossy().into_owned(),
            None => self.name.clone(),
        }
    }
}

/// A set's save methods, at most one of each kind: C's `save_method` mask
/// plus each method's parameters.
#[derive(Debug, Clone, Default)]
pub struct SaveMethods(Vec<SaveStrategy>);

impl SaveMethods {
    /// Add `method`, or refuse it with its bit when the set already has
    /// one of that kind (save_restore.c:2370-2375).
    pub fn add(&mut self, method: SaveStrategy) -> Result<(), u32> {
        let bit = method.method_bit();
        if self.mask() & bit != 0 {
            return Err(bit);
        }
        self.0.push(method);
        Ok(())
    }

    /// C `plist->save_method`.
    pub fn mask(&self) -> u32 {
        self.0.iter().fold(0, |m, s| m | s.method_bit())
    }

    pub fn iter(&self) -> impl Iterator<Item = &SaveStrategy> {
        self.0.iter()
    }
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
    methods: std::sync::Mutex<SaveMethods>,
    /// When C's `create_data_set` stamped `callback_time` and
    /// `save_attempt_time` (save_restore.c:2436, :2472).
    created_at: std::time::Instant,
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
        let (entries, list_status) = Self::load_entries(&config).await?;
        Ok(Self {
            config,
            entries,
            status: RwLock::new(SaveSetStatus::Idle),
            stats: SaveSetStats::default(),
            backup_state: RwLock::new(BackupState::default()),
            list_status: std::sync::Mutex::new(list_status),
            save_time: std::sync::Mutex::new(chrono::Local::now()),
            recent: std::sync::Mutex::new(None),
            methods: std::sync::Mutex::new(SaveMethods::default()),
            created_at: std::time::Instant::now(),
            compat,
        })
    }

    /// Build the set's member list. An empty one is a list C builds and
    /// saves like any other (save_restore.c:998-1004).
    ///
    /// Also returns the list status C's `create_data_set` leaves: the code
    /// `readReqFile` set, the text "Ready to connect..." (save_restore.c:2481).
    async fn load_entries(
        config: &SaveSetConfig,
    ) -> AutosaveResult<(Vec<RequestEntry>, ListStatus)> {
        let macros = MacroContext::from_map(config.macros.clone());
        let mut entries = Vec::new();
        let mut code = SrStatus::Init;

        if let Some(ref req_file) = config.request_file {
            let req_entries = request::load_request_file_with_search_paths(
                &req_file.to_string_lossy(),
                &config.search_paths,
                &macros,
            )
            .await?;
            if req_entries.include_unopened {
                code = SrStatus::Fail;
            }
            entries.extend(req_entries.entries);
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
        Ok((entries, ListStatus::new(code, "Ready to connect...")))
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

    /// Perform one pass-1 boot restore of the best file.
    pub async fn restore_once(&self, db: &PvDatabase) -> AutosaveResult<RestoreResult> {
        self.restore_once_with_mode(db, RestoreMode::Boot(RestorePass::One))
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
        let (entries, status) = Self::load_entries(&self.config).await?;
        self.entries = entries;
        *self.list_status.lock().unwrap() = status;
        Ok(())
    }

    pub async fn status(&self) -> SaveSetStatus {
        self.status.read().await.clone()
    }

    pub fn stats(&self) -> &SaveSetStats {
        &self.stats
    }

    /// The methods the set saves by, in the order they were added.
    pub fn methods(&self) -> Vec<SaveStrategy> {
        self.methods.lock().unwrap().iter().cloned().collect()
    }

    pub(super) fn created_at(&self) -> std::time::Instant {
        self.created_at
    }

    /// C `plist->period`, which only a PERIODIC set reads.
    pub(super) fn period(&self) -> Duration {
        self.methods
            .lock()
            .unwrap()
            .iter()
            .find_map(|m| match m {
                SaveStrategy::Periodic { interval } => Some(*interval),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// C `plist->monitor_period`, which only a MONITORED set reads.
    pub(super) fn monitor_period(&self) -> Duration {
        self.methods
            .lock()
            .unwrap()
            .iter()
            .find_map(|m| match m {
                SaveStrategy::OnChange { min_interval } => Some(*min_interval),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// C `plist->trigger_channel`.
    pub(super) fn trigger_channel(&self) -> Option<String> {
        self.methods.lock().unwrap().iter().find_map(|m| match m {
            SaveStrategy::Triggered { trigger_pv } => Some(trigger_pv.clone()),
            _ => None,
        })
    }

    /// `strNcpy(plist->statusStr, ...)` without a status code change, as
    /// `enable_list` reports itself (save_restore.c:1431).
    pub(super) fn set_status_text(&self, text: &str) {
        self.list_status.lock().unwrap().text = text.to_string();
    }

    /// C `plist->save_method`.
    pub fn save_method(&self) -> u32 {
        self.methods.lock().unwrap().mask()
    }

    /// Add one method; only the manager does, so a method and the task
    /// that serves it are added together.
    pub(super) fn add_method(&self, method: SaveStrategy) -> AutosaveResult<()> {
        self.methods
            .lock()
            .unwrap()
            .add(method)
            .map_err(|bit| AutosaveError::AlreadyInMode {
                request_file: self.config.key(),
                bit,
            })
    }

    pub fn config(&self) -> &SaveSetConfig {
        &self.config
    }

    pub fn pv_names(&self) -> Vec<String> {
        request::pv_names(&self.entries)
    }
}

/// How a restore stores what it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    /// C `reboot_restore(file, pass)` (`dbrestore.c:765-1080`), the boot
    /// restore: a scalar through the static database (`scalar_restore`), an
    /// array straight into the field (`SR_put_array_values`). Neither runs
    /// `special()` or processes; only a `DBF_NOACCESS` scalar takes `dbPut`.
    Boot(RestorePass),
    /// Write each PV with `put_record_field_from_ca_no_notify`, the
    /// `dbPutField` that C's `do_manual_restore` reaches through `ca_put`
    /// (save_restore.c:3245): `PP` fields, OUT links and FLNK chains run.
    Process,
}

/// The `iocInit` point a boot restore runs at: pass 0 before the records'
/// `init_record`, pass 1 after (`initHookAfterInitDevSup`,
/// `initHookAfterInitDatabase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePass {
    Zero,
    One,
}

/// Best-effort pass-1 boot restore from a save file. Each PV is
/// independent. See [`restore_from_entries_with_mode`] for the others.
pub async fn restore_from_entries(
    db: &PvDatabase,
    path: &std::path::Path,
) -> AutosaveResult<RestoreResult> {
    restore_from_entries_with_mode(db, path, RestoreMode::Boot(RestorePass::One)).await
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

        let put_result = match mode {
            RestoreMode::Boot(pass) => boot_store(db, entry, &current, pass).await,
            RestoreMode::Process => match save_file::parse_save_value(&entry.value, &current) {
                Some(parsed) => {
                    let (base, field) = crate::server::database::parse_pv_name(&entry.pv_name);
                    db.put_record_field_from_ca_no_notify(base, field, parsed)
                        .await
                        .map(|()| Stored::Yes)
                        .map_err(StoreError::Put)
                }
                None => Err(StoreError::Parse),
            },
        };
        match put_result {
            Ok(Stored::Yes) => {
                result.restored += 1;
            }
            Ok(Stored::Skipped) => {}
            Err(StoreError::Parse) => result.parse_failed.push(entry.pv_name.clone()),
            Err(StoreError::Put(e)) => {
                // Named here, at the one site every restore route reaches:
                // the iocsh summary prints only a count.
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

/// Whether [`boot_store`] wrote the value or C's pass rules left it alone.
enum Stored {
    Yes,
    Skipped,
}

enum StoreError {
    Parse,
    Put(crate::error::CaError),
}

/// One saved value into the database as C `reboot_restore` stores it
/// (`dbrestore.c:963-991`): an array, or a scalar by its declared type
/// (`scalar_restore`, `:222-327`).
async fn boot_store(
    db: &PvDatabase,
    entry: &SaveEntry,
    current: &EpicsValue,
    pass: RestorePass,
) -> Result<Stored, StoreError> {
    use crate::types::DbfCode;

    // C looks the field up without the long-string `$` (`:948-951`).
    let name = entry.pv_name.strip_suffix('$');
    let long_string = name.is_some();
    let Some(target) = db.restore_target(name.unwrap_or(&entry.pv_name)) else {
        // A PV no record serves has no special() and no processing to skip.
        let parsed = save_file::parse_save_value(&entry.value, current).ok_or(StoreError::Parse)?;
        return db
            .put_pv_no_process(&entry.pv_name, parsed)
            .await
            .map(|()| Stored::Yes)
            .map_err(StoreError::Put);
    };
    let put = |r: crate::error::CaResult<()>| r.map(|()| Stored::Yes).map_err(StoreError::Put);
    // `SR_array_restore`, for an `@array@` value or a field of more than one
    // element (`:914-928`), written in pass 1 only (`:722-723`).
    if !long_string && (target.elements > 1 || current.is_array()) {
        if pass == RestorePass::Zero {
            return Ok(Stored::Skipped);
        }
        let parsed = save_file::parse_save_value(&entry.value, current).ok_or(StoreError::Parse)?;
        return put(db.put_pv_array_static(&entry.pv_name, parsed));
    }
    // `value_string` after `epicsStrnRawFromEscaped` (`:235`).
    let text = match save_file::parse_save_value(&entry.value, &EpicsValue::String("".into())) {
        Some(EpicsValue::String(s)) => s.to_string(),
        _ => entry.value.clone(),
    };
    match target.dbf {
        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink => match pass {
            RestorePass::Zero => put(db.put_pv_static(&entry.pv_name, &text).await),
            RestorePass::One => Ok(Stored::Skipped),
        },
        // `dbPutMenuIndex(pdbentry, atol(value_string))`.
        DbfCode::Menu => put(db
            .put_pv_menu_index(&entry.pv_name, c_atol(&text) as i32)
            .await),
        DbfCode::NoAccess => match pass {
            RestorePass::Zero => Ok(Stored::Skipped),
            RestorePass::One => {
                let parsed =
                    save_file::parse_save_value(&entry.value, current).ok_or(StoreError::Parse)?;
                put(db.put_pv_no_process(&entry.pv_name, parsed).await)
            }
        },
        // The switch has no arm for it (`:306-311`).
        DbfCode::Device => Err(StoreError::Put(crate::error::CaError::InvalidValue(
            format!("field_type '{}' not handled", DbfCode::Device as u8),
        ))),
        // `SPC_CALC` fields may not be empty (`:258-260`).
        DbfCode::String if target.calc && text.is_empty() => {
            put(db.put_pv_static(&entry.pv_name, "0").await)
        }
        _ => put(db.put_pv_static(&entry.pv_name, &text).await),
    }
}

/// C `atol`: leading space, an optional sign, then digits up to the first
/// non-digit; 0 when there are none.
fn c_atol(text: &str) -> i64 {
    let t = text.trim_start();
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let n = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i64, |n, d| {
            n.wrapping_mul(10).wrapping_add(i64::from(d - b'0'))
        });
    if negative { n.wrapping_neg() } else { n }
}
