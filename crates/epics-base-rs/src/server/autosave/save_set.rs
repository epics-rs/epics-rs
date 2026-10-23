use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::server::database::PvDatabase;
use crate::server::db_loader::MacroDefs;
use crate::types::EpicsValue;
use epics_libcom_rs::cfmt::CFloat;
use tokio::sync::RwLock;

use super::backup::{
    self, BackupConfig, SavbState, fopen_and_check, publish_copy, publish_savb, rotate_backups,
};
use super::error::{AutosaveError, AutosaveResult};
use super::macros::MacroContext;
use super::request::{self, RequestEntry};
use super::save_file::{
    self, ArrayRestore, MalformedLine, SaveEntry, write_save_file_with_settings,
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
    /// C `create_data_set`'s `macrostring`, `None` for iocsh's NULL: the
    /// `readReqFile` trace prints that `NULL`, `save_restoreShow` either as
    /// `''`. `macros` is what the request file expands with.
    pub macro_string: Option<String>,
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
    file_macros: FileMacros,
    status: RwLock<SaveSetStatus>,
    stats: SaveSetStats,
    seq: std::sync::Mutex<SeqState>,
    /// C `last_save_file`: the file the last save, configMenu's included,
    /// wrote (save_restore.c:2110), empty before any.
    last_save_file: std::sync::Mutex<String>,
    /// C `save_state` as the save task left it at the end of the list's
    /// last pass, which is what C's `save_restoreShow` sees under the list
    /// lock.
    task_view: std::sync::Mutex<u32>,
    /// C `plist->not_connected`: -1 until the first fetch
    /// (save_restore.c:2440); only [`Self::get_channel_values`] writes it.
    not_connected: std::sync::atomic::AtomicI32,
    /// C's `pchannel->value` and `pArray`: the last fetch, only
    /// [`Self::get_channel_values`] writes it.
    fetched: std::sync::Mutex<Vec<Fetched>>,
    /// C's `pchan_list`, one per member in [`Self::pv_names`]' order: only
    /// [`Self::channels_created`] and [`Self::get_channel_values`] write it.
    channels: std::sync::Mutex<Vec<Channel>>,
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
        // save_restore.c:2457, ahead of `readReqFile`.
        request::concatenated(&config.save_path.to_string_lossy(), &settings);
        let (entries, list_status, file_macros) = Self::load_entries(&config, &settings).await?;
        let created_at = std::time::Instant::now();
        let channels = request::pv_names(&entries)
            .iter()
            .map(|_| Channel::default())
            .collect();
        Ok(Self {
            save_path: std::sync::Mutex::new(config.save_path.clone()),
            config,
            entries,
            file_macros,
            status: RwLock::new(SaveSetStatus::Idle),
            stats: SaveSetStats::default(),
            seq: std::sync::Mutex::new(SeqState {
                slot: None,
                backup_time: created_at,
            }),
            last_save_file: std::sync::Mutex::new(String::new()),
            task_view: std::sync::Mutex::new(0),
            not_connected: std::sync::atomic::AtomicI32::new(-1),
            fetched: std::sync::Mutex::new(Vec::new()),
            channels: std::sync::Mutex::new(channels),
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
    ) -> AutosaveResult<(Vec<RequestEntry>, ListStatus, FileMacros)> {
        // C installs the command's definitions in its order; the builder's
        // own macros, which C has none of, follow them.
        let mut defs = MacroContext::parse_defs(config.macro_string.as_deref().unwrap_or_default());
        for (name, value) in MacroDefs::from(&config.macros).iter() {
            defs.put(name, value);
        }
        let handle = !defs.is_empty()
            || config
                .macro_string
                .as_deref()
                .is_some_and(|s| !s.is_empty());
        let macros = MacroContext::from_defs(defs, handle);
        let mut entries = Vec::new();
        let mut code = SrStatus::Init;
        let mut file_macros = FileMacros::default();

        if let Some(ref req_file) = config.request_file {
            let req_entries = request::read_request_list(
                &req_file.to_string_lossy(),
                &config.search_paths,
                &macros,
                config.macro_string.as_deref(),
                settings,
            )
            .await?;
            if req_entries.include_unopened {
                code = SrStatus::Fail;
            }
            entries.extend(req_entries.entries);
            file_macros = req_entries.file_macros;
        } else {
            file_macros.take_from(&macros);
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

        // C `readReqFile` appends every PV line, a repeated one included
        // (save_restore.c:3650-3672).
        Ok((
            entries,
            ListStatus::new(code, "Ready to connect..."),
            file_macros,
        ))
    }

    /// Every member's current value in this set's file encoding, an
    /// unreachable member and each in `searching` as `connected: false`,
    /// each read value kept beside its text.
    fn fetch(&self, db: &PvDatabase, searching: &[usize]) -> Vec<Fetched> {
        let debug = self.settings.debug();
        let not_connected = |pv: String| {
            if debug > 1 {
                crate::epics_println!("save_restore:get_channel_values: {pv} not connected");
            }
            Fetched {
                entry: SaveEntry {
                    pv_name: pv,
                    value: String::new(),
                    connected: false,
                    array_fprintfs: Vec::new(),
                },
                value: None,
            }
        };
        let entries: Vec<Fetched> = self
            .pv_names()
            .into_iter()
            .enumerate()
            .map(|(i, pv)| match super::manager::ca_get(db, &pv) {
                _ if searching.contains(&i) => not_connected(pv),
                // C `write_it`'s long-string member (save_restore.c:1844).
                _ if pv.ends_with('$') => match long_string_text(db, &pv)
                    .filter(|_| super::manager::ca_connects(db))
                {
                    Some(text) => {
                        if debug >= 15 {
                            crate::epics_println!(
                                "save_restore:get_channel_values: '{pv}' currently has 1 elements"
                            );
                        }
                        Fetched {
                            entry: SaveEntry {
                                value: epics_libcom_rs::runtime::epics_string::print_escaped(&text),
                                pv_name: pv,
                                connected: true,
                                array_fprintfs: Vec::new(),
                            },
                            value: None,
                        }
                    }
                    None => not_connected(pv),
                },
                Ok(val) => {
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
                        crate::epics_println!(
                            "save_restore:get_channel_values: '{pv}' currently has {} elements",
                            val.count()
                        );
                    }
                    let (value, array_fprintfs) = save_file::save_text(&val);
                    Fetched {
                        entry: SaveEntry {
                            value,
                            pv_name: pv,
                            connected: true,
                            array_fprintfs,
                        },
                        value: Some(val),
                    }
                }
                Err(_) => not_connected(pv),
            })
            .collect();
        if debug > 1 {
            for e in entries.iter().filter(|f| !f.entry.connected) {
                crate::epics_println!(
                    "save_restore:get_channel_values: invalid channel {}",
                    e.entry.pv_name
                );
            }
        }
        entries
    }

    /// C `write_save_file(plist, configName, ...)` with a `configName`:
    /// this set's values written to `path`, with no `.savB` check. A file
    /// already at `path` is first copied to `<path>_<datetime>`, and with
    /// the set's backups on the new file is then copied to that name, which
    /// C keeps in `backup_file` and leaves "" when there was no file, so
    /// that copy fails and the save is a WARN and an error
    /// (save_restore.c:2081-2092, :2112-2124). The list's status, save time
    /// and SR_recentlyStr are the save's, as for any `write_save_file`
    /// (:2012, :2100-2110, :2127-2134). `entries` are the values
    /// [`Self::get_channel_values`] fetched.
    pub async fn save_to(
        &self,
        entries: Vec<SaveEntry>,
        path: &std::path::Path,
    ) -> AutosaveResult<()> {
        let datetime = backup::date_str();
        let missing = entries.iter().filter(|e| !e.connected).count();
        *self.list_status.lock().unwrap() = ListStatus::new(SrStatus::Ok, "Ok");
        request::concatenated(&path.to_string_lossy(), &self.settings);
        let mut backup_file = PathBuf::new();
        if std::fs::File::open(path).is_ok() {
            let mut name = path.as_os_str().to_os_string();
            name.push(format!("_{}", backup::date_str()));
            backup_file = PathBuf::from(name);
            let _ = publish_copy(path, &backup_file, &self.settings).await;
        }
        if self.settings.debug() > 2 {
            crate::epics_println!("write_save_file: saving to {}", path.display());
        }
        if let Err(e) =
            write_save_file_with_settings(path, &entries, &self.settings, &self.save_file()).await
        {
            return Err(self.record_write_failure(e).await);
        }
        *self.save_time.lock().unwrap() = chrono::Local::now();
        *self.last_save_file.lock().unwrap() = self.save_file();
        if self.do_backups()
            && let Err(e) = publish_copy(path, &backup_file, &self.settings).await
        {
            crate::epics_println!(
                "save_restore:write_save_file - Couldn't make backup '{}' [{datetime}]",
                backup_file.display()
            );
            self.note_recent(format!("Can't write '{}B'", self.save_file()));
            return Err(self
                .record_save_failure(
                    e,
                    ListStatus::new(SrStatus::Warn, "Can't copy .sav to .savB file"),
                )
                .await);
        }
        if missing > 0 {
            *self.list_status.lock().unwrap() = not_saved_status(missing);
        }
        self.note_recent(format!("Wrote '{}'", self.save_file()));
        Ok(())
    }

    /// Perform one save cycle: collect PV values -> guarantee `.savB` ->
    /// write `.sav` -> advance `.savB`.
    pub async fn save_once(&self, db: &PvDatabase) -> AutosaveResult<usize> {
        let entries = self.get_channel_values(db);
        self.write_entries(db, entries).await.map(|(n, _)| n)
    }

    /// [`Self::save_once`] of values already fetched, with the file it
    /// wrote: C's `retSaveFile`, empty when a path or name PV held "" and
    /// C wrote nothing and returned OK (save_restore.c:2022, :2035).
    pub async fn write_entries(
        &self,
        db: &PvDatabase,
        save_entries: Vec<SaveEntry>,
    ) -> AutosaveResult<(usize, PathBuf)> {
        let Some(path) = self.file_path(db) else {
            *self.list_status.lock().unwrap() = ListStatus::new(SrStatus::Ok, "Ok");
            return Ok((0, PathBuf::new()));
        };
        let n = self.write_to(save_entries, &path).await?;
        Ok((n, path))
    }

    /// C's `SAVEPATHPV` or `SAVENAMEPV` for this list, the PV its file's
    /// directory or name is read from (save_restore.c:3689-3697).
    pub fn file_pv(&self, key: &str) -> Option<&str> {
        match key {
            SAVEPATHPV => self.file_macros.path_pv.as_deref(),
            SAVENAMEPV => self.file_macros.name_pv.as_deref(),
            _ => None,
        }
    }

    /// C `plist->do_backups`: the set's backups, unless its request file's
    /// macros turned them off.
    pub fn do_backups(&self) -> bool {
        self.config.backup.enable_savb && !self.file_macros.no_backups
    }

    /// C `write_save_file`'s directory (save_restore.c:2018-2028):
    /// `standard`, C's `saveRestoreFilePath`, or the `SAVEPATHPV`'s value,
    /// under `standard` when relative. `None` for a PV holding "".
    pub(super) fn file_dir(&self, db: &PvDatabase, standard: &Path) -> Option<PathBuf> {
        let Some(pv) = self.file_pv(SAVEPATHPV) else {
            return Some(standard.to_path_buf());
        };
        let dir = file_pv_value(db, pv);
        if dir.is_empty() {
            return None;
        }
        if request::is_absolute(&dir) {
            return Some(dir.into());
        }
        let standard = standard.to_string_lossy();
        Some(request::concatenate_paths(&standard, &dir, &self.settings).into())
    }

    /// C `write_save_file`'s file with no `configName`
    /// (save_restore.c:2018-2039): the set's own unless a path or name PV
    /// says otherwise; `None` when one of them holds "".
    pub(super) fn file_path(&self, db: &PvDatabase) -> Option<PathBuf> {
        let name_pv = self.file_pv(SAVENAMEPV);
        if self.file_pv(SAVEPATHPV).is_none() && name_pv.is_none() {
            return Some(self.save_path());
        }
        let own = self.save_path();
        let dir = self.file_dir(db, own.parent().unwrap_or(Path::new("")))?;
        let name = match name_pv {
            Some(pv) => file_pv_value(db, pv),
            None => self.save_file(),
        };
        if name.is_empty() {
            return None;
        }
        let dir = dir.to_string_lossy();
        Some(request::concatenate_paths(&dir, &name, &self.settings).into())
    }

    /// [`Self::write_entries`] to `path`.
    async fn write_to(&self, save_entries: Vec<SaveEntry>, path: &Path) -> AutosaveResult<usize> {
        {
            let mut s = self.status.write().await;
            *s = SaveSetStatus::Saving;
        }

        // The values are read before the rotation because the `.savB` that
        // C writes when the old one is unusable is written from the values
        // about to be saved, not copied from a file.
        let saved_count = save_entries.iter().filter(|e| e.connected).count();
        request::concatenated(&path.to_string_lossy(), &self.settings);

        // Rotate backups. A rotation that could not preserve the current
        // `.sav`, or could not put a usable `.savB` in place first, aborts
        // the cycle instead of overwriting the generation it failed to
        // copy, and it lands in the same bookkeeping a failed write does —
        // an early `?` here left the set stuck in `Saving` with the error
        // counter untouched.
        let backup = BackupConfig {
            enable_savb: self.do_backups(),
        };
        let rotated = rotate_backups(path, &backup, &save_entries, &self.settings).await;
        let savb_state = match rotated {
            Ok(state) => state,
            Err(e) => {
                let status = ListStatus::new(SrStatus::Fail, "Can't write .savB file");
                return Err(self.record_save_failure(e, status).await);
            }
        };

        if self.settings.debug() > 2 {
            crate::epics_println!("write_save_file: saving to {}", path.display());
        }
        match write_save_file_with_settings(path, &save_entries, &self.settings, &self.save_file())
            .await
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
                if let Err(e) = publish_savb(path, savb_state, &self.settings).await {
                    crate::epics_println!(
                        "save_restore:write_save_file - Couldn't make backup '{}B' [{}]",
                        path.display(),
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
    /// `<save_file>_b_<yymmdd-hhmmss>` from the values the list last
    /// fetched, as C's `write_it` writes its channels (:2252).
    pub async fn do_periodic_dated_backup(&self) {
        let debug = self.settings.debug();
        if debug > 1 {
            crate::epics_println!("save_restore:doPeriodicDatedBackup: entry");
        }
        request::concatenated(&self.save_path().to_string_lossy(), &self.settings);
        let mut name = self.save_path().as_os_str().to_os_string();
        name.push(format!("_b_{}", backup::date_str()));
        let path = std::path::PathBuf::from(name);
        if debug > 1 {
            crate::epics_println!(
                "save_restore:doPeriodicDatedBackup: filename is '{}'",
                path.display()
            );
        }
        let entries = self.kept_entries();
        if write_save_file_with_settings(&path, &entries, &self.settings, &self.save_file())
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
    /// into the next sequence file, or write that file from the values the
    /// list last fetched when the copy fails; a missing `.sav` is written
    /// from them first. C fetches nothing here (:2183, :2192).
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
            crate::epics_println!(
                "save_restore:do_seq - '{}' not found.  Writing a new one. [{datetime}]",
                sav.display()
            );
            if self.write_entries(db, self.kept_entries()).await.is_err() {
                crate::epics_println!(
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
            crate::epics_println!(
                "save_restore:do_seq - Can't copy save file to '{}' [{datetime}]",
                path.display()
            );
            let entries = self.kept_entries();
            if write_save_file_with_settings(&path, &entries, &self.settings, &self.save_file())
                .await
                .is_err()
            {
                crate::epics_println!(
                    "save_restore:do_seq - Can't write seq. file from PV list. [{datetime}]"
                );
                let mut status = self.list_status.lock().unwrap();
                if status.code >= SrStatus::Warn {
                    *status = ListStatus::new(SrStatus::SeqWarn, "Can't write sequence file");
                }
                drop(status);
                self.note_recent(format!("Can't write '{name}'"));
                return;
            }
            crate::epics_println!(
                "save_restore:do_seq: Wrote seq. file from PV list. [{datetime}]"
            );
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
        let dir = path.parent().unwrap_or(Path::new("")).to_string_lossy();
        *path = request::concatenate(&dir, &save_filename[..end]).into();
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
        let (entries, status, file_macros) =
            Self::load_entries(&self.config, &self.settings).await?;
        self.entries = entries;
        self.file_macros = file_macros;
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

    /// The save task's `save_state` at the end of a pass.
    pub(super) fn publish_task_view(&self, save_state: u32) {
        *self.task_view.lock().unwrap() = save_state;
    }

    /// C `plist->save_state`, as of the last pass.
    pub fn task_view(&self) -> u32 {
        *self.task_view.lock().unwrap()
    }

    /// C `plist->not_connected`.
    pub fn not_connected(&self) -> i32 {
        self.not_connected.load(Ordering::Relaxed)
    }

    /// C `plist->not_connected = get_channel_values(plist)`
    /// (save_restore.c:1001, :1217, :1376): the members fetched, and how
    /// many were not, kept as the list's.
    pub fn get_channel_values(&self, db: &PvDatabase) -> Vec<SaveEntry> {
        self.get_channel_values_searching(db, &[])
    }

    /// [`Self::get_channel_values`] at the end of the `connect_list` that
    /// made the channels of the members `searching`, whose connection
    /// callbacks have not run yet (save_restore.c:1376).
    pub(super) fn get_channel_values_searching(
        &self,
        db: &PvDatabase,
        searching: &[usize],
    ) -> Vec<SaveEntry> {
        let fetched = self.fetch(db, searching);
        let not_connected = fetched.iter().filter(|f| !f.entry.connected).count();
        self.not_connected
            .store(not_connected as i32, Ordering::Relaxed);
        let entries = fetched.iter().map(|f| f.entry.clone()).collect();
        // save_restore.c:1509-1594: a member on a connected channel; one
        // whose count `connect_list` missed takes it now.
        for (ch, f) in self.channels.lock().unwrap().iter_mut().zip(&fetched) {
            if ch.chid.is_none() || !f.entry.connected {
                continue;
            }
            let name = &f.entry.pv_name;
            if ch.max == 0 {
                let (count, info) = super::manager::max_elements(db, name);
                ch.max = info.unwrap_or(i64::from(count));
                ch.curr = ch.max;
            }
            if ch.max >= 1 {
                (ch.value, ch.enum_val) = super::manager::channel_value(db, name, f.value.as_ref());
                if ch.max > 1 {
                    // `SR_get_array`'s count, of the field without the
                    // channel's filters (save_restore.c:1575-1578).
                    ch.curr = db
                        .get_pv(
                            crate::server::database::filters::parse_channel_name(name.as_bytes())
                                .record_path,
                        )
                        .map_or(ch.max, |v| i64::from(v.count()));
                }
            }
        }
        *self.fetched.lock().unwrap() = fetched;
        entries
    }

    /// The values the last [`Self::get_channel_values`] fetched, which C's
    /// channels hold and `write_it` writes.
    fn kept_entries(&self) -> Vec<SaveEntry> {
        let fetched = self.fetched.lock().unwrap();
        fetched.iter().map(|f| f.entry.clone()).collect()
    }

    /// C `connect_list`'s `ca_create_channel` of each member in `created`
    /// and its check of the new channel (save_restore.c:1286-1360): the
    /// member's index in [`Self::pv_names`], whether it was connected at
    /// the check, and C's `max_elements`.
    pub(super) fn channels_created(&self, created: &[(usize, bool, i64)]) {
        let mut channels = self.channels.lock().unwrap();
        for &(i, connected, max) in created {
            let ch = &mut channels[i];
            ch.chid = Some(Box::new(0));
            ch.value = match connected {
                true => "Connected",
                false => "Search Issued",
            }
            .to_string();
            ch.max = max;
            ch.curr = max;
        }
    }

    /// The members' channels, in [`Self::pv_names`]' order.
    pub(super) fn channels(&self) -> std::sync::MutexGuard<'_, Vec<Channel>> {
        self.channels.lock().unwrap()
    }

    /// C `do_manual_restore` of a file this list last saved
    /// (save_restore.c:3229-3266): the values its last fetch read, put
    /// back with no file read. C's status.
    async fn restore_fetched(&self, db: &PvDatabase, ca: CaContext) -> i32 {
        let not_connected = self.not_connected();
        if not_connected > 0 {
            crate::epics_println!(
                "save_restore:do_manual_restore: {not_connected} channel(s) not connected or fetched"
            );
            if !self.settings.incomplete_sets_ok() {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "../save_restore.c:do_manual_restore aborting restore\n"
                ));
                crate::runtime::log::errlog_vprintf(format_args!(
                    "do_manual_restore:failed because some PVs not connected\n"
                ));
                return -1;
            }
        }
        let fetched = self.fetched.lock().unwrap().clone();
        let mut num_errs = 0;
        for f in fetched {
            let pv = &f.entry.pv_name;
            let (base, field) = crate::server::database::parse_pv_name(pv);
            match f.value {
                // `curr_elements > 1`: `SR_put_array_values`, which returns
                // dbPut's 0 on success; C compares it with ECA_NORMAL and so
                // counts every array it restored as an error. A C defect,
                // not copied.
                Some(value) if value.is_array() && value.count() > 1 => {
                    if self.settings.debug() >= 5 {
                        let max = db.restore_target(pv).map_or(0, |t| t.elements);
                        trace(
                            "SR_put_array_values",
                            format_args!(
                                "restoring {} values to {pv} (max_elements={max})\n",
                                value.count()
                            ),
                        );
                    }
                    if let Err(e) = db.put_pv_array_static(pv, value) {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "do_manual_restore:SR_put_array_values() to '{pv}' failed with {}.\n",
                            e.to_eca_status(crate::error::CaOp::Write)
                        ));
                        num_errs += 1;
                    }
                }
                // `ca_put(DBR_STRING, ...)` of the fetched text. Through
                // rsrv a refusal by the record comes back as libca's block
                // on the list's channel, not as this status.
                _ if f.entry.connected => {
                    let text = match &f.value {
                        Some(value) if value.is_array() => {
                            super::manager::channel_value(db, pv, Some(value)).0
                        }
                        _ => String::from_utf8_lossy(
                            &epics_libcom_rs::runtime::epics_string::raw_from_escaped(
                                &f.entry.value,
                            ),
                        )
                        .into_owned(),
                    };
                    if db
                        .put_record_field_from_ca_no_notify(
                            base,
                            field,
                            EpicsValue::String(text.into()),
                        )
                        .await
                        .is_err()
                    {
                        match ca.refused_put() {
                            None => super::manager::write_exception(
                                db,
                                pv,
                                crate::types::DbFieldType::String,
                                1,
                            ),
                            Some(status) => {
                                crate::runtime::log::errlog_vprintf(format_args!(
                                    "do_manual_restore:ca_put() to '{pv}' failed with {status}.\n"
                                ));
                                num_errs += 1;
                            }
                        }
                    }
                }
                _ => {
                    crate::runtime::log::errlog_vprintf(format_args!(
                        "do_manual_restore:ca_put() to '{pv}' failed with {}.\n",
                        crate::error::CaError::Disconnected
                            .to_eca_status(crate::error::CaOp::Write)
                    ));
                    num_errs += 1;
                }
            }
        }
        num_errs
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

    /// The list's status code and text together, as save_restore.c:949-950
    /// sets both.
    pub(super) fn set_list_status(&self, status: ListStatus) {
        *self.list_status.lock().unwrap() = status;
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
    /// The task's context says what a refused put reports.
    Process(CaContext),
}

/// The CA context C's save task makes as it starts (save_restore.c:802),
/// which [`super::manager::AutosaveManager::start`] records once: it alone
/// decides what a put the record refuses prints and counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaContext {
    /// Made before iocInit's `dbServiceIOInit` (dbCa.c:344): rsrv serves
    /// the task's channels, `ca_put` returns ECA_NORMAL, and the server's
    /// refusal reaches libca's exception handler later.
    Rsrv,
    /// Made after it: `dbChannelIO::write` throws (dbChannelIO.cpp:93) and
    /// `ca_put` returns ECA_PUTFAIL.
    InProcess,
}

impl CaContext {
    /// The context a task starting now makes.
    pub(super) fn at(db: &PvDatabase) -> Self {
        match super::manager::ca_connects(db) {
            true => Self::InProcess,
            false => Self::Rsrv,
        }
    }

    /// `ca_put`'s status for a put the record refused: `None` for
    /// ECA_NORMAL.
    pub(super) fn refused_put(self) -> Option<u32> {
        match self {
            Self::Rsrv => None,
            Self::InProcess => Some(crate::error::ECA_PUTFAIL),
        }
    }
}

/// The `iocInit` point a boot restore runs at: pass 0 before the records'
/// `init_record`, pass 1 after (`initHookAfterInitDevSup`,
/// `initHookAfterInitDatabase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePass {
    Zero,
    One,
}

/// The macros that name a list's path and name PVs.
pub const SAVEPATHPV: &str = "SAVEPATHPV";
pub const SAVENAMEPV: &str = "SAVENAMEPV";

/// What C's `readReqFile` takes from a level's macros once that level is
/// read, its includes' levels first (save_restore.c:3689-3699): each name
/// whose [`MacroContext::get_value`] is text, so a later level overrides
/// only with one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMacros {
    /// C `plist->savePathPV`.
    pub path_pv: Option<String>,
    /// C `plist->saveNamePV`.
    pub name_pv: Option<String>,
    /// A level named a path PV, a name PV or `CONFIGMENU`: C's
    /// `do_backups = 0`.
    pub no_backups: bool,
}

impl FileMacros {
    pub(super) fn take_from(&mut self, macros: &MacroContext) {
        if let Some(pv) = macros.get_value(SAVEPATHPV) {
            self.no_backups = true;
            self.path_pv = Some(pv);
        }
        if let Some(pv) = macros.get_value(SAVENAMEPV) {
            self.no_backups = true;
            self.name_pv = Some(pv);
        }
        if macros.get_value("CONFIGMENU").is_some() {
            self.no_backups = true;
        }
    }
}

/// `ca_array_get(DBR_STRING, 1, chid)` of a list's path or name PV. A PV
/// that does not resolve is "": C reads it into a buffer the failed get
/// leaves unset (save_restore.c:2020, :2033), which the oracle IOC wrote
/// to a directory named by stale stack bytes.
pub(super) fn file_pv_value(db: &PvDatabase, pv: &str) -> String {
    match super::manager::ca_get(db, pv) {
        Ok(live) => db.pv_dbr_string(pv, &live),
        Err(_) => String::new(),
    }
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
/// does not abort the rest. Line by line, so an array reads on through
/// the lines after its own as C's does.
pub async fn restore_from_entries_with_mode(
    db: &PvDatabase,
    path: &std::path::Path,
    mode: RestoreMode,
    settings: &SrSettings,
) -> AutosaveResult<RestoreResult> {
    let text = save_file::read_save_text(path).await?;
    if !save_file::has_end_marker(&text) {
        return Err(AutosaveError::CorruptSaveFile {
            path: path.display().to_string(),
            message: "missing <END> marker".to_string(),
        });
    }
    let mut result = RestoreResult::of(path);
    let mut p_data = Vec::new();
    let mut lines = text.split_inclusive('\n').enumerate();
    while let Some((index, line)) = lines.next() {
        if line.trim() == super::format::END_MARKER {
            break;
        }
        let mut read_on = || lines.next().map(|(_, line)| line.to_string());
        match by_name(line, true, settings.debug(), &mut read_on) {
            ByName::Restore => {}
            ByName::Next => continue,
            ByName::End => break,
        }
        let contents = save_file::parse_save_content(line);
        let input = &mut SavInput {
            read_on: &mut read_on,
            p_data: &mut p_data,
        };
        let r = restore_contents(db, path, &contents, mode, settings, input).await;
        result.absorb(r, index);
        if result.aborted {
            break;
        }
    }
    Ok(result)
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
/// no file opens, else the errors. `task` holds what C reads as globals:
/// the settings, the lists and the task's CA context.
pub async fn do_manual_restore(
    db: &PvDatabase,
    task: &super::manager::AutosaveManager,
    filename: &str,
    path: &std::path::Path,
    from: ManualSource,
    macrostring: &str,
) -> i32 {
    let settings = task.settings();
    let ca = task.ca_context(db);
    let debug = settings.debug();
    if debug >= 5 {
        crate::epics_println!("save_restore:do_manual_restore: entry for file '{filename}'");
    }
    // A list whose last save was `filename` restores from its own values
    // (save_restore.c:3216-3267).
    if from == ManualSource::SaveFile {
        let _list_lock = super::manager::lock_list(settings);
        let set = task
            .sets()
            .into_iter()
            .find(|(s, _)| s.last_save_file() == filename);
        if let Some((set, _)) = set {
            return set.restore_fetched(db, ca).await;
        }
    }
    if !request::is_absolute(filename) {
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
    let macros = MacroContext::from_macrostring(macrostring);
    if !macrostring.is_empty() && debug >= 5 {
        crate::epics_println!("save_restore:do_manual_restore: Current macro definitions:");
        macros.report();
        crate::epics_println!("save_restore:do_manual_restore: --------------------------");
    }
    let mut r = RestoreResult::of(&file);
    // C's `p_data`, kept for its trace only (save_restore.c:3466-3470).
    let mut p_data = Vec::new();
    let mut pv_name = String::new();
    let mut lines = text.split_inclusive('\n').enumerate();
    while let Some((index, line)) = lines.next() {
        let line = macros.expand(line);
        // C's `sscanf` reads no name from a blank line and leaves `PVname`
        // the line before's (save_restore.c:3322-3326), whose value it then
        // overwrites with "". A C defect: the name is traced, nothing put.
        let blank = match line.split_ascii_whitespace().next() {
            Some(name) => {
                pv_name = name.to_string();
                false
            }
            None => true,
        };
        if pv_name.starts_with(super::format::END_MARKER) {
            break;
        }
        if debug >= 5 {
            crate::epics_println!("save_restore:do_manual_restore: PVname='{pv_name}'");
        }
        if blank {
            continue;
        }
        // The lines an array reads on into, unexpanded.
        let mut read_on = || lines.next().map(|(_, line)| line.to_string());
        match by_name(&line, false, debug, &mut read_on) {
            ByName::Restore => {}
            ByName::Next => continue,
            ByName::End => break,
        }
        let contents = save_file::parse_save_content(&line);
        let input = &mut SavInput {
            read_on: &mut read_on,
            p_data: &mut p_data,
        };
        let one = restore_contents(
            db,
            &file,
            &contents,
            RestoreMode::Process(ca),
            settings,
            input,
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
            crate::epics_println!(
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
    let macros = MacroContext::from_macrostring(macrostring);
    if !macrostring.is_empty() && debug >= 5 {
        errlog(format_args!("Current macro definitions:\n"));
        macros.report();
        errlog(format_args!("--------------------------\n"));
    }
    if debug >= 1 {
        let header = text.split_inclusive('\n').next().unwrap_or("");
        errlog(format_args!("header line '{header}'\n"));
    }
    let mut result = RestoreResult::of(path);
    // C's `p_data`, kept for its trace only (`:1074-1078`).
    let mut p_data = Vec::new();
    let mut lines = text.split_inclusive('\n').enumerate();
    while let Some((index, line)) = lines.next() {
        let line = if !macros.has_handle() {
            line.to_string()
        } else {
            let expanded = macros.expand(line);
            if debug >= 5 {
                crate::epics_println!("reboot_restore: buffer='{line}'");
                crate::epics_println!("                         ebuffer='{expanded}'");
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
        // The lines an array reads on into, unexpanded.
        let mut read_on = || lines.next().map(|(_, line)| line.to_string());
        match by_name(&line, true, debug, &mut read_on) {
            ByName::Restore => {}
            ByName::Next => continue,
            ByName::End => break,
        }
        let contents = save_file::parse_save_content(&line);
        let input = &mut SavInput {
            read_on: &mut read_on,
            p_data: &mut p_data,
        };
        let r = restore_contents(
            db,
            path,
            &contents,
            RestoreMode::Boot(pass),
            settings,
            input,
        )
        .await;
        result.absorb(r, index);
        if result.aborted {
            break;
        }
    }
    Ok(result)
}

/// The file a restore reads, as C's array restore uses it.
struct SavInput<'a> {
    /// The lines an array reads on into, unexpanded.
    read_on: &'a mut ReadOn<'a>,
    /// C's `p_data`, kept for its trace only.
    p_data: &'a mut Vec<u8>,
}

/// Write every entry of a parsed line of `input` into the database; one
/// failed put does not stop the rest.
async fn restore_contents(
    db: &PvDatabase,
    path: &std::path::Path,
    contents: &save_file::SaveFileContents,
    mode: RestoreMode,
    settings: &SrSettings,
    input: &mut SavInput<'_>,
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
            crate::epics_println!(
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
                crate::epics_println!("save_restore:do_manual_restore: aborting restore");
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
        // C reads `is_scalar` off the `@array@` marker, then off the field
        // (`:913-928`).
        let mut is_scalar = !entry.value.starts_with(super::format::ARRAY_MARKER);
        // `SR_get_array_info` of the name as written, a long string's `$`
        // address included.
        let long_string = entry.pv_name.ends_with('$');
        let elements = match boot {
            true => db.restore_target(&entry.pv_name).map_or(0, |t| t.elements),
            false => super::manager::ca_element_count(db, &entry.pv_name),
        };
        if !boot {
            if is_scalar && elements > 1 {
                if debug >= 5 {
                    crate::epics_println!(
                        "save_restore:do_manual_restore: PV '{}' is scalar in .sav file, but has {elements} elements.  Treating as array.",
                        entry.pv_name
                    );
                }
                is_scalar = false;
            }
            if debug >= 5 && long_string {
                // The value as far as the line's first `fgets` reached.
                let first = (BUF_SIZE - 1).saturating_sub(entry.pv_name.len() + 1);
                let value = &entry.value.as_bytes()[..entry.value.len().min(first)];
                crate::epics_println!(
                    "save_restore:do_manual_restore: PV '{}' is long string; value='{}'.",
                    entry.pv_name,
                    String::from_utf8_lossy(value)
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
                    crate::epics_println!(
                        "reboot_restore: PV '{pv}' is scalar in .sav file, but has {elements} elements.  Treating as array."
                    );
                }
                is_scalar = false;
            }
            if debug >= 10 {
                let value = &entry.value;
                trace(
                    "reboot_restore",
                    format_args!(
                        "Attempting to put {} '{value}' to '{pv}'\n",
                        if is_scalar { "scalar" } else { "array" }
                    ),
                );
            }
            // The line overran the first `fgets` (dbrestore.c:942-944).
            let line_len = entry.pv_name.len() + entry.value.len() + 2;
            if long_string && debug > 9 && line_len > BUF_SIZE - 1 {
                crate::epics_println!(
                    "reboot_restore: did not reach end of line for long-string PV"
                );
            }
        }

        // `SR_array_restore` / `manual_array_restore`, else `scalar_restore`
        // or `ca_put` (dbrestore.c:976-980, save_restore.c:3361-3401).
        let array = !is_scalar && !long_string;

        let current = if long_string {
            let reachable = match boot {
                true => db
                    .static_restore_target(&entry.pv_name[..entry.pv_name.len() - 1])
                    .is_some(),
                false => {
                    super::manager::ca_connects(db)
                        && long_string_view(db, &entry.pv_name).is_some()
                }
            };
            reachable.then_some(Current::LongString)
        } else if boot {
            // `dbFindRecord` and `dbFoundField` (`:966-975`) find any
            // declared field, one `dbGet` refuses included.
            match db.get_pv(&entry.pv_name) {
                Ok(value) => Some(Current::Value(value)),
                Err(_) => db
                    .static_restore_target(&entry.pv_name)
                    .map(|_| Current::Unreadable),
            }
        } else {
            super::manager::ca_get(db, &entry.pv_name)
                .ok()
                .map(Current::Value)
        };
        let current = match current {
            Some(current) => current,
            None => {
                if boot {
                    let pv = c_pv_name(&entry.pv_name);
                    // A missing field fails `dbFindRecord` as a missing
                    // record does (dbStaticLib.c:1847-1858); :971's "did
                    // not find field" needs a name with no `.`, which :912
                    // never leaves.
                    crate::runtime::log::errlog_vprintf(format_args!(
                        "../dbrestore.c:reboot_restore dbFindRecord for '{pv}' failed\n"
                    ));
                    // `SR_array_restore` gobbling the array (`:995-1000`).
                    if !is_scalar {
                        let value = &entry.value;
                        gobble_array(&pv, value, debug, input.read_on);
                    }
                } else if !entry.pv_name.ends_with('$')
                    && !entry.value.starts_with(super::format::ARRAY_MARKER)
                {
                    // `ca_pend_io` times the search out (save_restore.c:3367);
                    // a long string or an array is counted silently.
                    crate::epics_println!(
                        "save_restore:do_manual_restore: ca_search for {} timeout",
                        entry.pv_name
                    );
                } else if !is_scalar && !entry.pv_name.ends_with('$') {
                    // `manual_array_restore` gobbling the array (:3406-3410).
                    let manual = ArrayFn::Manual;
                    if debug >= 1 {
                        manual.entry(&entry.pv_name);
                    }
                    let walk =
                        ArrayWalk::read(Some(manual), &entry.value, input.read_on, 0, true, debug);
                    walk.finish(debug, 0);
                    if debug >= 1 {
                        manual.line(format_args!("Gobbled unused array data.\n"));
                    }
                    if walk.eof {
                        crate::epics_println!(
                            "save_restore:do_manual_restore: manual_array_restore() returned 1"
                        );
                        result.parse_failed.push(entry.pv_name.clone());
                    }
                }
                result.not_found.push(entry.pv_name.clone());
                continue;
            }
        };

        let put_result = match mode {
            RestoreMode::Boot(pass) => {
                boot_store(db, entry, &current, array, pass, debug, input).await
            }
            RestoreMode::Process(ca) => {
                match manual_store(db, entry, &current, array, elements, debug, input).await {
                    Err(StoreError::Put(e)) => file_put_refused(ca, entry, &current, array, e),
                    stored => stored,
                }
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
            // The array walk's `end_of_file` status.
            Err(StoreError::EndOfFile) => {
                match boot {
                    true => restore_failed(&entry.pv_name),
                    false => crate::epics_println!(
                        "save_restore:do_manual_restore: manual_array_restore() returned 1"
                    ),
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
            Err(StoreError::Refused(error)) => {
                result.failed_puts.push(PvRestoreError {
                    pv_name: entry.pv_name.clone(),
                    error,
                });
            }
            Err(StoreError::Put(e)) => {
                restore_failed(&entry.pv_name);
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
    current: &Current,
    array: bool,
    elements: u32,
    debug: i32,
    input: &mut SavInput<'_>,
) -> Result<Stored, StoreError> {
    let current = match current {
        Current::Value(current) => current,
        Current::Unreadable => {
            return Err(StoreError::Put(crate::error::CaError::BadDbrType(
                noaccess_put(&entry.pv_name),
            )));
        }
        Current::LongString => {
            let (base, field) = crate::server::database::parse_pv_name(&entry.pv_name);
            let chars = long_string_chars(&entry.value);
            // `ca_array_put` of more than the `$` channel's element count
            // is ECA_BADCOUNT (save_restore.c:3394-3397).
            if chars.count() > elements {
                let text = long_string_value(&entry.value);
                let text = text.split(|&c| c == 0).next().unwrap_or_default();
                crate::epics_println!(
                    "save_restore:do_manual_restore: ca_array_put of '{}' to '{}' failed",
                    String::from_utf8_lossy(text),
                    entry.pv_name
                );
                return Err(StoreError::Refused("ECA_BADCOUNT".into()));
            }
            return db
                .put_record_field_from_ca_no_notify(base, field, chars)
                .await
                .map(|()| Stored::Yes)
                .map_err(StoreError::Put);
        }
    };
    let parsed = if array {
        let manual = ArrayFn::Manual;
        let (field_type, field_size) = ca_field_type(current);
        manual.setup(&entry.pv_name, elements, field_size, debug, input.p_data);
        if debug >= 10 {
            manual.line(format_args!("...field_type is ({field_type})\n"));
        }
        let value = &entry.value;
        let walk = ArrayWalk::read(Some(manual), value, input.read_on, elements, false, debug);
        let parsed = walk.value(value, current, ArrayRestore::Manual);
        if let Some(parsed) = parsed.as_ref().filter(|_| debug >= 10 && walk.begin) {
            let num_read = (parsed.count() as u32).min(elements) as usize;
            crate::epics_println!("\nsave_restore: {num_read} array values:");
            for value in ca_value_lines(parsed).into_iter().take(num_read) {
                crate::epics_println!("\t{value}");
            }
            crate::epics_println!("save_restore: end of {num_read} array values.\n");
            crate::runtime::task::sleep_background(Duration::from_millis(500)).await;
        }
        walk.finish(debug, 0);
        if walk.eof {
            manual.unwritten(debug, 1, input.p_data);
            return Err(StoreError::EndOfFile);
        }
        if parsed.is_some() && debug >= 1 {
            manual.line(format_args!("Writing array to database\n"));
        }
        parsed
    } else {
        // `ca_put(DBR_STRING)` of the unescaped text, cut at 40
        // (save_restore.c:3362-3369): the field converts it.
        let mut text =
            epics_libcom_rs::runtime::epics_string::raw_from_escaped_bytes(entry.value.as_bytes());
        text.truncate(40);
        Some(EpicsValue::String(text.into()))
    };
    let mut parsed = parsed.ok_or(StoreError::Parse)?;
    if array {
        let Some(value) = into_field(parsed, current) else {
            return Ok(Stored::Yes);
        };
        parsed = value;
    }
    let (base, field) = crate::server::database::parse_pv_name(&entry.pv_name);
    db.put_record_field_from_ca_no_notify(base, field, parsed)
        .await
        .map(|()| Stored::Yes)
        .map_err(StoreError::Put)
}

/// [`manual_store`]'s refused put on the file's `ca_search` channel. Through
/// rsrv `ca_put` returns ECA_NORMAL and `ca_clear_channel`
/// (save_restore.c:3423) drops the server's refusal before libca reports
/// it: nothing prints or counts. In process C prints the put's line
/// (:3370-3371, :3397-3398, :3178-3179 with :3417-3419) and counts it.
fn file_put_refused(
    ca: CaContext,
    entry: &SaveEntry,
    current: &Current,
    array: bool,
    e: crate::error::CaError,
) -> Result<Stored, StoreError> {
    if ca.refused_put().is_none() {
        return Ok(Stored::Skipped);
    }
    let pv = &entry.pv_name;
    match current {
        Current::LongString => {
            let text = long_string_value(&entry.value);
            let text = text.split(|&c| c == 0).next().unwrap_or_default();
            crate::epics_println!(
                "save_restore:do_manual_restore: ca_array_put of '{}' to '{pv}' failed",
                String::from_utf8_lossy(text)
            );
        }
        _ if array => {
            crate::runtime::log::errlog_vprintf(format_args!(
                "../save_restore.c:manual_array_restore ca_array_put to '{pv}' failed\n"
            ));
            crate::epics_println!(
                "save_restore:do_manual_restore: manual_array_restore() returned -1"
            );
        }
        _ => {
            // `value_string[40] = '\0'` (:3363).
            let mut text = epics_libcom_rs::runtime::epics_string::raw_from_escaped(&entry.value);
            text.truncate(40);
            let text = text.split(|&c| c == 0).next().unwrap_or_default();
            crate::epics_println!(
                "save_restore:do_manual_restore: ca_put of {} to {pv} failed",
                String::from_utf8_lossy(text)
            );
        }
    }
    Err(StoreError::Refused(e.to_string()))
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

/// A boot put's outcome. C's `dbPutString` and `dbPut` check no SPC_NOMOD
/// and write any field `dbFindField` found, RTYP's attribute and PACT
/// included; rs keeps a field it does not let a put write, and prints
/// nothing, as C prints nothing for the write. A C defect, not copied.
fn boot_put(r: crate::error::CaResult<()>) -> crate::error::CaResult<Stored> {
    use crate::error::CaError;
    match r {
        Ok(()) => Ok(Stored::Yes),
        Err(CaError::ReadOnlyField(_) | CaError::FieldNotFound(_)) => Ok(Stored::Skipped),
        Err(e) => Err(e),
    }
}

/// C `dbPut`'s refusal of a DBF_NOACCESS field (dbAccess.c:1340-1347),
/// its line printed; the status's text.
fn noaccess_put(pv_name: &str) -> String {
    use crate::types::DbfCode;
    let (record, field) = crate::server::database::parse_pv_name(pv_name);
    crate::server::recgbl::rec_gbl_bad_dbrtype(
        "dbPut",
        DbfCode::String,
        DbfCode::NoAccess,
        &String::from_utf8_lossy(record),
        field,
    );
    "Illegal Database Request Type".into()
}

/// One member as C `get_channel_values` left it: the text a save writes,
/// and the value read, for a member that was not a long string.
#[derive(Clone)]
struct Fetched {
    entry: SaveEntry,
    value: Option<EpicsValue>,
}

/// The fields of C's `struct channel` that `save_restoreShow` prints; its
/// `channel_connected` is [`super::manager::channel_state`]'s.
pub(super) struct Channel {
    /// C `chid`, made by `connect_list`; show prints its address.
    pub(super) chid: Option<Box<u8>>,
    pub(super) max: i64,
    pub(super) curr: i64,
    pub(super) value: String,
    pub(super) enum_val: Option<i16>,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            chid: None,
            max: 0,
            curr: 0,
            // save_restore.c:3672.
            value: "Not Connected".to_string(),
            enum_val: None,
        }
    }
}

/// [`boot_store`]'s static write (`dbPutString`, `dbPutMenuIndex`,
/// `SR_put_array_values`). C's static writes check no SPC_NOMOD and
/// overwrite PACT, PUTF or RBV; rs refuses them as a put would, and
/// [`boot_put`] keeps the field silently.
async fn static_write(
    target: &crate::server::database::RestoreTarget,
    pv: &str,
    write: impl std::future::Future<Output = crate::error::CaResult<()>>,
) -> crate::error::CaResult<()> {
    if target.no_mod {
        return Err(crate::error::CaError::ReadOnlyField(pv.to_string()));
    }
    write.await
}

/// Whether [`boot_store`] wrote the value or C's pass rules left it alone.
enum Stored {
    Yes,
    Skipped,
}

enum StoreError {
    Parse,
    /// An array walk's `end_of_file` status.
    EndOfFile,
    /// A put whose `ca_put` status C printed, named by `.0`.
    Refused(String),
    /// A boot store's refusal.
    Put(crate::error::CaError),
    /// `scalar_restore`'s refusal of `value`, with the status's
    /// `errSymLookup` text, empty for a status C has no symbol for.
    Scalar {
        value: String,
        status: String,
    },
}

/// One saved value into the database as C `reboot_restore` stores it
/// (`dbrestore.c:963-991`): an `array`, or a scalar by its declared type
/// (`scalar_restore`, `:222-327`).
async fn boot_store(
    db: &PvDatabase,
    entry: &SaveEntry,
    current: &Current,
    array: bool,
    pass: RestorePass,
    debug: i32,
    input: &mut SavInput<'_>,
) -> Result<Stored, StoreError> {
    use crate::types::DbfCode;
    let pv = c_pv_name(&entry.pv_name);

    // C looks the field up without the long-string `$` (`:948-951`).
    let name = entry.pv_name.strip_suffix('$');
    let Some(target) = db.static_restore_target(name.unwrap_or(&entry.pv_name)) else {
        // A PV no record serves has no special() and no processing to skip.
        let parsed = match current {
            Current::Value(current) if entry.value.starts_with(super::format::ARRAY_MARKER) => {
                let walk = ArrayWalk::read(None, &entry.value, input.read_on, u32::MAX, false, 0);
                if walk.eof {
                    return Err(StoreError::EndOfFile);
                }
                let parsed = walk
                    .value(&entry.value, current, ArrayRestore::Boot)
                    .ok_or(StoreError::Parse)?;
                let Some(parsed) = into_field(parsed, current) else {
                    return Ok(Stored::Yes);
                };
                parsed
            }
            Current::Value(current) => {
                save_file::parse_save_value(&entry.value, current).ok_or(StoreError::Parse)?
            }
            Current::LongString => long_string_chars(&entry.value),
            Current::Unreadable => {
                return Err(StoreError::Put(crate::error::CaError::BadDbrType(
                    noaccess_put(&entry.pv_name),
                )));
            }
        };
        return boot_put(db.put_pv_no_process(&entry.pv_name, parsed).await)
            .map_err(StoreError::Put);
    };
    let put = |r: crate::error::CaResult<()>| boot_put(r).map_err(StoreError::Put);
    // `SR_array_restore`, written in pass 1 only (`:722-723`).
    if array {
        let array = |line: std::fmt::Arguments<'_>| trace("SR_array_restore", line);
        let (field_type, elements) = (target.addr_type, target.elements);
        let field_size = usize::from(target.field_size);
        ArrayFn::Boot.setup(&pv, elements, field_size, debug, input.p_data);
        if debug >= 10 {
            array(format_args!(
                "field_type is 'DBF_{}' ({})\n",
                field_type.name(),
                field_type as i16
            ));
        }
        // Its switch (`:461-474`): DBF_NOACCESS goes through the motions.
        let status = match field_type {
            DbfCode::Device | DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink => {
                array(format_args!(
                    "field_type 'DBF_{}' not handled\n",
                    field_type.name()
                ));
                -1
            }
            _ => 0,
        };
        // Only a type the element switch stores holds, and counts, elements
        // (`:624-645`); a field kept only by name has no value to convert
        // them by.
        let stores = status == 0 && field_type != DbfCode::NoAccess;
        let value = &entry.value;
        let walk = ArrayWalk::read(
            Some(ArrayFn::Boot),
            value,
            input.read_on,
            elements,
            !stores,
            debug,
        );
        let parsed = match current {
            Current::Value(current) if stores => walk.value(value, current, ArrayRestore::Boot),
            _ => None,
        };
        let num_read = parsed
            .as_ref()
            .map_or(0, |p| p.count().min(elements) as usize);
        // Only a value with its `{` is listed (`:537`, `:653`).
        if debug >= 10 && walk.begin {
            array(format_args!("\nsave_restore: {num_read} array values:\n"));
            for value in parsed.iter().flat_map(array_value_lines).take(num_read) {
                array(format_args!("\t{value}\n"));
            }
            array(format_args!("end of {num_read} array values.\n\n"));
            crate::runtime::task::sleep_background(Duration::from_millis(500)).await;
        }
        walk.finish(debug, status);
        let status = if status == 0 && walk.eof { 1 } else { status };
        match pass {
            RestorePass::Zero if debug >= 1 => array(format_args!("No array write in pass 0.\n")),
            RestorePass::One if status != 0 => ArrayFn::Boot.unwritten(debug, status, input.p_data),
            _ => {}
        }
        match status {
            0 => {}
            1 => return Err(StoreError::EndOfFile),
            _ => {
                return Err(StoreError::Put(crate::error::CaError::BadDbrType(format!(
                    "field_type 'DBF_{}' not handled",
                    field_type.name()
                ))));
            }
        }
        if stores && parsed.is_none() {
            return Err(StoreError::Parse);
        }
        if pass == RestorePass::Zero {
            return Ok(Stored::Skipped);
        }
        if debug >= 1 {
            array(format_args!("Writing array to database\n"));
        }
        if debug >= 5 {
            trace(
                "SR_put_array_values",
                format_args!("restoring {num_read} values to {pv} (max_elements={elements})\n"),
            );
        }
        // `VALID_DB_REQ` (`:354-358`): not a menu, nor DBF_NOACCESS.
        if field_type as i16 > DbfCode::Enum as i16 {
            let refused = format!("PV {pv}: bad field type '{}'", field_type as i16);
            trace("SR_put_array_values", format_args!("{refused}\n"));
            return Err(StoreError::Put(crate::error::CaError::BadDbrType(refused)));
        }
        let (Some(parsed), Current::Value(current)) = (parsed, current) else {
            return Err(StoreError::Parse);
        };
        let Some(parsed) = into_field(parsed, current) else {
            return Ok(Stored::Yes);
        };
        return put(static_write(&target, &entry.pv_name, async {
            db.put_pv_array_static(&entry.pv_name, parsed)
        })
        .await);
    }
    if debug >= 5 {
        trace(
            "scalar_restore",
            format_args!("entry:field type 'DBF_{}'\n", target.dbf.name()),
        );
    }
    // `value_string` after `epicsStrnRawFromEscaped` (`:235`).
    let text = match current {
        Current::LongString => {
            String::from_utf8_lossy(&long_string_value(&entry.value)).into_owned()
        }
        Current::Value(_) | Current::Unreadable => {
            match save_file::parse_save_value(&entry.value, &EpicsValue::String("".into())) {
                Some(EpicsValue::String(s)) => s.to_string(),
                _ => entry.value.clone(),
            }
        }
    };
    let scalar = |value: &str, r: crate::error::CaResult<()>| {
        boot_put(r).map_err(|e| StoreError::Scalar {
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
                scalar(
                    &text,
                    static_write(
                        &target,
                        &entry.pv_name,
                        db.put_pv_static(&entry.pv_name, &text),
                    )
                    .await,
                ),
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
                static_write(
                    &target,
                    &entry.pv_name,
                    db.put_pv_menu_index(&entry.pv_name, crate::types::c_parse::atol(&text) as i32),
                )
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
            // `dbPut` of the text as DBF_STRING (`:296-302`), which the
            // field converts.
            RestorePass::One => {
                let parsed = match current {
                    Current::Value(_) => Some(EpicsValue::String(text.as_str().into())),
                    Current::LongString => Some(long_string_chars(&entry.value)),
                    Current::Unreadable => None,
                };
                match parsed {
                    Some(parsed) => {
                        scalar(&text, db.put_pv_no_process(&entry.pv_name, parsed).await)
                    }
                    None => Err(StoreError::Scalar {
                        status: noaccess_put(&entry.pv_name),
                        value: text.clone(),
                    }),
                }
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
            scalar(
                "0",
                static_write(
                    &target,
                    &entry.pv_name,
                    db.put_pv_static(&entry.pv_name, "0"),
                )
                .await,
            ),
        ),
        _ => traced(
            "dbPutString",
            265,
            scalar(
                &text,
                static_write(
                    &target,
                    &entry.pv_name,
                    db.put_pv_static(&entry.pv_name, &text),
                )
                .await,
            ),
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

/// The text C saves for the long-string member `pv` (`name$`): the `$`
/// channel's `DBR_CHAR` read of at most BUF_SIZE characters
/// (save_restore.c:1580-1588), to its NUL and cut at BUF_SIZE-1 (:1846).
/// `None` is a field the `$` channel cannot connect to.
pub(super) fn long_string_text(db: &PvDatabase, pv: &str) -> Option<Vec<u8>> {
    let view = long_string_view(db, pv)?;
    let n = view.no_elements.min(BUF_SIZE);
    let chars = view.get(crate::types::DbFieldType::Char, n)?;
    let EpicsValue::CharArray(chars) = chars else {
        return None;
    };
    Some(
        chars
            .iter()
            .copied()
            .take_while(|&c| c != 0)
            .take(BUF_SIZE - 1)
            .collect(),
    )
}

use super::format::BUF_SIZE;

/// The `$` address of the long-string member `pv` (`name$`), as
/// `dbNameToAddr` and the `$` channel give it (`dbAccess.c:682-701`);
/// `None` where C refuses the modifier.
fn long_string_view(
    db: &PvDatabase,
    pv: &str,
) -> Option<crate::server::database::char_view::CharView> {
    let parts = crate::server::database::DbNameParts::of(pv.as_bytes());
    if !parts.tail.starts_with(b"$") {
        return None;
    }
    let rec = db.get_record(parts.record)?;
    let field = rec.canonical_field(if parts.field.is_empty() {
        "VAL"
    } else {
        parts.field
    });
    crate::server::database::char_view::char_view(&rec.read(), &field)
}

/// A long-string member's file value as C restores it: at most BUF_SIZE-1
/// characters (dbrestore.c:944-958, save_restore.c:3378-3387), unescaped.
fn long_string_value(value: &str) -> Vec<u8> {
    let cut = &value.as_bytes()[..value.len().min(BUF_SIZE - 1)];
    epics_libcom_rs::runtime::epics_string::raw_from_escaped_bytes(cut)
}

/// [`long_string_value`] as the `DBR_CHAR` put of `strlen + 1` characters
/// C makes through the `$` address (dbrestore.c:298-299, save_restore.c:3394).
fn long_string_chars(value: &str) -> EpicsValue {
    let mut chars: Vec<u8> = long_string_value(value)
        .into_iter()
        .take_while(|&c| c != 0)
        .collect();
    chars.push(0);
    EpicsValue::CharArray(chars.into())
}

/// What a restore holds of a member before it writes: its value, or
/// nothing for a long-string member (`name$`), which C writes without
/// reading, through dbStatic at boot (dbrestore.c:948-951) and through its
/// `$` channel by hand (save_restore.c:3389-3397).
enum Current {
    Value(EpicsValue),
    LongString,
    /// A declared field with no value: `dbNameToAddr` leaves it
    /// DBF_NOACCESS, so `dbGet` and `dbPut` refuse it. Only the boot
    /// lookup finds one; CA does not connect it.
    Unreadable,
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

/// `SR_array_restore(pass, inp_fd, pv, value, 1)`: `value`, an `@array@`
/// text, walked on through `read_on` and dropped, its status unread
/// (`dbrestore.c:418`, `:720-721`).
fn gobble_array(pv: &str, value: &str, debug: i32, read_on: &mut ReadOn<'_>) {
    if debug >= 1 {
        ArrayFn::Boot.entry(pv);
    }
    ArrayWalk::read(Some(ArrayFn::Boot), value, read_on, 0, true, debug).finish(debug, 0);
    if debug >= 1 {
        trace(
            "SR_array_restore",
            format_args!("Gobbled unused array data.\n"),
        );
    }
}

/// What C's line loop makes of a line by its name before it reads the
/// value (dbrestore.c:886-1016, save_restore.c:3322-3449).
enum ByName {
    Restore,
    /// A comment, its `@array@` value walked and dropped, or a name C
    /// neither restores nor reads on from.
    Next,
    /// The boot restore's `<` name, the end of the file.
    End,
}

/// [`ByName`] of `line` under the boot (`boot`) or the manual restore's
/// rules. C's `sscanf` splits the name, one separator and the value up to
/// `\n`, and `\r` too at boot.
fn by_name(line: &str, boot: bool, debug: i32, read_on: &mut ReadOn<'_>) -> ByName {
    let line = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let end = line
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(line.len());
    let (name, rest) = line.split_at(end);
    let first = name.as_bytes().first().copied().unwrap_or(0);
    if first == b'#' {
        let mut rest = rest.chars();
        rest.next();
        let value = rest.as_str();
        let stop: &[char] = if boot { &['\n', '\r'] } else { &['\n'] };
        let value = &value[..value.find(stop).unwrap_or(value.len())];
        if value.starts_with(super::format::ARRAY_MARKER) {
            gobble_array(name, value, debug, read_on);
        }
        return ByName::Next;
    }
    // `%80s` stops at 80 (:887, :906-910).
    if boot && name.len() >= 80 {
        let name = String::from_utf8_lossy(&name.as_bytes()[..80]);
        trace(
            "reboot_restore",
            format_args!("'{name}' is too long to be a PV name.\n"),
        );
        return ByName::Next;
    }
    let restores = match boot {
        true => first.is_ascii_alphanumeric(),
        false => super::startup::is_valid_first_pv_char(name),
    };
    match first {
        _ if restores => ByName::Restore,
        // The count of PVs with no saved value.
        b'!' => ByName::Restore,
        b'<' if boot => ByName::End,
        _ => ByName::Next,
    }
}

/// C's `%p` of `p_data`.
fn p_data_at(p_data: &[u8]) -> String {
    match p_data.is_empty() {
        true => "(nil)".to_string(),
        false => format!("{:p}", p_data.as_ptr()),
    }
}

/// The C function an array walk traces as: dbrestore.c's
/// `SR_array_restore` (ERRLOG) or save_restore.c's `manual_array_restore`
/// (printf), whose texts differ in `:entry:` and the element-search line.
#[derive(Clone, Copy)]
pub(super) enum ArrayFn {
    Boot,
    Manual,
}

impl ArrayFn {
    fn line(self, line: std::fmt::Arguments<'_>) {
        match self {
            Self::Boot => trace("SR_array_restore", line),
            Self::Manual => crate::epics_print!("save_restore:manual_array_restore: {line}"),
        }
    }

    /// The walk's first line (`dbrestore.c:418`, save_restore.c:2927).
    fn entry(self, pv: &str) {
        match self {
            Self::Boot => self.line(format_args!(":entry: PV = '{pv}'\n")),
            Self::Manual => {
                crate::epics_println!("save_restore:manual_array_restore:entry: PV = '{pv}'")
            }
        }
    }

    /// The `>= 1` report of a walk the file ended inside, which writes
    /// nothing (`dbrestore.c:706-709`, save_restore.c:3182-3187).
    fn unwritten(self, debug: i32, status: i64, p_data: &[u8]) {
        if debug >= 1 {
            self.line(format_args!(
                "No array write to database attempted because of error condition\n"
            ));
            self.line(format_args!(
                "status={status}, p_data={}\n",
                p_data_at(p_data)
            ));
        }
    }

    /// `:entry:` and the `p_data` bookkeeping of a non-gobble walk
    /// (`dbrestore.c:430-457`, save_restore.c:2927-2952); `p_data` is kept
    /// for the trace only.
    fn setup(self, pv: &str, elements: u32, field_size: usize, debug: i32, p_data: &mut Vec<u8>) {
        if debug >= 1 {
            self.entry(pv);
            let need = elements as usize * field_size;
            if p_data.is_empty() || need > p_data.len() {
                self.line(format_args!(
                    "p_data = {}, p_data_size = {}\n",
                    p_data_at(p_data),
                    p_data.len()
                ));
                *p_data = vec![0; need];
                if debug >= 10 {
                    self.line(format_args!(
                        "allocated p_data = {}, p_data_size = {}\n",
                        p_data_at(p_data),
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

/// The next line of the file, raw, as `fgets` gives it to C's array walk.
pub(super) type ReadOn<'a> = dyn FnMut() -> Option<String> + Send + 'a;

/// C's walk of an array member's value (`SR_array_restore`,
/// `dbrestore.c:477-713`; `manual_array_restore`, save_restore.c:2984-3170),
/// traced as `func` traces it, or silent without one. From the `{` of an
/// `@array@` value it reads on through each line `read_on` gives, as C reads
/// on in the file, to the `}` or the end of the file. A value without the
/// marker, C's text for an array of one element, is that element: C walks
/// from a `{` in it into the next PVs' lines, a C defect not copied.
pub(super) struct ArrayWalk {
    func: Option<ArrayFn>,
    /// `begin_mark_found`.
    pub(super) begin: bool,
    /// `end_of_file`: the file ended before the `}`.
    pub(super) eof: bool,
    /// The elements stored, each its file text with the escapes kept.
    elements: Vec<Vec<u8>>,
}

impl ArrayWalk {
    /// The walk up to `SR_array_restore`'s listing (`:477-651`); `max` is
    /// the field's elements, and a `gobble` walk stores none.
    pub(super) fn read(
        func: Option<ArrayFn>,
        value: &str,
        read_on: &mut ReadOn<'_>,
        max: u32,
        gobble: bool,
        debug: i32,
    ) -> Self {
        let debug = if func.is_some() { debug } else { 0 };
        let t = |line: std::fmt::Arguments<'_>| {
            if let Some(func) = func {
                func.line(line);
            }
        };
        let mut walk = Self {
            func,
            begin: false,
            eof: false,
            elements: Vec::new(),
        };
        if debug >= 11 {
            t(format_args!("parsing buffer '{value}'\n"));
        }
        if value.is_empty() {
            if debug >= 11 {
                t(format_args!("value_string is null or empty\n"));
            }
            return walk;
        }
        let begin = match value.starts_with(super::format::ARRAY_MARKER) {
            true => value.find('{'),
            false => None,
        };
        let Some(mut i) = begin else {
            if debug >= 11 {
                t(format_args!("ARRAY_BEGIN not found\n"));
            }
            if !gobble {
                walk.elements.push(value.as_bytes().to_vec());
            }
            return walk;
        };
        walk.begin = true;
        if debug >= 10 {
            t(format_args!("parsing array buffer '{}'\n", &value[i..]));
        }
        let dots = match func {
            Some(ArrayFn::Manual) => "...",
            _ => "",
        };
        let mut buf = value.as_bytes().to_vec();
        let at = |buf: &[u8], i: usize| buf.get(i).copied().unwrap_or(0);
        let rest = |buf: &[u8], i: usize| {
            String::from_utf8_lossy(buf.get(i..).unwrap_or_default()).into_owned()
        };
        // C's `fgets` at the end of the buffer; `false` at the end of the file.
        let mut fgets = |buf: &mut Vec<u8>, i: &mut usize| match read_on() {
            Some(line) => {
                (*buf, *i) = (line.into_bytes(), 0);
                true
            }
            None => false,
        };
        let (mut end, mut num_read) = (false, 0);
        while !end && !walk.eof {
            if debug >= 10 {
                t(format_args!("looking for element[{num_read}] \n"));
            }
            while !end && !walk.eof && at(&buf, i) != b'"' {
                if debug >= 12 {
                    t(format_args!("{dots}buffer contains '{}'\n", rest(&buf, i)));
                }
                match at(&buf, i) {
                    0 if !fgets(&mut buf, &mut i) => {
                        walk.errlog(format_args!("*** EOF during array-parse\n"));
                        walk.eof = true;
                    }
                    0 => {}
                    b'}' => end = true,
                    _ => i += 1,
                }
            }
            if end || walk.eof {
                break;
            }
            if debug >= 11 {
                t(format_args!(
                    "Found element-begin; buffer contains '{}'\n",
                    rest(&buf, i)
                ));
            }
            i += 1;
            // `text` is C's element, which the trace shows and whose 39
            // characters C counts. `raw` keeps the `\` C first takes off
            // `\"` and `\\` (:583-586), so the element is decoded once:
            // C's double decode reads `\\q` as `q`, a C defect not kept.
            let (mut text, mut raw) = (Vec::new(), Vec::new());
            while text.len() < 39 && at(&buf, i) != b'"' {
                if debug >= 11 {
                    t(format_args!(
                        "*bp={} ({})\n",
                        at(&buf, i) as char,
                        at(&buf, i) as i8
                    ));
                }
                if at(&buf, i) == 0 {
                    if !fgets(&mut buf, &mut i) {
                        walk.errlog(format_args!("*** premature EOF.\n"));
                        walk.eof = true;
                        break;
                    }
                    if debug >= 11 {
                        t(format_args!("new buffer: '{}'\n", rest(&buf, i)));
                    }
                    if at(&buf, i) == b'"' {
                        break;
                    }
                } else if at(&buf, i) == b'\\' && matches!(at(&buf, i + 1), b'"' | b'\\') {
                    raw.push(b'\\');
                    i += 1;
                }
                // A byte `isprint` rejects is dropped (:589).
                if (0x20..0x7f).contains(&at(&buf, i)) {
                    text.push(at(&buf, i));
                    raw.push(at(&buf, i));
                }
                i += 1;
            }
            if debug >= 10 {
                t(format_args!(
                    "element[{num_read}] value = '{}'\n",
                    String::from_utf8_lossy(&text)
                ));
                if !walk.eof {
                    t(format_args!(
                        "look for element-end: buffer contains '{}'\n",
                        rest(&buf, i)
                    ));
                }
            }
            let mut found = false;
            while !found && !walk.eof {
                while !matches!(at(&buf, i), 0 | b'"' | b'\\') {
                    i += 1;
                }
                match at(&buf, i) {
                    b'"' => {
                        found = true;
                        i += 1;
                    }
                    b'\\' => {
                        i += 1;
                        if matches!(at(&buf, i), b'"' | b'\\') {
                            i += 1;
                        }
                    }
                    _ => walk.eof = !fgets(&mut buf, &mut i),
                }
            }
            if num_read < max && !gobble {
                walk.elements.push(raw);
                num_read += 1;
            }
        }
        walk
    }

    /// The walk's `ERRLOG` line.
    fn errlog(&self, line: std::fmt::Arguments<'_>) {
        match self.func {
            Some(ArrayFn::Boot) => trace("SR_array_restore", line),
            Some(ArrayFn::Manual) => crate::runtime::log::errlog_vprintf(format_args!(
                "../save_restore.c:manual_array_restore {line}"
            )),
            None => {}
        }
    }

    /// What follows the listing (`:683-718`). The walk ends only at the
    /// `}` or the end of the file, so C's `ARRAY_END` search reads nothing;
    /// the end of the file is a status only when the setup gave none
    /// (`:707-710`, save_restore.c:3165-3168).
    pub(super) fn finish(&self, debug: i32, status: i64) {
        let Some(func) = self.func else {
            return;
        };
        if debug >= 10 && self.begin {
            func.line(format_args!("looking for ARRAY_END\n"));
        } else if debug >= 10 {
            func.line(format_args!("ARRAY_BEGIN wasn't found.\n"));
        }
        if status == 0 && self.eof {
            func.line(format_args!("status = end_of_file.\n"));
        }
    }

    /// The stored elements, each decoded once with `epicsStrnRawFromEscaped`.
    pub(super) fn texts(&self) -> Vec<String> {
        use epics_libcom_rs::runtime::epics_string::raw_from_escaped_bytes;
        self.elements
            .iter()
            .map(|e| String::from_utf8_lossy(&raw_from_escaped_bytes(e)).into_owned())
            .collect()
    }

    /// The member's value for a field like `template`: an `@array@` value's
    /// elements, another text as [`save_file::parse_array_value`] reads it.
    fn value(&self, value: &str, template: &EpicsValue, from: ArrayRestore) -> Option<EpicsValue> {
        if value.starts_with(super::format::ARRAY_MARKER) {
            Some(save_file::array_of_texts(&self.texts(), template, from))
        } else {
            save_file::parse_array_value(value, template, from)
        }
    }
}

/// The CA type `ca_field_type` gives a channel holding `value` and its
/// `dbr_size` (`dbDBRnewToDBRold`).
fn ca_field_type(value: &EpicsValue) -> (i16, usize) {
    use EpicsValue as V;
    match value {
        V::StringArray(_) | V::String(_) => (0, 40),
        V::ShortArray(_) | V::Short(_) => (1, 2),
        V::FloatArray(_) | V::Float(_) => (2, 4),
        V::EnumArray(_) | V::Enum(_) | V::EnumWithChoices { .. } => (3, 2),
        V::CharArray(_) | V::UCharArray(_) | V::Char(_) | V::UChar(_) => (4, 1),
        V::UShortArray(_) | V::LongArray(_) | V::UShort(_) | V::Long(_) => (5, 4),
        V::ULongArray(_)
        | V::Int64Array(_)
        | V::UInt64Array(_)
        | V::DoubleArray(_)
        | V::ULong(_)
        | V::Int64(_)
        | V::UInt64(_)
        | V::Double(_) => (6, 8),
    }
}

/// What `dbPutConvertRoutine` and `ca_array_put` write of an array `value`
/// into a field holding `current`: the array, or the first element of it
/// into a scalar, and nothing for no element (`num_read` 0).
fn into_field(value: EpicsValue, current: &EpicsValue) -> Option<EpicsValue> {
    match current.is_array() {
        true => Some(value),
        false => value.first_element(),
    }
}

/// `manual_array_restore`'s listing (save_restore.c:3115-3133), of the
/// values as their CA type holds them.
fn ca_value_lines(value: &EpicsValue) -> Vec<String> {
    let g = |v: f64| CFloat(v).g().to_string();
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
        EpicsValue::FloatArray(a) => each(a, |v| CFloat(f64::from(v)).to_string()),
        EpicsValue::DoubleArray(a) => each(a, |v| CFloat(v).g().to_string()),
        _ => Vec::new(),
    }
}
