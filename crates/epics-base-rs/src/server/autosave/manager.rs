use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::calc::engine::cvt::fmt_g;
use crate::server::database::PvDatabase;
use crate::server::iocsh::registry::CommandContext;
use crate::server::record::record_instance::value_as_dbr_string;
use crate::types::{DbFieldType, EpicsValue};
use tokio::sync::watch;

use crate::runtime::task::TaskHandle;

use super::error::{AutosaveError, AutosaveResult};
use super::save_set::{
    ListStatus, ManualSource, NUM_STATUS_PV_SETS, RestoreResult, SaveSet, SaveSetConfig,
    SaveSetStatus, SaveStrategy, SrStatus, do_manual_restore,
};
use super::settings::SrSettings;
use crate::server::database::db_access::DbSubscription;
use crate::server::recgbl::EventMask;
use std::time::{Duration, Instant};

/// Builder for constructing an AutosaveManager.
pub struct AutosaveBuilder {
    save_sets: Vec<(SaveSetConfig, SaveStrategy)>,
    global_macros: HashMap<String, String>,
    status_prefix: Option<String>,
    reboot_statuses: Vec<Arc<Mutex<ListStatus>>>,
    settings: Arc<SrSettings>,
}

impl AutosaveBuilder {
    pub fn new() -> Self {
        Self {
            save_sets: Vec::new(),
            global_macros: HashMap::new(),
            status_prefix: None,
            reboot_statuses: Vec::new(),
            settings: Arc::default(),
        }
    }

    /// One `create_*_set` call: `method` joins the set built from the same
    /// request file, if there is one.
    pub fn add_set(mut self, config: SaveSetConfig, method: SaveStrategy) -> Self {
        self.save_sets.push((config, method));
        self
    }

    pub fn macros(mut self, macros: HashMap<String, String>) -> Self {
        self.global_macros = macros;
        self
    }

    pub fn status_prefix(mut self, prefix: &str) -> Self {
        self.status_prefix = Some(prefix.to_string());
        self
    }

    /// The pass-0 then pass-1 restore files' statuses, which SR_rebootStatus
    /// reports when the save task starts.
    pub fn reboot_statuses(mut self, statuses: Vec<Arc<Mutex<ListStatus>>>) -> Self {
        self.reboot_statuses = statuses;
        self
    }

    /// The IOC's settings, which every set and the save task read.
    pub fn settings(mut self, settings: Arc<SrSettings>) -> Self {
        self.settings = settings;
        self
    }

    /// Build the manager.
    ///
    /// Infallible by construction. A save set that cannot be built is one
    /// set's fault, and there is no longer a whole-manager error for that
    /// fault to become: the `?` that used to sit here abandoned every set
    /// already constructed, so one `.req` line naming an undefined macro
    /// left the IOC saving nothing at all and `fdblist` listing nothing to
    /// say so. The failure is announced on the error log when it happens
    /// and kept as that set's [`SaveSetStatus::Error`], which
    /// [`AutosaveManager::status_all`] reports.
    pub async fn build(self) -> AutosaveManager {
        let (shutdown_tx, _) = watch::channel(false);
        let manager = AutosaveManager {
            sets: Mutex::new(Vec::new()),
            adding: tokio::sync::Mutex::new(()),
            status_prefix: self.status_prefix,
            shutdown: shutdown_tx,
            global_macros: self.global_macros,
            recent: Mutex::new(None),
            started: AtomicBool::new(false),
            reboot_statuses: self.reboot_statuses,
            settings: self.settings,
            sr_status: Mutex::new(ListStatus::new(SrStatus::Init, "")),
        };
        for (cfg, method) in self.save_sets {
            // Announced and recorded by `add_set`; nothing is left to say.
            let _ = manager.add_set(cfg, method).await;
        }
        manager
    }
}

/// A built save set and the lock that serialises its saves.
type SetEntry = (Arc<SaveSet>, Arc<tokio::sync::Mutex<()>>);

/// Manages multiple save sets with task orchestration.
///
/// The set list is not fixed at build time: C's `create_data_set`
/// (`save_restore.c`) appends to the running list at any point, so an
/// `st.cmd` that calls `create_monitor_set` after `iocInit` gets a set
/// that saves. [`Self::add_set`] is the one path that constructs a set,
/// before or after [`Self::start`]; the tasks for sets added afterwards are
/// spawned onto the runtime `start` captured.
pub struct AutosaveManager {
    sets: Mutex<Vec<SetEntry>>,
    /// Serialises [`Self::add_set`]: the lookup by request file and the
    /// push of a new set are one step, across the build's awaits.
    adding: tokio::sync::Mutex<()>,
    status_prefix: Option<String>,
    shutdown: watch::Sender<bool>,
    global_macros: HashMap<String, String>,
    /// The manager's own latest SR_recentlyStr event; see [`SaveSet::recent`].
    recent: Mutex<Option<(std::time::Instant, String)>>,
    /// Set by the [`Self::start`] that made the one save task.
    started: AtomicBool,
    reboot_statuses: Vec<Arc<Mutex<ListStatus>>>,
    settings: Arc<SrSettings>,
    /// C `SR_status` and `SR_statusStr`: the worst list status of the last
    /// pass, starting from OK (save_restore.c:903, :1043-1046).
    sr_status: Mutex<ListStatus>,
}

impl std::fmt::Debug for AutosaveManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutosaveManager")
            .field("sets", &self.set_names())
            .finish()
    }
}

impl AutosaveManager {
    /// C `create_data_set`: add `method` to the set built from the same
    /// request file, or build that set; once [`Self::start`] has run, the
    /// method's task starts too.
    ///
    /// A second method of a kind the set already has is refused with C's
    /// "already in %x mode" line. A set that cannot be built is not kept,
    /// as C frees the list `readReqFile` failed (save_restore.c:2460-2462),
    /// so a later call for the file starts over. Either error is also
    /// returned, already reported.
    pub async fn add_set(&self, cfg: SaveSetConfig, method: SaveStrategy) -> AutosaveResult<()> {
        self.add(cfg, method, false).await
    }

    /// [`Self::add_set`] for a `create_*_set` command: a method that makes
    /// the list is held to C's `MIN_PERIOD` floor, one that joins it is not.
    pub(super) async fn create_data_set(
        &self,
        cfg: SaveSetConfig,
        method: SaveStrategy,
    ) -> AutosaveResult<()> {
        self.add(cfg, method, true).await
    }

    async fn add(
        &self,
        mut cfg: SaveSetConfig,
        method: SaveStrategy,
        from_iocsh: bool,
    ) -> AutosaveResult<()> {
        let _adding = self.adding.lock().await;
        let key = cfg.key();
        let existing = self
            .sets
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| s.config().key() == key)
            .cloned();
        if let Some((set, _)) = existing {
            set.add_method(method).inspect_err(report)?;
            return Ok(());
        }
        // Merge global macros (config overrides globals)
        for (k, v) in &self.global_macros {
            cfg.macros.entry(k.clone()).or_insert_with(|| v.clone());
        }
        let method = if from_iocsh {
            method.creating_list()
        } else {
            method
        };
        let name = cfg.name.clone();
        let set = match SaveSet::new_with_settings(cfg, self.settings.clone()).await {
            Ok(set) => Arc::new(set),
            // `readReqFile` printed C's own line for this one.
            Err(e @ AutosaveError::ReqFileUnopened(_)) => return Err(e),
            Err(e) => {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "autosave: save set '{name}' not built: {e} - its PVs are not saved\n"
                ));
                return Err(e);
            }
        };
        set.add_method(method)?;
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut sets = self.sets.lock().unwrap();
        // The first slot no list holds (save_restore.c:2466-2470).
        let slot = (0..NUM_STATUS_PV_SETS)
            .find(|i| !sets.iter().any(|(s, _)| s.status_pv_index() == Some(*i)))
            .unwrap_or(NUM_STATUS_PV_SETS);
        set.set_status_pv_index(slot);
        sets.push((set, lock));
        Ok(())
    }

    /// Restore all save sets. Returns results per set.
    pub async fn restore_all(
        &self,
        db: &PvDatabase,
    ) -> Vec<(String, AutosaveResult<RestoreResult>)> {
        let mut results = Vec::new();
        for (set, _lock) in self.sets() {
            let name = set.config().name.clone();
            let result = set.restore_once(db).await;
            results.push((name, result));
        }
        results
    }

    /// Start C's save_restore task (save_restore.c:782-1260): one task
    /// serves every set's methods, so a set's events land in one
    /// `save_state` and cost it one save a pass.
    ///
    /// Takes the [`Reactor`](crate::runtime::task::Reactor), unlike every
    /// deferred record tail: autosave is started from `IocApplication::run`,
    /// which owns that runtime; it is not on the synchronous
    /// record-processing chain that reaches a runtime-less connection
    /// thread. `EXEMPT` in `server/mod.rs` is still what lets this file
    /// past `a_deferred_record_tail_never_uses_the_ambient_seam`: the pass
    /// delay is the ambient `runtime::task::sleep`.
    ///
    /// `None` once a task is running: an IOC serving CA and PVA hands the
    /// manager to both servers, and a second task would save each list
    /// again a moment after the first.
    pub fn start(
        self: Arc<Self>,
        reactor: &crate::runtime::task::Reactor,
        db: Arc<PvDatabase>,
    ) -> Option<TaskHandle<()>> {
        if self.started.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(reactor.spawn(async move {
            let debug = self.settings.debug();
            if debug > 1 {
                println!(
                    "save_restore:save_restore: entry; status_prefix='{}'",
                    self.status_prefix.as_deref().unwrap_or("")
                );
            }
            self.save_task(&db).await;
            if self.settings.debug() != 0 {
                println!("save_restore: exiting");
            }
        }))
    }

    /// The task's loop: a pass over every set, then the status PVs, at
    /// most once per `MIN_DELAY`.
    async fn save_task(&self, db: &PvDatabase) {
        let mut shutdown_rx = self.shutdown.subscribe();
        let mut lists: Vec<ListState> = Vec::new();
        let mut heartbeat = 0i16;
        // C `last_seq_check` and `lastPeriodicDatedBackup` (save_restore.c:798-799).
        let started = Instant::now();
        // C connects the general status PVs once, at the task's start
        // (save_restore.c:813).
        let general = self
            .status_prefix
            .as_deref()
            .filter(|_| self.settings.use_status_pvs());
        if let Some(prefix) = general {
            self.report_reboot(db, prefix).await;
        }
        loop {
            if is_stopped(&shutdown_rx) {
                return;
            }
            let now = Instant::now();
            self.settings.hold_seq_settings();
            // A set created since the last pass, before or after iocInit,
            // joins here, as C's `lptr` list is walked afresh each pass.
            let sets = self.sets();
            // A list `remove_data_set` took is dropped with its monitors.
            lists.retain(|l| sets.iter().any(|(s, _)| Arc::ptr_eq(&l.set, s)));
            for (set, lock) in sets {
                if !lists.iter().any(|l| Arc::ptr_eq(&l.set, &set)) {
                    lists.push(ListState::new(set, lock, started));
                }
            }
            for list in &mut lists {
                if !list.pass(db, now, &shutdown_rx, self).await {
                    return;
                }
            }
            // `<=`: of equally bad lists the later one's text wins.
            let mut worst = ListStatus::new(SrStatus::Ok, "Ok");
            for list in &lists {
                let status = list.set.list_status();
                if status.code <= worst.code {
                    worst = status;
                }
            }
            *self.sr_status.lock().unwrap() = worst.clone();
            if let Some(prefix) = &self.status_prefix {
                heartbeat = (heartbeat + 1) % 2;
                self.report_status(db, prefix, general.is_some(), &lists, worst, heartbeat)
                    .await;
            }
            tokio::select! {
                biased;
                _ = stopped(&mut shutdown_rx) => return,
                _ = crate::runtime::task::sleep(MIN_DELAY.saturating_sub(now.elapsed())) => {}
            }
        }
    }

    /// save_restore.c:854-877, once: the worst restore-file status, the first
    /// of equally bad ones.
    async fn report_reboot(&self, db: &PvDatabase, prefix: &str) {
        let mut worst = ListStatus::new(SrStatus::Ok, "Ok");
        for status in &self.reboot_statuses {
            let status = status.lock().unwrap();
            if status.code < worst.code {
                worst = status.clone();
            }
        }
        let _ = ca_put(
            db,
            &format!("{prefix}SR_rebootStatus"),
            EpicsValue::Long(worst.code as i32),
        )
        .await;
        let _ = ca_put_str(db, &format!("{prefix}SR_rebootStatusStr"), &worst.text).await;
        let time = chrono::Local::now().format("%a %b %d %H:%M:%S").to_string();
        let _ = ca_put_str(db, &format!("{prefix}SR_rebootTime"), &time).await;
    }

    /// The tail of each pass (save_restore.c:1059-1121): SR_status is
    /// `worst`, when `general`;
    /// the lists' own PVs while `save_restoreUseStatusPVs` is set (:1067).
    async fn report_status(
        &self,
        db: &PvDatabase,
        prefix: &str,
        general: bool,
        lists: &[ListState],
        worst: ListStatus,
        heartbeat: i16,
    ) {
        let statuses: Vec<_> = lists.iter().map(|l| l.set.list_status()).collect();
        if general {
            let recently = lists
                .iter()
                .filter_map(|l| l.set.recent())
                .chain(self.recent.lock().unwrap().clone())
                .max_by_key(|(at, _)| *at)
                .map(|(_, text)| text)
                .unwrap_or_default();
            let _ = ca_put(
                db,
                &format!("{prefix}SR_status"),
                EpicsValue::Long(worst.code as i32),
            )
            .await;
            let _ = ca_put(
                db,
                &format!("{prefix}SR_heartbeat"),
                EpicsValue::Short(heartbeat),
            )
            .await;
            let _ = ca_put_str(db, &format!("{prefix}SR_statusStr"), &worst.text).await;
            let _ = ca_put_str(db, &format!("{prefix}SR_recentlyStr"), &recently).await;
        }
        if !self.settings.use_status_pvs() {
            return;
        }
        for (list, status) in lists.iter().zip(statuses) {
            if let Some(index) = list.set.status_pv_index() {
                report_list(db, &format!("{prefix}SR_{index}_"), list, status).await;
            }
        }
    }

    /// Manual save for a specific set.
    pub async fn manual_save(&self, set_name: &str, db: &PvDatabase) -> AutosaveResult<usize> {
        let (set, lock) = self
            .find_set(set_name)
            .ok_or_else(|| AutosaveError::PvNotFound(format!("save set '{set_name}' not found")))?;

        let _guard = lock.lock().await;
        set.save_once(db).await
    }

    /// C `set_savefile_name(filename, save_filename)` (save_restore.c:2264-2289):
    /// the list built from request file `filename` writes `save_filename`.
    pub fn set_savefile_name(&self, filename: &str, save_filename: &str) -> i32 {
        let Some((set, _)) = self.find_set_by_request_file(filename) else {
            crate::runtime::log::errlog_vprintf(format_args!(
                "../save_restore.c:set_savefile_name No save set enabled for {filename}\n"
            ));
            return -1;
        };
        set.set_save_file(save_filename);
        self.note_recent(format!("New save file: '{save_filename}'"));
        0
    }

    /// The save task's `op_SaveFile` (save_restore.c:1207-1235): the list
    /// built from `request_file` saved as usual when `filename` is empty,
    /// else to `path` alone, then `asVerify`'d. C's status: -1 when there
    /// is no such list or nothing was written, else the verify's or the
    /// members not fetched.
    pub async fn save_file_op(
        &self,
        db: &PvDatabase,
        request_file: &str,
        filename: &str,
        path: &std::path::Path,
    ) -> i32 {
        let debug = self.settings.debug();
        if debug != 0 {
            println!("save_restore task: manual save('{filename}')");
        }
        let found = self.find_set_by_request_file(request_file);
        let (mut status, mut num_errs, mut full_path) = (-1, 0, None);
        if let Some((set, lock)) = &found {
            let _guard = lock.lock().await;
            let entries = set.collect_entries(db);
            let not_connected = entries.iter().filter(|e| !e.connected).count();
            num_errs += not_connected as i32;
            if not_connected == 0 || self.settings.incomplete_sets_ok() {
                // C sends a NULL name as "" (save_restore.c:714), so its
                // write_save_file takes the configName path: no .savB, and
                // with no .sav yet myFileCopy(save_file, "") fails and the
                // save reports -1. A C defect: "" is the list's own file here.
                let written = if filename.is_empty() {
                    let path = set.save_path();
                    set.write_entries(entries).await.map(|_| path)
                } else {
                    set.save_to(entries, path)
                        .await
                        .map(|()| path.to_path_buf())
                };
                match written {
                    Ok(path) => {
                        status = 0;
                        full_path = Some(path);
                    }
                    Err(_) => println!(
                        "save_restore: write_save_file() failed for {}",
                        set.save_file()
                    ),
                }
            }
        }
        if let Some(path) = full_path {
            status = super::verify::do_as_verify(db, &path, -1, None).await;
        }
        if debug > 1 {
            println!("save_restore: manual save status={status} (0==success)");
        }
        let name = match &found {
            Some((set, _)) if status == 0 => set.save_file(),
            None if status == 0 => "<null>".to_string(),
            _ => filename.to_string(),
        };
        self.note_recent(format!(
            "Save of '{name}' {}",
            if status != 0 { "Failed" } else { "Succeeded" }
        ));
        if status == 0 && num_errs != 0 {
            status = num_errs;
        }
        status
    }

    /// The save task's `op_RestoreFromSaveFile` / `op_RestoreFromAsciiFile`
    /// (save_restore.c:1137-1150): [`do_manual_restore`] of `path`, then
    /// SR_recentlyStr names `filename` and how it went. C's status.
    pub async fn manual_restore(
        &self,
        db: &PvDatabase,
        path: &std::path::Path,
        filename: &str,
        from: ManualSource,
        macrostring: &str,
    ) -> i32 {
        let status = do_manual_restore(db, filename, path, from, macrostring, &self.settings).await;
        self.note_recent(format!(
            "Restore of '{filename}' {}",
            if status != 0 { "Failed" } else { "Succeeded" }
        ));
        status
    }

    /// C `do_remove_data_set(filename)` (save_restore.c:2663-2728) and
    /// `op_Remove`'s SR_recentlyStr (:1162-1168): the list built from
    /// request file `filename` is no longer saved, and its status PV slot
    /// is free.
    pub fn remove_data_set(&self, filename: &str) -> i32 {
        let status = self.take_list(filename);
        let outcome = if status == 0 { "Succeeded" } else { "Failed" };
        self.note_recent(format!("Remove '{filename}' {outcome}"));
        status
    }

    /// `do_remove_data_set` alone, as `op_Reload*` calls it, with the
    /// traces every op around it prints.
    pub(super) fn take_list(&self, filename: &str) -> i32 {
        let debug = self.settings.debug();
        if debug != 0 {
            println!("save_restore task: calling do_remove_data_set('{filename}')");
        }
        let status = self.remove_list(filename);
        if debug > 1 {
            println!("save_restore: remove status={status} (0==success)");
        }
        status
    }

    fn remove_list(&self, filename: &str) -> i32 {
        let mut sets = self.sets.lock().unwrap();
        let found = sets.iter().position(|(s, _)| {
            s.config()
                .request_file
                .as_deref()
                .is_some_and(|f| f == std::path::Path::new(filename))
        });
        match found {
            Some(i) => {
                sets.remove(i);
                0
            }
            None => {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "../save_restore.c:do_remove_data_set Couldn't find '{filename}'\n"
                ));
                -1
            }
        }
    }

    pub(super) fn note_recent(&self, text: String) {
        *self.recent.lock().unwrap() = Some((std::time::Instant::now(), text));
    }

    /// Get status of all save sets.
    pub async fn status_all(&self) -> Vec<(String, SaveSetStatus)> {
        let mut results = Vec::new();
        for (set, _) in self.sets() {
            results.push((set.config().name.clone(), set.status().await));
        }
        results
    }

    /// Send shutdown signal to all tasks.
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Get the list of save set names.
    pub fn set_names(&self) -> Vec<String> {
        self.sets
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.config().name.clone())
            .collect()
    }

    /// Find a save set by name.
    fn find_set(&self, name: &str) -> Option<SetEntry> {
        self.sets
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| s.config().name == name)
            .map(|(s, l)| (s.clone(), l.clone()))
    }

    /// The set built from `request_file`, as C matches `plist->reqFile`
    /// (`getMacroString`, `op_SaveFile`).
    pub fn find_set_by_request_file(&self, request_file: &str) -> Option<SetEntry> {
        self.sets
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| {
                s.config()
                    .request_file
                    .as_deref()
                    .is_some_and(|f| f == std::path::Path::new(request_file))
            })
            .map(|(s, l)| (s.clone(), l.clone()))
    }

    /// A snapshot of the built sets.
    pub fn sets(&self) -> Vec<SetEntry> {
        self.sets.lock().unwrap().clone()
    }

    /// C `SR_status` and `SR_statusStr`.
    pub fn sr_status(&self) -> ListStatus {
        self.sr_status.lock().unwrap().clone()
    }

    /// The lists' part of C `save_restoreShow` (save_restore.c:2522-2572),
    /// newest first as C's `lptr` is. A member's line has no `chid`, and its
    /// value is the database's now rather than the last `ca_get`'s.
    pub fn show_lists(&self, db: &PvDatabase, ctx: &CommandContext, verbose: i32) {
        const TIMEFMT: &str = "%a %b %d %H:%M:%S %Y";
        // C `MIN_PERIOD`, the floor `create_data_set` leaves on a period no
        // method set.
        const MIN_PERIOD: u64 = 4;
        for (set, _) in self.sets().iter().rev() {
            let cfg = set.config();
            let status = set.list_status();
            let method = set.save_method();
            let (save_state, not_connected) = set.task_view();
            let status_pv = match (&self.status_prefix, set.status_pv_index()) {
                (Some(prefix), Some(index)) if self.settings.use_status_pvs() => {
                    format!("{prefix}SR_{index}_Status")
                }
                _ => String::new(),
            };
            let mut methods = String::from("[ ");
            for (bit, name) in [
                (SaveStrategy::PERIODIC, "PERIODIC "),
                (SaveStrategy::TRIGGERED, "TRIGGERED "),
                (SaveStrategy::MONITORED, "TIMER+CHANGE "),
                (SaveStrategy::MANUAL, "MANUAL "),
            ] {
                if method & bit == bit {
                    methods.push_str(name);
                }
            }
            methods.push(']');
            let period = if method & SaveStrategy::PERIODIC != 0 {
                set.period().as_secs()
            } else {
                MIN_PERIOD
            };
            let monitor_period = if method & SaveStrategy::MONITORED == SaveStrategy::MONITORED {
                set.monitor_period().as_secs()
            } else {
                MIN_PERIOD
            };
            ctx.print_fmt(format_args!("  {}: ", cfg.key()));
            ctx.print_fmt(format_args!("    macro string: '{}'", cfg.macro_string));
            ctx.print_fmt(format_args!("    Status PV: {status_pv}"));
            ctx.print_fmt(format_args!(
                "    Status: '{}' - '{}'",
                status.code.label(),
                status.text
            ));
            ctx.print_fmt(format_args!(
                "    Last save time  :{}",
                set.save_time().format(TIMEFMT)
            ));
            ctx.print_fmt(format_args!(
                "    Last backup time:{}",
                set.backup_time().format(TIMEFMT)
            ));
            ctx.print_fmt(format_args!("    methods: {methods}"));
            ctx.print_fmt(format_args!("    path PV: None"));
            ctx.print_fmt(format_args!("    name PV: None"));
            ctx.print_fmt(format_args!(
                "    backups: {}",
                if cfg.backup.enable_savb { "YES" } else { "NO" }
            ));
            ctx.print_fmt(format_args!("    save_state = 0x{save_state:x}"));
            ctx.print_fmt(format_args!(
                "    period: {period}; trigger chan: '{}'; monitor period: {monitor_period}",
                set.trigger_channel().unwrap_or_default()
            ));
            ctx.print_fmt(format_args!(
                "    last saved file - {}",
                set.last_save_file()
            ));
            ctx.print_fmt(format_args!(
                "    {not_connected} channel{} not connected (or ca_get failed)",
                if not_connected == 1 { ' ' } else { 's' }
            ));
            if verbose != 0 && !*self.shutdown.borrow() {
                for name in set.pv_names() {
                    show_member(db, ctx, &name);
                }
            }
        }
    }
}

/// A list member's `save_restoreShow` line (save_restore.c:2564-2570).
fn show_member(db: &PvDatabase, ctx: &CommandContext, name: &str) {
    let Ok(live) = db.get_pv(name) else {
        ctx.print_fmt(format_args!(
            "\t{name} state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0"
        ));
        return;
    };
    let max = db
        .restore_target(name)
        .map(|t| t.elements as usize)
        .unwrap_or(live.count() as usize);
    // `get_channel_values`' one element (save_restore.c:1562-1625).
    let native = live.dbr_type();
    let value = if matches!(native, DbFieldType::Float | DbFieldType::Double) {
        let first = match live.convert_to(DbFieldType::Double) {
            EpicsValue::Double(d) => d,
            EpicsValue::DoubleArray(a) => a.first().copied().unwrap_or_default(),
            _ => 0.0,
        };
        if native == DbFieldType::Float {
            fmt_g(first as f32 as f64, 7, false, false)
        } else {
            fmt_g(first, 14, false, false)
        }
    } else if live.is_array() {
        match live.get_convert(DbFieldType::String) {
            Ok(EpicsValue::StringArray(a)) => a.first().map(|s| s.to_string()).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        // `ca_get(DBR_STRING)`: a menu field as its choice.
        let (base, field) = crate::server::database::parse_pv_name(name);
        match db.get_record(base) {
            Some(rec) => rec
                .read()
                .source_render_of(field, &live)
                .dbr_string(live.clone()),
            None => value_as_dbr_string(&live),
        }
        .map(|s| s.to_string())
        .unwrap_or_default()
    };
    let enum_val = match live {
        EpicsValue::Enum(i) => format!("\t{i}"),
        _ => String::new(),
    };
    ctx.print_fmt(format_args!(
        "\t{name} state:Conn (max:{max} curr:{} elements)\t{value}   channel_connected = 1{enum_val}",
        live.count()
    ));
}

/// Print a refusal [`AutosaveManager::add_set`] returns as C's errlog line.
fn report(e: &AutosaveError) {
    crate::runtime::log::errlog_vprintf(format_args!("{e}\n"));
}

/// Resolve once `shutdown` has been called, or the manager is gone. A task
/// subscribes when it starts, which can be after `shutdown`; a late receiver
/// has already seen `true`, so this cannot be `changed()`, and `shutdown`
/// must `send_replace`, since `send` stores nothing while no task listens.
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// Whether the manager has been shut down or dropped, without waiting.
///
/// Read immediately before a save starts: the loops' `select!` only sees a
/// stop between passes, and a pass can sit on the set lock behind a slow
/// save while the manager goes away. Once this is true no save may start.
pub(super) fn is_stopped(rx: &watch::Receiver<bool>) -> bool {
    *rx.borrow() || rx.has_changed().is_err()
}

/// One list's status PVs, as save_restore.c:1109-1121 writes them.
async fn report_list(db: &PvDatabase, pv: &str, list: &ListState, status: ListStatus) {
    let set = &list.set;
    let _ = ca_put(
        db,
        &format!("{pv}Status"),
        EpicsValue::Long(status.code as i32),
    )
    .await;
    // `save_file` cut to a DBR_STRING, then at its last '.'.
    let mut name: String = set.save_file().chars().take(39).collect();
    if let Some(dot) = name.rfind('.') {
        name.truncate(dot);
    }
    let _ = ca_put_str(db, &format!("{pv}Name"), &name).await;
    // C's `save_state` once the pass has cleared the events it served.
    let _ = ca_put(
        db,
        &format!("{pv}State"),
        EpicsValue::Long(list.save_state as i32),
    )
    .await;
    let _ = ca_put_str(db, &format!("{pv}StatusStr"), &status.text).await;
    if status.code >= SrStatus::Warn {
        let time = set.save_time().format("%a %b %d %H:%M:%S").to_string();
        let _ = ca_put_str(db, &format!("{pv}Time"), &time).await;
    }
}

/// C autosave's `ca_put` on a status PV, which rsrv serves as `dbPutField`
/// (save_restore.c:1061-1062): the record processes, so an mbbo status takes
/// its state alarm and a heartbeat bo fires its FLNK.
async fn ca_put(db: &PvDatabase, name: &str, value: EpicsValue) -> crate::error::CaResult<()> {
    let (record, field) = crate::server::database::parse_pv_name(name);
    db.put_record_field_from_ca_no_notify(record, field, value)
        .await
}

/// [`ca_put`] of C's `DBR_STRING` from a `STATUS_STR_LEN` buffer: the record
/// keeps the first 39 characters.
async fn ca_put_str(db: &PvDatabase, name: &str, text: &str) -> crate::error::CaResult<()> {
    let text: String = text.chars().take(39).collect();
    ca_put(db, name, EpicsValue::String(text.into())).await
}

/// C `MIN_DELAY` (save_restore.c:395): the save task makes at most one
/// pass a second.
const MIN_DELAY: Duration = Duration::from_secs(1);

/// C `CA_RECONNECT_TIME_SECONDS` (save_restore.c:380).
const CA_RECONNECT_TIME: Duration = Duration::from_secs(60);
/// C `save_state` bits (save_restore.c:205-212): MONITORED is
/// `TIMER | CHANGE`, and a save clears the SINGLE_EVENTS.
const TIMER: u32 = 0x04;
const CHANGE: u32 = 0x08;
const SINGLE_EVENTS: u32 = SaveStrategy::PERIODIC | SaveStrategy::TRIGGERED | SaveStrategy::MANUAL;
/// `ca_add_event`'s mask 0: DBE_VALUE | DBE_ALARM.
const CA_DEFAULT_MASK: u16 = EventMask::VALUE.bits() | EventMask::ALARM.bits();

/// The save task's half of C's `chlist`. Only the task holds it: each
/// method's timer or monitor sets a bit in `save_state`, and the pass
/// is the one place a bit becomes a save.
struct ListState {
    set: Arc<SaveSet>,
    lock: Arc<tokio::sync::Mutex<()>>,
    /// C `enabled_method`.
    enabled: u32,
    /// C `save_state`.
    save_state: u32,
    /// The pending `periodicCb` / `monitorCb`.
    periodic_due: Option<Instant>,
    monitor_due: Option<Instant>,
    callback_time: Instant,
    save_attempt_time: Instant,
    /// `triggered_save`'s and `on_change_save`'s monitors.
    trigger: Option<DbSubscription>,
    members: Vec<DbSubscription>,
    /// C `last_seq_check`.
    last_seq_check: Instant,
    /// C `lastPeriodicDatedBackup`, one per list: C keeps one for the task
    /// and resets it inside the per-list loop (save_restore.c:1020), so in
    /// each period only the first list with backups got its dated backup.
    last_dated: Instant,
    /// The members C's `channel_connected` is set for.
    connected: HashSet<String>,
    /// C `plist->not_connected`.
    not_connected: usize,
    /// C `plist->reconnect_check_time`.
    reconnect_check_time: Instant,
}

impl ListState {
    fn new(set: Arc<SaveSet>, lock: Arc<tokio::sync::Mutex<()>>, task_started: Instant) -> Self {
        let created = set.created_at();
        Self {
            set,
            lock,
            enabled: 0,
            save_state: 0,
            periodic_due: None,
            monitor_due: None,
            callback_time: created,
            save_attempt_time: created,
            trigger: None,
            members: Vec::new(),
            last_seq_check: task_started,
            last_dated: task_started,
            connected: HashSet::new(),
            not_connected: 0,
            reconnect_check_time: task_started,
        }
    }

    /// One list's share of a pass (save_restore.c:929-1047). `false` once
    /// the manager has stopped: no save starts after that.
    async fn pass(
        &mut self,
        db: &PvDatabase,
        now: Instant,
        stop: &watch::Receiver<bool>,
        mgr: &AutosaveManager,
    ) -> bool {
        let method = self.set.save_method();
        // C `do_seq_check`, every half sequence period (save_restore.c:908).
        let settings = self.set.settings().clone();
        if settings.debug() >= 30 {
            println!(
                "save_restore: '{}' save_state = 0x{:x}",
                self.set.config().key(),
                self.save_state
            );
        }
        let half = Duration::from_secs(settings.seq_period().as_secs() / 2);
        let seq_check = now.duration_since(self.last_seq_check) > half;
        if seq_check {
            self.last_seq_check = now;
        }
        if self.enabled == 0 {
            self.connect(db, mgr, true);
            self.reconnect_check_time = now;
        } else if settings.ca_reconnect()
            && self.not_connected > 0
            && now.duration_since(self.reconnect_check_time) > CA_RECONNECT_TIME
        {
            // save_restore.c:958-963
            self.reconnect_check_time = now;
            self.connect(db, mgr, false);
        }
        if self.enabled != method {
            self.enable(db, now, mgr).await;
        }
        self.take_events(now);

        if (method & SaveStrategy::PERIODIC != 0
            || method & SaveStrategy::MONITORED == SaveStrategy::MONITORED)
            && settings
                .callback_timeout()
                .is_some_and(|t| now.duration_since(self.callback_time) > t)
        {
            self.save_state = method;
            if settings.debug() > 1 {
                println!(
                    "save_restore: Callback time out of {}, force to save!",
                    self.set.config().key()
                );
            }
        }

        let save_needed = self.save_state & SINGLE_EVENTS != 0
            || self.save_state & SaveStrategy::MONITORED == SaveStrategy::MONITORED
            || (self.set.list_status().code <= SrStatus::Fail
                && now.duration_since(self.save_attempt_time) > settings.retry());
        if save_needed {
            let lock = self.lock.clone();
            let _guard = lock.lock().await;
            if is_stopped(stop) {
                return false;
            }
            self.save_attempt_time = now;
            let entries = self.set.collect_entries(db);
            self.not_connected = entries.iter().filter(|e| !e.connected).count();
            if self.not_connected == 0 || settings.incomplete_sets_ok() {
                let _ = self.set.write_entries(entries).await;
            }
            // Monitor events delivered while the values were fetched are
            // cleared with the rest, as C's preemptive callbacks are.
            self.take_events(now);
        }

        if seq_check && self.set.list_status().code > SrStatus::Fail && self.set.seq_due(now) {
            let lock = self.lock.clone();
            let _guard = lock.lock().await;
            if is_stopped(stop) {
                return false;
            }
            self.set.do_seq(db).await;
        }

        if settings
            .periodic_dated_backup()
            .is_some_and(|period| now.duration_since(self.last_dated) > period)
        {
            let lock = self.lock.clone();
            let _guard = lock.lock().await;
            if is_stopped(stop) {
                return false;
            }
            self.set.do_periodic_dated_backup(db).await;
            self.last_dated = now;
        }

        if self.save_state & SaveStrategy::PERIODIC != 0 {
            self.periodic_due = Some(now + self.set.period());
            self.callback_time = now;
        }
        self.save_state &= !SINGLE_EVENTS;
        if self.save_state & SaveStrategy::MONITORED == SaveStrategy::MONITORED {
            self.monitor_due = Some(now + self.set.monitor_period());
            self.save_state &= !SaveStrategy::MONITORED;
            self.callback_time = now;
        }
        self.set
            .publish_task_view(self.save_state, self.not_connected);
        true
    }

    /// C `connect_list(plist, verbose)` (save_restore.c:1276-1374): a search
    /// for each member not yet connected. Members are database names, so a
    /// member is connected when it resolves, as [`SaveSet::save_once`]
    /// reads it.
    fn connect(&mut self, db: &PvDatabase, mgr: &AutosaveManager, verbose: bool) {
        self.set.set_status_text("Connecting PVs...");
        let names = self.set.pv_names();
        let debug = self.set.settings().debug();
        if debug >= 10 {
            for name in &names {
                println!("save_restore:connect_list: channel '{name}'");
            }
        }
        let searched: Vec<&String> = names
            .iter()
            .filter(|n| !self.connected.contains(n.as_str()))
            .collect();
        // `ca_create_channel` makes the server's `dbChannelCreate`, which
        // prints its own refusal once per create.
        let (found, failed): (Vec<&String>, Vec<&String>) = searched.iter().partition(|n| {
            crate::server::database::filters::db_channel_create(db, n.as_str()).is_ok()
        });
        if !failed.is_empty() {
            println!("save_restore:connect_list: not all searches successful");
            if verbose {
                for name in &failed {
                    println!("save_restore: connect failed for channel '{name}'");
                }
            }
        }
        if debug >= 10 {
            // C `ca_element_count`, 0 for a channel that did not connect.
            for name in &searched {
                let count = db.restore_target(name).map_or(0, |t| t.elements);
                println!("save_restore:connect_list: '{name}' has, at most, {count} elements");
                if count > 1 {
                    println!(
                        "save_restore:connect_list:(after SR_get_array_info) '{name}' has, at most, {count} elements"
                    );
                }
            }
        }
        let line = format!(
            "{}: {} of {} PV's connected",
            self.set.save_file(),
            found.len(),
            searched.len()
        );
        if verbose {
            println!("{line}");
        }
        mgr.note_recent(line);
        self.connected.extend(found.into_iter().cloned());
        self.not_connected = names
            .iter()
            .filter(|n| super::save_set::read_member(db, n).is_none())
            .count();
    }

    /// C `enable_list` (save_restore.c:1425-1487): start what each method
    /// not yet enabled needs. A trigger that cannot be monitored stays
    /// disabled, and the next pass tries again.
    async fn enable(&mut self, db: &PvDatabase, now: Instant, mgr: &AutosaveManager) {
        let method = self.set.save_method();
        let debug = self.set.settings().debug();
        if debug >= 4 {
            println!("save_restore:enable_list: entry");
        }
        self.set.set_status_text("Enabling list...");
        if method & SaveStrategy::PERIODIC != 0 && self.enabled & SaveStrategy::PERIODIC == 0 {
            self.periodic_due = Some(now + self.set.period());
            self.enabled |= SaveStrategy::PERIODIC;
            self.callback_time = now;
        }
        if method & SaveStrategy::TRIGGERED != 0 && self.enabled & SaveStrategy::TRIGGERED == 0 {
            if let Some(name) = self.set.trigger_channel() {
                // A name no record answers is C's `ca_pend_io` timeout.
                match DbSubscription::subscribe_with_mask(db, &name, 0, CA_DEFAULT_MASK).await {
                    Ok(sub) => {
                        // `ca_add_event`'s first callback carries the
                        // current value.
                        sub.activation_handle().enable(db).await;
                        self.trigger = Some(sub);
                        self.enabled |= SaveStrategy::TRIGGERED;
                    }
                    Err(_) => {
                        println!("save_restore:enable_list: timeout on search of {name}");
                    }
                }
            }
        }
        if method & SaveStrategy::MONITORED != 0 && self.enabled & SaveStrategy::MONITORED == 0 {
            for name in self.set.pv_names() {
                if debug >= 10 {
                    println!("save_restore:enable_list: calling ca_add_event for '{name}'");
                    println!(
                        "save_restore:enable_list: arg = {:p}",
                        Arc::as_ptr(&self.set)
                    );
                }
                // `ca_add_event` goes on the channel `connect_list` made, so
                // it creates none; one C refused never raises an event.
                if !self.connected.contains(&name) {
                    continue;
                }
                if let Ok(sub) =
                    DbSubscription::subscribe_with_mask(db, &name, 0, CA_DEFAULT_MASK).await
                {
                    sub.activation_handle().enable(db).await;
                    self.members.push(sub);
                }
            }
            if debug >= 4 {
                println!("save_restore:enable_list: done calling ca_add_event for list channels");
            }
            self.monitor_due = Some(now + self.set.monitor_period());
            self.enabled |= SaveStrategy::MONITORED;
            self.callback_time = now;
        }
        if method & SaveStrategy::MANUAL != 0 {
            self.enabled |= SaveStrategy::MANUAL;
        }
        mgr.note_recent(format!("list '{}' enabled", self.set.save_file()));
    }

    /// The callbacks since the last look: `triggered_save` on a trigger
    /// event that carries a value, `on_change_save` on any member event,
    /// and the expired timers.
    fn take_events(&mut self, now: Instant) {
        if let Some(trigger) = &mut self.trigger {
            while let Ok(event) = trigger.try_recv_event() {
                if event.value().as_value().is_some() {
                    self.save_state |= SaveStrategy::TRIGGERED;
                }
            }
        }
        let debug = self.set.settings().debug();
        for member in &mut self.members {
            while member.try_recv_event().is_ok() {
                if debug >= 10 {
                    crate::runtime::log::errlog_vprintf(format_args!(
                        "on_change_save: event.usr=0x{:x}\n",
                        Arc::as_ptr(&self.set) as usize
                    ));
                }
                self.save_state |= CHANGE;
            }
        }
        if self.periodic_due.is_some_and(|due| due <= now) {
            self.periodic_due = None;
            self.save_state |= SaveStrategy::PERIODIC;
        }
        if self.monitor_due.is_some_and(|due| due <= now) {
            self.monitor_due = None;
            if debug >= 10 {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "on_change_timer for {} (period is {} seconds)\n",
                    self.set.config().key(),
                    self.set.monitor_period().as_secs()
                ));
            }
            self.save_state |= TIMER;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::autosave::backup::BackupConfig;
    use crate::server::records::ao::AoRecord;

    /// The list's SR_recentlyStr and statusStr after a pass at `start`, a
    /// record `B` loaded, and passes at `start + secs` for each of `later`.
    async fn after_passes(ca_reconnect: i32, later: &[u64]) -> Vec<(String, String)> {
        let db = PvDatabase::new();
        db.add_record("A", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        let settings = Arc::new(SrSettings::default());
        settings.set_ca_reconnect(ca_reconnect);
        // No retry save, which would read `B` first.
        settings.set_retry_seconds(1000);
        let dir = tempfile::tempdir().unwrap();
        let mgr = AutosaveBuilder::new()
            .settings(settings)
            .add_set(
                SaveSetConfig {
                    name: "s".into(),
                    save_path: dir.path().join("s.sav"),
                    request_file: None,
                    request_pvs: vec!["A".into(), "B".into()],
                    backup: BackupConfig { enable_savb: false },
                    macros: HashMap::new(),
                    macro_string: String::new(),
                    search_paths: Vec::new(),
                },
                SaveStrategy::Manual,
            )
            .build()
            .await;
        let (set, lock) = mgr.sets().remove(0);
        let start = Instant::now();
        let mut list = ListState::new(set.clone(), lock, start);
        let (_tx, stop) = watch::channel(false);
        let read = || {
            (
                mgr.recent.lock().unwrap().clone().unwrap().1,
                set.list_status().text,
            )
        };
        assert!(list.pass(&db, start, &stop, &mgr).await);
        let mut out = vec![read()];
        db.add_record("B", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        for secs in later {
            assert!(
                list.pass(&db, start + Duration::from_secs(*secs), &stop, &mgr)
                    .await
            );
            out.push(read());
        }
        out
    }

    fn row(recent: &str, status: &str) -> (String, String) {
        (recent.into(), status.into())
    }

    // RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --lib server::autosave::manager::tests` (2/2).
    /// save_restore.c:958-963: a search for the channel that failed, once
    /// CA_RECONNECT_TIME_SECONDS has passed, and not again once it is
    /// connected: a search at 200 s would read "0 of 0".
    #[tokio::test]
    async fn a_failed_member_is_searched_again_after_a_minute() {
        let enabled = row("list 's.sav' enabled", "Enabling list...");
        assert_eq!(
            after_passes(1, &[60, 61, 200]).await,
            [
                enabled.clone(),
                enabled,
                row("s.sav: 1 of 1 PV's connected", "Connecting PVs..."),
                row("s.sav: 1 of 1 PV's connected", "Connecting PVs..."),
            ]
        );
    }

    #[tokio::test]
    async fn without_ca_reconnect_it_is_not() {
        assert_eq!(
            after_passes(0, &[61]).await,
            [
                row("list 's.sav' enabled", "Enabling list..."),
                row("list 's.sav' enabled", "Enabling list..."),
            ]
        );
    }
}
