use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::server::database::PvDatabase;
use crate::types::EpicsValue;
use tokio::sync::RwLock;

use super::backup::{
    self, BackupConfig, SavbState, fopen_and_check, publish_copy, publish_savb, rotate_backups,
};
use super::error::{AutosaveError, AutosaveResult};
use super::macros::MacroContext;
use super::request::{self, RequestEntry};
use super::save_file::{
    self, MalformedLine, SaveEntry, read_save_file, write_save_file_with_settings,
};
use super::settings::SrSettings;

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
    /// C `plist->macrostring` as the `create_*_set` gave it, which only
    /// `save_restoreShow` prints; `macros` is what the request file expands
    /// with.
    pub macro_string: String,
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
    /// The restore stopped at the file's `!` line because
    /// `save_restoreIncompleteSetsOk` is cleared (dbrestore.c:1003-1012,
    /// save_restore.c:3430-3442).
    pub aborted: bool,
}

impl RestoreResult {
    fn of(path: &std::path::Path) -> Self {
        Self {
            source_file: path.to_path_buf(),
            restored: 0,
            failed_puts: Vec::new(),
            parse_failed: Vec::new(),
            not_found: Vec::new(),
            disconnected_skipped: Vec::new(),
            not_connected_at_save: 0,
            malformed_lines: Vec::new(),
            aborted: false,
        }
    }

    /// Fold in the result of the file's line `index` (0-based).
    fn absorb(&mut self, r: RestoreResult, index: usize) {
        self.restored += r.restored;
        self.failed_puts.extend(r.failed_puts);
        self.parse_failed.extend(r.parse_failed);
        self.not_found.extend(r.not_found);
        self.disconnected_skipped.extend(r.disconnected_skipped);
        self.not_connected_at_save += r.not_connected_at_save;
        self.malformed_lines
            .extend(r.malformed_lines.into_iter().map(|m| MalformedLine {
                line_no: m.line_no + index,
                text: m.text,
            }));
        self.aborted |= r.aborted;
    }
}

/// C `NUM_STATUS_PV_SETS` (save_restore.c:288).
pub const NUM_STATUS_PV_SETS: usize = 8;

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

impl SrStatus {
    /// C `SR_STATUS_STR` (save_restore.c:347).
    pub fn label(self) -> &'static str {
        match self {
            Self::Init => "No Status",
            Self::Fail => " Failure ",
            Self::Warn | Self::SeqWarn => " Warning ",
            Self::Ok => "    Ok   ",
        }
    }
}

/// A list's status code and the text C writes beside it to `SR_<i>_StatusStr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListStatus {
    pub code: SrStatus,
    pub text: String,
}

/// C's WARN for values `get_channel_values` could not fetch
/// (save_restore.c:2127-2130).
/// A member's value as C's `ca_array_get` reads it (`save_restore.c:1562`):
/// a channel `dbChannelCreate` makes, read through its filters, a `$`
/// member as its chars. `None` for a member no channel serves. The create
/// is the read's own, as C's channel was made at `connect_list`, so it
/// prints nothing.
pub(crate) fn read_member(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
    crate::server::database::filters::channel::db_channel_create_unreported(db, name.as_bytes())
        .ok()?
        .get(db)
}

fn not_saved_status(n: usize) -> ListStatus {
    let noun = if n == 1 { "value" } else { "values" };
    ListStatus::new(SrStatus::Warn, &format!("{n} {noun} not saved"))
}

impl ListStatus {
    pub(super) fn new(code: SrStatus, text: &str) -> Self {
        Self {
            code,
            text: text.to_string(),
        }
    }
}

/// C's `backup_sequence_num` (-1 until the first `do_seq`) and
/// `backup_time` (the list's creation, then the last sequence file).
struct SeqState {
    slot: Option<usize>,
    backup_time: std::time::Instant,
}

/// A save set: a named group of PVs with save/restore logic.
pub struct SaveSet {
    /// As created; its `save_path` is moved into [`Self::save_path`].
    config: SaveSetConfig,
    /// C's `plist->save_file` under the save path, which
    /// `set_savefile_name` renames.
    save_path: std::sync::Mutex<PathBuf>,
    entries: Vec<RequestEntry>,
    status: RwLock<SaveSetStatus>,
    stats: SaveSetStats,
    seq: std::sync::Mutex<SeqState>,
    /// C `last_save_file`: the file the last save, configMenu's included,
    /// wrote (save_restore.c:2110), empty before any.
    last_save_file: std::sync::Mutex<String>,
    /// C `save_state` and `not_connected` as the save task left them at the
    /// end of the list's last pass, which is what C's `save_restoreShow`
    /// sees under the list lock.
    task_view: std::sync::Mutex<(u32, usize)>,
    /// C `statusPvIndex`, the `SR_<i>_` slot the manager gave the list;
    /// `NUM_STATUS_PV_SETS` when all were in use.
    status_pv_index: std::sync::atomic::AtomicUsize,
    list_status: std::sync::Mutex<ListStatus>,
    save_time: std::sync::Mutex<chrono::DateTime<chrono::Local>>,
    recent: std::sync::Mutex<Option<(std::time::Instant, String)>>,
    methods: std::sync::Mutex<SaveMethods>,
    /// When C's `create_data_set` stamped `callback_time` and
    /// `save_attempt_time` (save_restore.c:2436, :2472).
    created_at: std::time::Instant,
    settings: Arc<SrSettings>,
}

impl SaveSet {
    /// Create a new save set, loading the request file if configured.
    pub async fn new(config: SaveSetConfig) -> AutosaveResult<Self> {
        Self::new_with_settings(config, Arc::default()).await
    }

    /// Create a new save set that reads the IOC's settings.
    pub async fn new_with_settings(
        config: SaveSetConfig,
        settings: Arc<SrSettings>,
    ) -> AutosaveResult<Self> {
        let (entries, list_status) = Self::load_entries(&config, &settings).await?;
        let created_at = std::time::Instant::now();
        Ok(Self {
            save_path: std::sync::Mutex::new(config.save_path.clone()),
            config,
            entries,
            status: RwLock::new(SaveSetStatus::Idle),
            stats: SaveSetStats::default(),
            seq: std::sync::Mutex::new(SeqState {
                slot: None,
                backup_time: created_at,
            }),
            last_save_file: std::sync::Mutex::new(String::new()),
            task_view: std::sync::Mutex::new((0, 0)),
            status_pv_index: std::sync::atomic::AtomicUsize::new(NUM_STATUS_PV_SETS),
            list_status: std::sync::Mutex::new(list_status),
            save_time: std::sync::Mutex::new(chrono::Local::now()),
            recent: std::sync::Mutex::new(None),
            methods: std::sync::Mutex::new(SaveMethods::default()),
            created_at,
            settings,
        })
    }

    /// Build the set's member list. An empty one is a list C builds and
    /// saves like any other (save_restore.c:998-1004).
    ///
    /// Also returns the list status C's `create_data_set` leaves: the code
    /// `readReqFile` set, the text "Ready to connect..." (save_restore.c:2481).
    async fn load_entries(
        config: &SaveSetConfig,
        settings: &SrSettings,
    ) -> AutosaveResult<(Vec<RequestEntry>, ListStatus)> {
        let macros = MacroContext::from_map(config.macros.clone());
        let mut entries = Vec::new();
        let mut code = SrStatus::Init;

        if let Some(ref req_file) = config.request_file {
            let req_entries = request::read_request_list(
                &req_file.to_string_lossy(),
                &config.search_paths,
                &macros,
                &config.macro_string,
                settings,
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
    pub fn collect_entries(&self, db: &PvDatabase) -> Vec<SaveEntry> {
        let debug = self.settings.debug();
        let entries: Vec<SaveEntry> = self
            .pv_names()
            .into_iter()
            .map(|pv| match read_member(db, &pv) {
                Some(val) => {
                    // C `SR_get_array` for a member of more than one element
                    // (save_restore.c:1576-1579, dbrestore.c:1357-1360).
                    if self.settings.debug() >= 10
                        && db.restore_target(&pv).is_some_and(|t| t.elements > 1)
                    {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "../dbrestore.c:SR_get_array '{pv}' currently has {} elements\n",
                            val.count()
                        ));
                    }
                    if debug >= 15 {
                        println!(
                            "save_restore:get_channel_values: '{pv}' currently has {} elements",
                            val.count()
                        );
                    }
                    SaveEntry {
                        value: save_file::value_to_save_str(&val),
                        pv_name: pv,
                        connected: true,
                    }
                }
                None => {
                    if debug > 1 {
                        println!("save_restore:get_channel_values: {pv} not connected");
                    }
                    SaveEntry {
                        pv_name: pv,
                        value: String::new(),
                        connected: false,
                    }
                }
            })
            .collect();
        if debug > 1 {
            for e in entries.iter().filter(|e| !e.connected) {
                println!(
                    "save_restore:get_channel_values: invalid channel {}",
                    e.pv_name
                );
            }
        }
        entries
    }

    /// C `write_save_file(plist, configName, ...)` with a `configName`:
    /// this set's values written to `path` alone, with no backups. The
    /// list's status, save time and SR_recentlyStr are the save's, as for
    /// any `write_save_file` (save_restore.c:2012, :2100-2110, :2127-2134).
    /// `entries` are the values [`Self::collect_entries`] fetched.
    pub async fn save_to(
        &self,
        entries: Vec<SaveEntry>,
        path: &std::path::Path,
    ) -> AutosaveResult<()> {
        let missing = entries.iter().filter(|e| !e.connected).count();
        *self.list_status.lock().unwrap() = ListStatus::new(SrStatus::Ok, "Ok");
        request::concatenated(&path.to_string_lossy(), &self.settings);
        if self.settings.debug() > 2 {
            println!("write_save_file: saving to {}", path.display());
        }
        if let Err(e) = write_save_file_with_settings(path, &entries, &self.settings).await {
            return Err(self.record_write_failure(e).await);
        }
        *self.save_time.lock().unwrap() = chrono::Local::now();
        *self.last_save_file.lock().unwrap() = self.save_file();
        if missing > 0 {
            *self.list_status.lock().unwrap() = not_saved_status(missing);
        }
        self.note_recent(format!("Wrote '{}'", self.save_file()));
        Ok(())
    }

    /// Perform one save cycle: collect PV values -> guarantee `.savB` ->
    /// write `.sav` -> advance `.savB`.
    pub async fn save_once(&self, db: &PvDatabase) -> AutosaveResult<usize> {
        self.write_entries(self.collect_entries(db)).await
    }

    /// [`Self::save_once`] of values already fetched.
    pub async fn write_entries(&self, save_entries: Vec<SaveEntry>) -> AutosaveResult<usize> {
        {
            let mut s = self.status.write().await;
            *s = SaveSetStatus::Saving;
        }

        // The values are read before the rotation because the `.savB` that
        // C writes when the old one is unusable is written from the values
        // about to be saved, not copied from a file.
        let saved_count = save_entries.iter().filter(|e| e.connected).count();
        request::concatenated(&self.save_path().to_string_lossy(), &self.settings);

        // Rotate backups. A rotation that could not preserve the current
        // `.sav`, or could not put a usable `.savB` in place first, aborts
        // the cycle instead of overwriting the generation it failed to
        // copy, and it lands in the same bookkeeping a failed write does —
        // an early `?` here left the set stuck in `Saving` with the error
        // counter untouched.
        let rotated = rotate_backups(
            &self.save_path(),
            &self.config.backup,
            &save_entries,
            &self.settings,
        )
        .await;
        let savb_state = match rotated {
            Ok(state) => state,
            Err(e) => {
                let status = ListStatus::new(SrStatus::Fail, "Can't write .savB file");
                return Err(self.record_save_failure(e, status).await);
            }
        };

        if self.settings.debug() > 2 {
            println!("write_save_file: saving to {}", self.save_path().display());
        }
        match write_save_file_with_settings(&self.save_path(), &save_entries, &self.settings).await
        {
            Ok(()) => {
                self.stats.save_count.fetch_add(1, Ordering::Relaxed);
                *self.last_save_file.lock().unwrap() = self.save_file();
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
                if let Err(e) = publish_savb(&self.save_path(), savb_state, &self.settings).await {
                    println!(
                        "save_restore:write_save_file - Couldn't make backup '{}B' [{}]",
                        self.save_path().display(),
                        backup::date_str()
                    );
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
                    not_saved_status(not_saved)
                } else if savb_state == SavbState::Rewritten {
                    ListStatus::new(SrStatus::Warn, ".savB file was bad")
                } else {
                    ListStatus::new(SrStatus::Ok, "Ok")
                };
                self.note_recent(format!("Wrote '{}'", self.save_file()));

                Ok(saved_count)
            }
            Err(e) => Err(self.record_write_failure(e).await),
        }
    }

    /// Whether the save task's `do_seq` is due (save_restore.c:1008-1012):
    /// a save has written its file, and the last sequence file is older than
    /// `seq_period`, which C holds to 10 s at least (:905).
    pub(super) fn seq_due(&self, now: std::time::Instant) -> bool {
        self.settings.seq_files() > 0
            && !self.last_save_file.lock().unwrap().is_empty()
            && now.duration_since(self.seq.lock().unwrap().backup_time) > self.settings.seq_period()
    }

    /// C `doPeriodicDatedBackup` (save_restore.c:2218-2262): write
    /// `<save_file>_b_<yymmdd-hhmmss>` from the PVs.
    pub async fn do_periodic_dated_backup(&self, db: &PvDatabase) {
        let debug = self.settings.debug();
        if debug > 1 {
            println!("save_restore:doPeriodicDatedBackup: entry");
        }
        request::concatenated(&self.save_path().to_string_lossy(), &self.settings);
        let mut name = self.save_path().as_os_str().to_os_string();
        name.push(format!("_b_{}", backup::date_str()));
        let path = std::path::PathBuf::from(name);
        if debug > 1 {
            println!(
                "save_restore:doPeriodicDatedBackup: filename is '{}'",
                path.display()
            );
        }
        let entries = self.collect_entries(db);
        if write_save_file_with_settings(&path, &entries, &self.settings)
            .await
            .is_err()
        {
            backup::print_banner(
                17,
                &format!(
                    "save_restore:doPeriodicDatedBackup: Can't write file. [{}]",
                    path.display()
                ),
            );
            *self.list_status.lock().unwrap() =
                ListStatus::new(SrStatus::Fail, "Can't write dated backup file");
        }
    }

    /// C `do_seq` (save_restore.c:2146-2215): copy the `.sav` just written
    /// into the next sequence file, or write that file from the PVs when
    /// the copy fails.
    pub async fn do_seq(&self, db: &PvDatabase) {
        let datetime = backup::date_str();
        let sav = &self.save_path();
        request::concatenated(&sav.to_string_lossy(), &self.settings);
        let slot = self.seq.lock().unwrap().slot;
        let slot = match slot {
            Some(slot) => slot,
            None => backup::oldest_seq_slot(sav, self.settings.seq_files()).await,
        };
        self.seq.lock().unwrap().slot = Some(slot);

        if backup::check_file(sav, &self.settings).await == backup::FileState::None {
            println!(
                "save_restore:do_seq - '{}' not found.  Writing a new one. [{datetime}]",
                sav.display()
            );
            if self.save_once(db).await.is_err() {
                println!(
                    "save_restore:do_seq - write_save_file() failed for '{}' [{datetime}]",
                    sav.display()
                );
                *self.list_status.lock().unwrap() =
                    ListStatus::new(SrStatus::SeqWarn, "Can't write sequence file");
            }
        }
        let path = sav.with_extension(format!("sav{slot}"));
        let name = format!("{}{slot}", self.save_file());
        if publish_copy(sav, &path, &self.settings).await.is_err() {
            println!(
                "save_restore:do_seq - Can't copy save file to '{}' [{datetime}]",
                path.display()
            );
            let entries = self.collect_entries(db);
            if write_save_file_with_settings(&path, &entries, &self.settings)
                .await
                .is_err()
            {
                println!("save_restore:do_seq - Can't write seq. file from PV list. [{datetime}]");
                let mut status = self.list_status.lock().unwrap();
                if status.code >= SrStatus::Warn {
                    *status = ListStatus::new(SrStatus::SeqWarn, "Can't write sequence file");
                }
                drop(status);
                self.note_recent(format!("Can't write '{name}'"));
                return;
            }
            println!("save_restore:do_seq: Wrote seq. file from PV list. [{datetime}]");
        }
        self.note_recent(format!("Wrote '{name}'"));
        let mut seq = self.seq.lock().unwrap();
        seq.backup_time = std::time::Instant::now();
        // C's form, against the count as it is now: the `%` this replaced
        // divided by a count set to 0 since `do_seq` began.
        let next = slot + 1;
        seq.slot = Some(if next >= self.settings.seq_files() {
            0
        } else {
            next
        });
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

    /// The `.sav` file this set writes.
    pub fn save_path(&self) -> PathBuf {
        self.save_path.lock().unwrap().clone()
    }

    /// C `set_savefile_name`'s `strNcpy(plist->save_file, save_filename,
    /// FN_LEN)` (save_restore.c:2280).
    pub fn set_save_file(&self, save_filename: &str) {
        let mut end = save_filename.len().min(79);
        while !save_filename.is_char_boundary(end) {
            end -= 1;
        }
        let mut path = self.save_path.lock().unwrap();
        *path = path.with_file_name(&save_filename[..end]);
    }

    /// C's `plist->save_file`, the name SR_recentlyStr and `SR_<i>_Name` use.
    pub fn save_file(&self) -> String {
        self.save_path()
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

    /// C `write_save_file`'s failed `write_it` (save_restore.c:2097-2105).
    async fn record_write_failure(&self, e: AutosaveError) -> AutosaveError {
        backup::print_banner(
            17,
            &format!(
                "save_restore:write_save_file: Can't write save file. [{}]",
                backup::date_str()
            ),
        );
        self.note_recent(format!("Can't write '{}'", self.save_file()));
        self.record_save_failure(e, ListStatus::new(SrStatus::Fail, "Can't write .sav file"))
            .await
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
        let source = fopen_and_check(&self.save_path(), &self.settings)
            .await
            .map(|(path, _)| path)
            .ok_or_else(|| AutosaveError::CorruptSaveFile {
                path: self.save_path().display().to_string(),
                message: "no valid save file found".to_string(),
            })?;

        restore_from_entries_with_mode(db, &source, mode, &self.settings).await
    }

    /// Reload request file.
    pub async fn reload_request(&mut self) -> AutosaveResult<()> {
        let (entries, status) = Self::load_entries(&self.config, &self.settings).await?;
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

    /// The list's `SR_<i>_` status PV slot, if it has one.
    pub fn status_pv_index(&self) -> Option<usize> {
        let i = self.status_pv_index.load(Ordering::Relaxed);
        (i < NUM_STATUS_PV_SETS).then_some(i)
    }

    pub(super) fn set_status_pv_index(&self, i: usize) {
        self.status_pv_index.store(i, Ordering::Relaxed);
    }

    /// C `plist->last_save_file`.
    pub fn last_save_file(&self) -> String {
        self.last_save_file.lock().unwrap().clone()
    }

    /// C `plist->backup_time`: the set's creation, then the last sequence
    /// file.
    pub fn backup_time(&self) -> chrono::DateTime<chrono::Local> {
        chrono::Local::now() - self.seq.lock().unwrap().backup_time.elapsed()
    }

    /// The save task's `save_state` and `not_connected` at the end of a pass.
    pub(super) fn publish_task_view(&self, save_state: u32, not_connected: usize) {
        *self.task_view.lock().unwrap() = (save_state, not_connected);
    }

    /// C `plist->save_state` and `plist->not_connected`, as of the last pass.
    pub fn task_view(&self) -> (u32, usize) {
        *self.task_view.lock().unwrap()
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

    /// The IOC's settings this set reads.
    pub fn settings(&self) -> &Arc<SrSettings> {
        &self.settings
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
    restore_from_entries_with_mode(
        db,
        path,
        RestoreMode::Boot(RestorePass::One),
        &SrSettings::default(),
    )
    .await
}

/// Best-effort restore from a save file with an explicit
/// [`RestoreMode`]. Each PV is independent — a failed put on one PV
/// does not abort the rest.
pub async fn restore_from_entries_with_mode(
    db: &PvDatabase,
    path: &std::path::Path,
    mode: RestoreMode,
    settings: &SrSettings,
) -> AutosaveResult<RestoreResult> {
    let contents = read_save_file(path)
        .await?
        .ok_or_else(|| AutosaveError::CorruptSaveFile {
            path: path.display().to_string(),
            message: "missing <END> marker".to_string(),
        })?;
    Ok(restore_contents(db, path, &contents, mode, settings, &mut Vec::new()).await)
}

/// What C `do_manual_restore` reads: `FROM_SAVE_FILE` (`fdbrestore`) or
/// `FROM_ASCII_FILE` (`fdbrestoreX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualSource {
    SaveFile,
    AsciiFile,
}

/// C `do_manual_restore(filename, ...)` from the file `path` it names
/// (save_restore.c:3270-3473): a save file through `fopen_and_check` with
/// its header line skipped, an ASCII file as it is; line by line, each
/// expanded with `macrostring` and written with `dbPutField` before the
/// next is read, and a save file copied to `<path>.bu`. C's status: -1 when
/// no file opens, else the errors.
pub async fn do_manual_restore(
    db: &PvDatabase,
    filename: &str,
    path: &std::path::Path,
    from: ManualSource,
    macrostring: &str,
    settings: &SrSettings,
) -> i32 {
    let debug = settings.debug();
    if debug >= 5 {
        println!("save_restore:do_manual_restore: entry for file '{filename}'");
    }
    if !std::path::Path::new(filename).is_absolute() {
        request::concatenated(&path.to_string_lossy(), settings);
    }
    let file = match from {
        ManualSource::SaveFile => fopen_and_check(path, settings).await.map(|(file, _)| file),
        ManualSource::AsciiFile => Some(path.to_path_buf()),
    };
    let text = match &file {
        Some(file) => crate::runtime::fs::read_to_string(file).await.ok(),
        None => None,
    };
    let (Some(file), Some(text)) = (file, text) else {
        crate::runtime::log::errlog_vprintf(format_args!(
            "../save_restore.c:do_manual_restore Can't open save file.\n"
        ));
        return -1;
    };
    let text = match from {
        ManualSource::SaveFile => text
            .split_once('\n')
            .map_or("", |(_, rest)| rest)
            .to_string(),
        ManualSource::AsciiFile => text,
    };
    let macros = MacroContext::parse_inline(macrostring);
    if !macrostring.is_empty() && debug >= 5 {
        println!("save_restore:do_manual_restore: Current macro definitions:");
        report_macros(macrostring);
        println!("save_restore:do_manual_restore: --------------------------");
    }
    let mut r = RestoreResult::of(&file);
    // C's `p_data`, kept for its trace only (save_restore.c:3466-3470).
    let mut p_data = Vec::new();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let line = expand_each_line(line.to_string(), &macros);
        let pv_name = line.split_ascii_whitespace().next().unwrap_or("");
        if pv_name.starts_with(super::format::END_MARKER) {
            break;
        }
        if debug >= 5 {
            println!("save_restore:do_manual_restore: PVname='{pv_name}'");
        }
        let contents = save_file::parse_save_content(&line);
        let one = restore_contents(
            db,
            &file,
            &contents,
            RestoreMode::Process,
            settings,
            &mut p_data,
        )
        .await;
        r.absorb(one, index);
        if r.aborted {
            break;
        }
    }
    if r.aborted {
        return -1;
    }
    if from == ManualSource::SaveFile {
        let mut bu = path.as_os_str().to_os_string();
        bu.push(".bu");
        let bu = std::path::PathBuf::from(bu);
        if publish_copy(path, &bu, settings).await.is_err() {
            println!(
                "save_restore:do_manual_restore: myFileCopy() failed for '{}' -> '{}'",
                path.display(),
                bu.display()
            );
        }
    }
    (r.failed_puts.len() + r.not_found.len() + r.parse_failed.len() + r.not_connected_at_save)
        as i32
}

/// C `reboot_restore`'s read of the file `fopen_and_check` chose
/// (dbrestore.c:839-1010): line by line, each expanded with the item's
/// `macrostring` and restored in `pass` before the next is read, so the
/// `save_restoreDebug` lines come in C's order.
pub async fn restore_boot_file(
    db: &PvDatabase,
    path: &std::path::Path,
    pass: RestorePass,
    macrostring: &str,
    settings: &SrSettings,
) -> AutosaveResult<RestoreResult> {
    let errlog = |line: std::fmt::Arguments<'_>| {
        crate::runtime::log::errlog_vprintf(format_args!("../dbrestore.c:reboot_restore {line}"))
    };
    let debug = settings.debug();
    let text = crate::runtime::fs::read_to_string(path).await?;
    let macros = MacroContext::parse_inline(macrostring);
    if !macrostring.is_empty() && debug >= 5 {
        errlog(format_args!("Current macro definitions:\n"));
        report_macros(macrostring);
        errlog(format_args!("--------------------------\n"));
    }
    if debug >= 1 {
        let header = text.split_inclusive('\n').next().unwrap_or("");
        errlog(format_args!("header line '{header}'\n"));
    }
    let mut result = RestoreResult::of(path);
    // C's `p_data`, kept for its trace only (`:1074-1078`).
    let mut p_data = Vec::new();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let line = if macros.is_empty() {
            line.to_string()
        } else {
            let expanded = expand_each_line(line.to_string(), &macros);
            if debug >= 5 {
                println!("reboot_restore: buffer='{line}'");
                println!("                         ebuffer='{expanded}'");
            }
            expanded
        };
        let framing = line.trim();
        if framing.is_empty() {
            if debug >= 10 {
                errlog(format_args!("line (fragment) '{line}' ignored.\n"));
            }
            continue;
        }
        if framing == super::format::END_MARKER {
            break;
        }
        let contents = save_file::parse_save_content(&line);
        let r = restore_contents(
            db,
            path,
            &contents,
            RestoreMode::Boot(pass),
            settings,
            &mut p_data,
        )
        .await;
        result.absorb(r, index);
        if result.aborted {
            break;
        }
    }
    Ok(result)
}

/// C `macReportMacros` (macCore.c:472-504) for `macrostring`'s definitions,
/// in the order they were installed.
pub(super) fn report_macros(macrostring: &str) {
    let row = |e: &str, name: &str, raw: &str, value: &str| {
        println!("{e:<1} {name:<16} {raw:<16} {value}");
    };
    row("e", "name", "rawval", "value");
    row("-", "----", "------", "-----");
    let mut seen: Vec<(String, String)> = Vec::new();
    for (name, value) in crate::server::iocsh::macro_defn_pairs(macrostring) {
        let Some(value) = value.filter(|_| !name.is_empty()) else {
            continue;
        };
        match seen.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => seen.push((name, value)),
        }
    }
    for (name, value) in &seen {
        row(" ", name, value, value);
    }
}

/// Line by line, as C's `fgets` + `macExpandString`: an unterminated
/// reference must not swallow the lines after it. An undefined one stays
/// as text, and the put to the name it leaves fails and counts.
fn expand_each_line(text: String, macros: &HashMap<String, String>) -> String {
    if macros.is_empty() {
        return text;
    }
    text.split_inclusive('\n')
        .map(|line| crate::server::db_loader::expand_macros(line, macros, Default::default()).text)
        .collect()
}

/// Write every entry of a parsed file into the database; one failed put
/// does not stop the rest.
async fn restore_contents(
    db: &PvDatabase,
    path: &std::path::Path,
    contents: &save_file::SaveFileContents,
    mode: RestoreMode,
    settings: &SrSettings,
    p_data: &mut Vec<u8>,
) -> RestoreResult {
    let mut result = RestoreResult {
        not_connected_at_save: contents.not_connected,
        malformed_lines: contents.malformed.clone(),
        ..RestoreResult::of(path)
    };
    let boot = matches!(mode, RestoreMode::Boot(_));
    let debug = settings.debug();
    // C reads the `!` line in its place: it logs it, `reboot_restore` marks
    // the set WARN (`dbrestore.c:994-1002`, save_restore.c:3426-3429), and
    // either stops there when the site has cleared
    // `save_restoreIncompleteSetsOk`.
    let incomplete = |result: &mut RestoreResult| {
        let n = result.not_connected_at_save;
        if boot {
            crate::runtime::log::errlog_vprintf(format_args!(
                "../dbrestore.c:reboot_restore {n} {} had no saved value.\n",
                if n == 1 { "PV" } else { "PVs" }
            ));
        } else {
            println!(
                "save_restore:do_manual_restore: {n} PV{} had no saved value",
                if n == 1 { ' ' } else { 's' }
            );
        }
        if !settings.incomplete_sets_ok() {
            if boot {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "../dbrestore.c:reboot_restore aborting restore\n"
                ));
            } else {
                println!("save_restore:do_manual_restore: aborting restore");
            }
            result.aborted = true;
        }
    };

    for (index, entry) in contents.entries.iter().enumerate() {
        if contents.incomplete_at == Some(index) {
            incomplete(&mut result);
            if result.aborted {
                return result;
            }
        }
        if !entry.connected {
            result.disconnected_skipped.push(entry.pv_name.clone());
            continue;
        }
        // C reads `is_scalar` off the `@array@` marker, which the parse
        // turned into the `[..]` form, and then off the field (`:913-928`).
        let mut is_scalar = !entry.value.starts_with('[');
        let elements = db
            .restore_target(entry.pv_name.strip_suffix('$').unwrap_or(&entry.pv_name))
            .map_or(0, |t| t.elements);
        if !boot {
            if is_scalar && elements > 1 {
                if debug >= 5 {
                    println!(
                        "save_restore:do_manual_restore: PV '{}' is scalar in .sav file, but has {elements} elements.  Treating as array.",
                        entry.pv_name
                    );
                }
                is_scalar = false;
            }
            if debug >= 5 && entry.pv_name.ends_with('$') {
                println!(
                    "save_restore:do_manual_restore: PV '{}' is long string; value='{}'.",
                    entry.pv_name, entry.value
                );
            }
        }
        if boot {
            let pv = c_pv_name(&entry.pv_name);
            if debug > 9 {
                trace("reboot_restore", format_args!("\n"));
            }
            if is_scalar && elements > 1 {
                if debug >= 5 {
                    println!(
                        "reboot_restore: PV '{pv}' is scalar in .sav file, but has {elements} elements.  Treating as array."
                    );
                }
                is_scalar = false;
            }
            if debug >= 10 {
                let value = c_array_text(&entry.value).unwrap_or_else(|| entry.value.clone());
                trace(
                    "reboot_restore",
                    format_args!(
                        "Attempting to put {} '{value}' to '{pv}'\n",
                        if is_scalar { "scalar" } else { "array" }
                    ),
                );
            }
        }

        // Get current value to determine type
        let current = match db.get_pv(&entry.pv_name) {
            Ok(v) => v,
            Err(_) => {
                if boot {
                    let (record, _) = crate::server::database::parse_pv_name(
                        entry.pv_name.strip_suffix('$').unwrap_or(&entry.pv_name),
                    );
                    let pv = c_pv_name(&entry.pv_name);
                    if db.get_record(record).is_none() {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "../dbrestore.c:reboot_restore dbFindRecord for '{pv}' failed\n"
                        ));
                    } else {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "../dbrestore.c:reboot_restore dbFindRecord did not find field '{pv}'\n"
                        ));
                    }
                    // `SR_array_restore` gobbling the array (`:995-1000`).
                    if !is_scalar {
                        if debug >= 1 {
                            trace("SR_array_restore", format_args!(":entry: PV = '{pv}'\n"));
                        }
                        let value =
                            c_array_text(&entry.value).unwrap_or_else(|| entry.value.clone());
                        ArrayWalk::start(ArrayFn::Boot, &value, 0, true, debug)
                            .finish(&value, debug);
                        if debug >= 1 {
                            trace(
                                "SR_array_restore",
                                format_args!("Gobbled unused array data.\n"),
                            );
                        }
                    }
                } else if !entry.pv_name.ends_with('$')
                    && !entry.value.starts_with(super::format::ARRAY_MARKER)
                {
                    // `ca_pend_io` times the search out (save_restore.c:3367);
                    // a long string or an array is counted silently.
                    println!(
                        "save_restore:do_manual_restore: ca_search for {} timeout",
                        entry.pv_name
                    );
                } else if !is_scalar && !entry.pv_name.ends_with('$') {
                    // `manual_array_restore` gobbling the array (:3406-3410).
                    let manual = ArrayFn::Manual;
                    if debug >= 1 {
                        manual.line(format_args!(":entry: PV = '{}'\n", entry.pv_name));
                    }
                    let value = c_array_text(&entry.value).unwrap_or_else(|| entry.value.clone());
                    ArrayWalk::start(manual, &value, 0, true, debug).finish(&value, debug);
                    if debug >= 1 {
                        manual.line(format_args!("Gobbled unused array data.\n"));
                    }
                }
                result.not_found.push(entry.pv_name.clone());
                continue;
            }
        };

        let put_result = match mode {
            RestoreMode::Boot(pass) => boot_store(db, entry, &current, pass, debug, p_data).await,
            RestoreMode::Process => {
                let array = !is_scalar && !entry.pv_name.ends_with('$');
                manual_store(db, entry, &current, array, elements, debug, p_data).await
            }
        };
        match put_result {
            Ok(Stored::Yes) => {
                result.restored += 1;
            }
            Ok(Stored::Skipped) => {}
            Err(StoreError::Parse) => {
                if boot {
                    restore_failed(&entry.pv_name);
                }
                result.parse_failed.push(entry.pv_name.clone());
            }
            // `scalar_restore` named the value it refused.
            Err(StoreError::Scalar { status, .. }) => {
                restore_failed(&entry.pv_name);
                result.failed_puts.push(PvRestoreError {
                    pv_name: entry.pv_name.clone(),
                    error: status,
                });
            }
            Err(StoreError::Put(e)) if boot => {
                restore_failed(&entry.pv_name);
                result.failed_puts.push(PvRestoreError {
                    pv_name: entry.pv_name.clone(),
                    error: e.to_string(),
                });
            }
            Err(StoreError::Put(e)) => {
                // Named here, at the one site every restore route reaches:
                // the iocsh summary prints only a count.
                crate::runtime::log::errlog_vprintf(format_args!(
                    "autosave restore: {} put failed: {e}\n",
                    entry.pv_name
                ));
                result.failed_puts.push(PvRestoreError {
                    pv_name: entry.pv_name.clone(),
                    error: e.to_string(),
                });
            }
        }
    }
    if contents.incomplete_at == Some(contents.entries.len()) {
        incomplete(&mut result);
    }

    result
}

/// `do_manual_restore`'s `dbPutField` of `entry`, through
/// `manual_array_restore` (save_restore.c:2913-3194) for an `array`, whose
/// field has `elements`.
async fn manual_store(
    db: &PvDatabase,
    entry: &SaveEntry,
    current: &EpicsValue,
    array: bool,
    elements: u32,
    debug: i32,
    p_data: &mut Vec<u8>,
) -> Result<Stored, StoreError> {
    let parsed = if array {
        let manual = ArrayFn::Manual;
        let (field_type, field_size) = ca_array_type(current).unwrap_or((0, 0));
        manual.setup(&entry.pv_name, elements, field_size, debug, p_data);
        if debug >= 10 {
            manual.line(format_args!("...field_type is ({field_type})\n"));
        }
        let value = c_array_text(&entry.value).unwrap_or_else(|| entry.value.clone());
        let walk = ArrayWalk::start(manual, &value, elements, false, debug);
        let parsed = save_file::parse_save_value(&entry.value, current);
        if let Some(parsed) = parsed.as_ref().filter(|_| debug >= 10 && walk.begin) {
            let num_read = (parsed.count() as u32).min(elements) as usize;
            println!("\nsave_restore: {num_read} array values:");
            for value in ca_value_lines(parsed).into_iter().take(num_read) {
                println!("\t{value}");
            }
            println!("save_restore: end of {num_read} array values.\n");
            crate::runtime::task::sleep_background(Duration::from_millis(500)).await;
        }
        walk.finish(&value, debug);
        if parsed.is_some() && debug >= 1 {
            manual.line(format_args!("Writing array to database\n"));
        }
        parsed
    } else {
        save_file::parse_save_value(&entry.value, current)
    };
    let parsed = parsed.ok_or(StoreError::Parse)?;
    let (base, field) = crate::server::database::parse_pv_name(&entry.pv_name);
    db.put_record_field_from_ca_no_notify(base, field, parsed)
        .await
        .map(|()| Stored::Yes)
        .map_err(StoreError::Put)
}

/// `reboot_restore`'s name for a PV, `.VAL` added to a bare record name
/// (`dbrestore.c:912`).
fn c_pv_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.contains('.') {
        name.into()
    } else {
        format!("{name}.VAL").into()
    }
}

/// `reboot_restore`'s line for a store that returned a status (`:981-984`).
fn restore_failed(pv_name: &str) {
    crate::runtime::log::errlog_vprintf(format_args!(
        "../dbrestore.c:reboot_restore restore for PV '{}' failed\n",
        c_pv_name(pv_name)
    ));
}

/// Whether [`boot_store`] wrote the value or C's pass rules left it alone.
enum Stored {
    Yes,
    Skipped,
}

enum StoreError {
    Parse,
    Put(crate::error::CaError),
    /// `scalar_restore`'s refusal of `value`, with the status's
    /// `errSymLookup` text, empty for a status C has no symbol for.
    Scalar {
        value: String,
        status: String,
    },
}

/// One saved value into the database as C `reboot_restore` stores it
/// (`dbrestore.c:963-991`): an array, or a scalar by its declared type
/// (`scalar_restore`, `:222-327`).
async fn boot_store(
    db: &PvDatabase,
    entry: &SaveEntry,
    current: &EpicsValue,
    pass: RestorePass,
    debug: i32,
    p_data: &mut Vec<u8>,
) -> Result<Stored, StoreError> {
    use crate::types::DbfCode;
    let pv = c_pv_name(&entry.pv_name);

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
        let array = |line: std::fmt::Arguments<'_>| trace("SR_array_restore", line);
        // The field's type as `dbNameToAddr` gives it: a waveform's VAL is
        // FTVL's, not the declared DBF_NOACCESS.
        let (field_type, field_size) = array_type_of(current).unwrap_or((target.dbf, 0));
        ArrayFn::Boot.setup(&pv, target.elements, field_size, debug, p_data);
        if debug >= 10 {
            array(format_args!(
                "field_type is 'DBF_{}' ({})\n",
                field_type.name(),
                field_type as i16
            ));
        }
        let value = c_array_text(&entry.value).unwrap_or_else(|| entry.value.clone());
        let walk = ArrayWalk::start(ArrayFn::Boot, &value, target.elements, false, debug);
        let parsed = save_file::parse_save_value(&entry.value, current).ok_or(StoreError::Parse)?;
        let num_read = parsed.count().min(target.elements) as usize;
        // Only a value with its `{` is listed (`:537`, `:653`).
        if debug >= 10 && walk.begin {
            array(format_args!("\nsave_restore: {num_read} array values:\n"));
            for value in array_value_lines(&parsed).into_iter().take(num_read) {
                array(format_args!("\t{value}\n"));
            }
            array(format_args!("end of {num_read} array values.\n\n"));
            crate::runtime::task::sleep_background(Duration::from_millis(500)).await;
        }
        walk.finish(&value, debug);
        if pass == RestorePass::Zero {
            if debug >= 1 {
                array(format_args!("No array write in pass 0.\n"));
            }
            return Ok(Stored::Skipped);
        }
        if debug >= 1 {
            array(format_args!("Writing array to database\n"));
        }
        if debug >= 5 {
            trace(
                "SR_put_array_values",
                format_args!(
                    "restoring {num_read} values to {pv} (max_elements={})\n",
                    target.elements
                ),
            );
        }
        return put(db.put_pv_array_static(&entry.pv_name, parsed));
    }
    if debug >= 5 {
        trace(
            "scalar_restore",
            format_args!("entry:field type 'DBF_{}'\n", target.dbf.name()),
        );
    }
    // `value_string` after `epicsStrnRawFromEscaped` (`:235`).
    let text = match save_file::parse_save_value(&entry.value, &EpicsValue::String("".into())) {
        Some(EpicsValue::String(s)) => s.to_string(),
        _ => entry.value.clone(),
    };
    let scalar = |value: &str, r: crate::error::CaResult<()>| {
        r.map(|()| Stored::Yes).map_err(|e| StoreError::Scalar {
            value: value.to_string(),
            status: match e {
                crate::error::CaError::InvalidValue(status) => status,
                e => e.to_string(),
            },
        })
    };
    // The `>= 15` report of a put's status (`:263-266`, `:276-279`,
    // `:288-291`), the `errMessage` at `line`. rs has the status's
    // `errSymLookup` text, not its number, so a refusal prints the text.
    let traced = |what: &str, line: u32, r: Result<Stored, StoreError>| {
        if debug >= 15 {
            let status = match &r {
                Err(StoreError::Scalar { status, .. }) => status.as_str(),
                _ => "0",
            };
            trace(
                "scalar_restore",
                format_args!("{what}() returns: {status}\n"),
            );
            let name = if status == "0" { "" } else { status };
            let gap = if name.is_empty() { "" } else { " " };
            crate::runtime::log::errlog_vprintf(format_args!(
                "{name}{gap}filename=\"../dbrestore.c\" line number={line}   \n"
            ));
        }
        r
    };
    let stored = match target.dbf {
        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink => match pass {
            RestorePass::Zero => traced(
                "dbPutString",
                278,
                scalar(&text, db.put_pv_static(&entry.pv_name, &text).await),
            ),
            RestorePass::One => {
                if debug > 1 {
                    trace(
                        "scalar_restore",
                        format_args!("Can't restore link field ({pv}) in pass 1.\n"),
                    );
                }
                Ok(Stored::Skipped)
            }
        },
        // `dbPutMenuIndex(pdbentry, atol(value_string))`.
        DbfCode::Menu => traced(
            "dbPutMenuIndex",
            290,
            scalar(
                &text,
                db.put_pv_menu_index(&entry.pv_name, c_atol(&text) as i32)
                    .await,
            ),
        ),
        DbfCode::NoAccess => match pass {
            RestorePass::Zero => {
                if debug > 1 {
                    trace(
                        "scalar_restore",
                        format_args!("Can't restore DBF_NOACCESS field ({pv}) in pass 0.\n"),
                    );
                }
                Ok(Stored::Skipped)
            }
            RestorePass::One => {
                let parsed =
                    save_file::parse_save_value(&entry.value, current).ok_or(StoreError::Parse)?;
                scalar(&text, db.put_pv_no_process(&entry.pv_name, parsed).await)
            }
        },
        // The switch has no arm for it (`:306-311`): status -1.
        DbfCode::Device => {
            if debug >= 1 {
                trace(
                    "scalar_restore",
                    format_args!("field_type '{}' not handled\n", DbfCode::Device as i16),
                );
            }
            Err(StoreError::Scalar {
                value: text,
                status: String::new(),
            })
        }
        // `SPC_CALC` fields may not be empty (`:258-260`).
        DbfCode::String if target.calc && text.is_empty() => traced(
            "dbPutString",
            265,
            scalar("0", db.put_pv_static(&entry.pv_name, "0").await),
        ),
        _ => traced(
            "dbPutString",
            265,
            scalar(&text, db.put_pv_static(&entry.pv_name, &text).await),
        ),
    };
    if let Err(StoreError::Scalar { value, status }) = &stored {
        // `errMessage(status, " ")`: `errPrintf`'s own line, with no symbol
        // for a status that is not positive.
        crate::runtime::log::errlog_vprintf(format_args!(
            "../dbrestore.c:scalar_restore restore of '{value}' for '{pv}' failed\n\
             {status} filename=\"../dbrestore.c\" line number=318   \n"
        ));
    }
    if debug >= 15 {
        let text = db_get_string(db, name.unwrap_or(&entry.pv_name), pass);
        trace(
            "scalar_restore",
            format_args!("dbGetString() returns '{text}'\n"),
        );
    }
    stored
}

/// C `dbGetString(pdbentry)` for the field `name` (dbrestore.c:320-322): a
/// link reads back as it was put until iocInit parses it between the
/// passes, then as dbStatic renders it.
fn db_get_string(db: &PvDatabase, name: &str, pass: RestorePass) -> String {
    let (base, field) = crate::server::database::parse_pv_name(name);
    let Some(rec) = db.get_record(base) else {
        return String::new();
    };
    let (field, _) = rec.put_name(field);
    let inst = rec.read();
    let Some(desc) = inst.field_desc(&field) else {
        return String::new();
    };
    if pass == RestorePass::Zero
        && crate::types::link_class_of(desc).is_some()
        && let Some(EpicsValue::String(raw)) = inst.resolve_field_stored(&field)
    {
        return raw.as_str_lossy().into_owned();
    }
    inst.resolve_field(&field)
        .map(|v| {
            String::from_utf8_lossy(&crate::server::iocsh::db_get_string(&inst, desc, &v))
                .into_owned()
        })
        .unwrap_or_default()
}

/// `ERRLOG` (log_macros.h:6) from dbrestore.c's `func`.
fn trace(func: &str, line: std::fmt::Arguments<'_>) {
    crate::runtime::log::errlog_vprintf(format_args!("../dbrestore.c:{func} {line}"));
}

/// The C function an array walk traces as: dbrestore.c's
/// `SR_array_restore` (ERRLOG) or save_restore.c's `manual_array_restore`
/// (printf), whose texts differ only in the element-search line.
#[derive(Clone, Copy)]
enum ArrayFn {
    Boot,
    Manual,
}

impl ArrayFn {
    fn line(self, line: std::fmt::Arguments<'_>) {
        match self {
            Self::Boot => trace("SR_array_restore", line),
            Self::Manual => print!("save_restore:manual_array_restore: {line}"),
        }
    }

    /// `:entry:` and the `p_data` bookkeeping of a non-gobble walk
    /// (`dbrestore.c:430-457`, save_restore.c:2927-2952); `p_data` is kept
    /// for the trace only.
    fn setup(self, pv: &str, elements: u32, field_size: usize, debug: i32, p_data: &mut Vec<u8>) {
        if debug >= 1 {
            self.line(format_args!(":entry: PV = '{pv}'\n"));
            let need = elements as usize * field_size;
            if p_data.is_empty() || need > p_data.len() {
                let at = |p: &Vec<u8>| {
                    if p.is_empty() {
                        "(nil)".to_string()
                    } else {
                        format!("{:p}", p.as_ptr())
                    }
                };
                self.line(format_args!(
                    "p_data = {}, p_data_size = {}\n",
                    at(p_data),
                    p_data.len()
                ));
                *p_data = vec![0; need];
                if debug >= 10 {
                    self.line(format_args!(
                        "allocated p_data = {}, p_data_size = {}\n",
                        at(p_data),
                        p_data.len()
                    ));
                }
            }
        }
        if debug >= 10 {
            self.line(format_args!(
                "Looking for up to {elements} elements of field-size {field_size}\n"
            ));
        }
    }
}

/// Where C's walk of an `@array@` text stopped (`SR_array_restore`,
/// `:477-713`, and `manual_array_restore`). rs parses the value with
/// `parse_save_value`; the walk is kept for C's trace of it, over the one
/// line rs holds, so it ends where C would read the next line.
struct ArrayWalk {
    func: ArrayFn,
    begin: bool,
    end: bool,
    eof: bool,
    at: usize,
}

impl ArrayWalk {
    /// The walk up to `SR_array_restore`'s listing (`:477-651`); `max` is
    /// the field's elements, and a `gobble` walk stores none.
    fn start(func: ArrayFn, value: &str, max: u32, gobble: bool, debug: i32) -> Self {
        let t = |line: std::fmt::Arguments<'_>| func.line(line);
        let b = value.as_bytes();
        let at = |i: usize| b.get(i).copied().unwrap_or(0);
        let rest = |i: usize| String::from_utf8_lossy(b.get(i..).unwrap_or_default());
        let mut walk = Self {
            func,
            begin: false,
            end: false,
            eof: false,
            at: 0,
        };
        if debug >= 11 {
            t(format_args!("parsing buffer '{value}'\n"));
        }
        if b.is_empty() {
            if debug >= 11 {
                t(format_args!("value_string is null or empty\n"));
            }
            return walk;
        }
        let Some(mut i) = b.iter().position(|&c| c == b'{') else {
            if debug >= 11 {
                t(format_args!("ARRAY_BEGIN not found\n"));
            }
            return walk;
        };
        walk.begin = true;
        if debug >= 10 {
            t(format_args!("parsing array buffer '{}'\n", rest(i)));
        }
        let mut num_read = 0;
        while !walk.end && !walk.eof {
            if debug >= 10 {
                t(format_args!("looking for element[{num_read}] \n"));
            }
            while !walk.end && !walk.eof && at(i) != b'"' {
                if debug >= 12 {
                    let dots = if matches!(func, ArrayFn::Manual) {
                        "..."
                    } else {
                        ""
                    };
                    t(format_args!("{dots}buffer contains '{}'\n", rest(i)));
                }
                match at(i) {
                    0 => walk.eof = true,
                    b'}' => walk.end = true,
                    _ => i += 1,
                }
            }
            if walk.end || walk.eof {
                break;
            }
            if debug >= 11 {
                t(format_args!(
                    "Found element-begin; buffer contains '{}'\n",
                    rest(i)
                ));
            }
            i += 1;
            let mut element = Vec::new();
            while element.len() < 39 && at(i) != b'"' {
                if debug >= 11 {
                    t(format_args!("*bp={} ({})\n", at(i) as char, at(i) as i8));
                }
                if at(i) == 0 {
                    walk.eof = true;
                    break;
                }
                if at(i) == b'\\' && matches!(at(i + 1), b'"' | b'\\') {
                    i += 1;
                }
                if (0x20..0x7f).contains(&at(i)) {
                    element.push(at(i));
                }
                i += 1;
            }
            if debug >= 10 {
                t(format_args!(
                    "element[{num_read}] value = '{}'\n",
                    String::from_utf8_lossy(&element)
                ));
                t(format_args!(
                    "look for element-end: buffer contains '{}'\n",
                    rest(i)
                ));
            }
            let mut found = false;
            while !found && !walk.eof {
                while !matches!(at(i), 0 | b'"' | b'\\') {
                    i += 1;
                }
                match at(i) {
                    b'"' => {
                        found = true;
                        i += 1;
                    }
                    b'\\' => {
                        i += 1;
                        if matches!(at(i), b'"' | b'\\') {
                            i += 1;
                        }
                    }
                    _ => walk.eof = true,
                }
            }
            if num_read < max && !gobble {
                num_read += 1;
            }
        }
        walk.at = i;
        walk
    }

    /// The `ARRAY_END` search after the listing (`:683-714`).
    fn finish(self, value: &str, debug: i32) {
        let t = |line: std::fmt::Arguments<'_>| self.func.line(line);
        if !self.begin {
            if debug >= 10 {
                t(format_args!("ARRAY_BEGIN wasn't found.\n"));
            }
            return;
        }
        if debug >= 10 {
            t(format_args!("looking for ARRAY_END\n"));
        }
        let b = value.as_bytes();
        let at = |i: usize| b.get(i).copied().unwrap_or(0);
        let (mut i, mut end, mut in_element) = (self.at, self.end, false);
        while !end && !self.eof {
            if debug >= 11 {
                t(format_args!(
                    "...buffer contains '{}'\n",
                    String::from_utf8_lossy(b.get(i..).unwrap_or_default())
                ));
            }
            match at(i) {
                b'\\' if in_element && at(i + 1) == b'"' => i += 1,
                b'}' => {
                    if debug >= 10 {
                        t(format_args!(
                            "found ARRAY_END.  in_element={}\n",
                            i32::from(in_element)
                        ));
                    }
                    end = !in_element;
                }
                0 => break,
                b'"' => in_element = !in_element,
                _ => {}
            }
            i += 1;
        }
    }
}

/// The `dbNameToAddr` type and element size of an array `value`.
fn array_type_of(value: &EpicsValue) -> Option<(crate::types::DbfCode, usize)> {
    use crate::types::DbfCode;
    Some(match value {
        EpicsValue::StringArray(_) => (DbfCode::String, 40),
        EpicsValue::CharArray(_) => (DbfCode::Char, 1),
        EpicsValue::UCharArray(_) => (DbfCode::UChar, 1),
        EpicsValue::ShortArray(_) => (DbfCode::Short, 2),
        EpicsValue::UShortArray(_) => (DbfCode::UShort, 2),
        EpicsValue::EnumArray(_) => (DbfCode::Enum, 2),
        EpicsValue::LongArray(_) => (DbfCode::Long, 4),
        EpicsValue::ULongArray(_) => (DbfCode::ULong, 4),
        EpicsValue::Int64Array(_) => (DbfCode::Int64, 8),
        EpicsValue::UInt64Array(_) => (DbfCode::UInt64, 8),
        EpicsValue::FloatArray(_) => (DbfCode::Float, 4),
        EpicsValue::DoubleArray(_) => (DbfCode::Double, 8),
        _ => return None,
    })
}

/// The CA type `ca_field_type` gives an array `value` and its `dbr_size`
/// (`dbDBRnewToDBRold`).
fn ca_array_type(value: &EpicsValue) -> Option<(i16, usize)> {
    Some(match value {
        EpicsValue::StringArray(_) => (0, 40),
        EpicsValue::ShortArray(_) => (1, 2),
        EpicsValue::FloatArray(_) => (2, 4),
        EpicsValue::EnumArray(_) => (3, 2),
        EpicsValue::CharArray(_) | EpicsValue::UCharArray(_) => (4, 1),
        EpicsValue::UShortArray(_) | EpicsValue::LongArray(_) => (5, 4),
        EpicsValue::ULongArray(_)
        | EpicsValue::Int64Array(_)
        | EpicsValue::UInt64Array(_)
        | EpicsValue::DoubleArray(_) => (6, 8),
        _ => return None,
    })
}

/// `manual_array_restore`'s listing (save_restore.c:3115-3133), of the
/// values as their CA type holds them.
fn ca_value_lines(value: &EpicsValue) -> Vec<String> {
    use crate::calc::engine::cvt::fmt_g;
    let g = |v: f64| fmt_g(v, 6, false, false);
    match value {
        EpicsValue::UCharArray(a) => a
            .iter()
            .map(|&c| format!("'{}' ({})", c as char, c as i8))
            .collect(),
        EpicsValue::UShortArray(a) => a.iter().map(|v| v.to_string()).collect(),
        EpicsValue::ULongArray(a) => a.iter().map(|&v| g(f64::from(v))).collect(),
        EpicsValue::Int64Array(a) => a.iter().map(|&v| g(v as f64)).collect(),
        EpicsValue::UInt64Array(a) => a.iter().map(|&v| g(v as f64)).collect(),
        _ => array_value_lines(value),
    }
}

/// `SR_array_restore`'s listing of the values it read (`:653-679`).
fn array_value_lines(value: &EpicsValue) -> Vec<String> {
    use crate::calc::engine::cvt::fmt_g;
    fn each<T: Copy>(a: &[T], f: impl Fn(T) -> String) -> Vec<String> {
        a.iter().map(|&v| f(v)).collect()
    }
    match value {
        EpicsValue::StringArray(a) => a
            .iter()
            .map(|s| format!("'{}'", s.as_str_lossy()))
            .collect(),
        EpicsValue::CharArray(a) => each(a, |c| format!("'{}' ({})", c as char, c as i8)),
        EpicsValue::UCharArray(a) => each(a, |c| format!("'{}' ({c})", c as char)),
        EpicsValue::ShortArray(a) => each(a, |v| v.to_string()),
        EpicsValue::UShortArray(a) | EpicsValue::EnumArray(a) => each(a, |v| v.to_string()),
        EpicsValue::LongArray(a) => each(a, |v| v.to_string()),
        EpicsValue::ULongArray(a) => each(a, |v| v.to_string()),
        EpicsValue::Int64Array(a) => each(a, |v| v.to_string()),
        EpicsValue::UInt64Array(a) => each(a, |v| v.to_string()),
        EpicsValue::FloatArray(a) => each(a, |v| format!("{:.6}", f64::from(v))),
        EpicsValue::DoubleArray(a) => each(a, |v| fmt_g(v, 6, false, false)),
        _ => Vec::new(),
    }
}

/// The `@array@ { "e" .. }` text C read for an array the parse holds in
/// its `[..]` form.
fn c_array_text(value: &str) -> Option<String> {
    if !value.starts_with('[') {
        return None;
    }
    let elements: Vec<String> = save_file::decode_array_text(value)
        .iter()
        .map(|e| format!("\"{}\"", e.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    Some(format!(
        "{} {{ {} }}",
        super::format::ARRAY_MARKER,
        elements.join(" ")
    ))
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
