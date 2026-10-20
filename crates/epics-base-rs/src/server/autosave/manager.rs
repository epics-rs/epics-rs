use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::server::database::PvDatabase;
use crate::server::record::record_instance::value_as_dbr_string;
use crate::types::{DbFieldType, EpicsValue};
use epics_libcom_rs::cfmt::CFloat;
use tokio::sync::watch;

use super::error::{AutosaveError, AutosaveResult};
use super::save_set::{
    CaContext, Channel, ListStatus, ManualSource, NUM_STATUS_PV_SETS, RestoreResult, SAVENAMEPV,
    SAVEPATHPV, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, SrStatus, do_manual_restore,
    file_pv_value,
};
use super::settings::SrSettings;
use crate::server::database::db_access::{DbSubscription, SubscriptionActivation};
use crate::server::recgbl::EventMask;
use std::time::{Duration, Instant};

/// The running save_restore thread: awaiting it waits for the thread to
/// end; dropping it leaves the thread running.
#[derive(Debug)]
pub struct SaveTaskHandle {
    mgr: std::sync::Weak<AutosaveManager>,
    done: tokio::sync::oneshot::Receiver<()>,
}

impl SaveTaskHandle {
    /// Stop the task before any save it has not yet started: the flag is
    /// set before this returns, and a pass checks it after taking a set's
    /// lock.
    pub fn abort(&self) {
        if let Some(mgr) = self.mgr.upgrade() {
            mgr.shutdown();
        }
    }
}

impl std::future::Future for SaveTaskHandle {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        std::pin::Pin::new(&mut self.done).poll(cx).map(|_| ())
    }
}

/// Builder for constructing an AutosaveManager.
pub struct AutosaveBuilder {
    save_sets: Vec<(SaveSetConfig, SaveStrategy)>,
    global_macros: HashMap<String, String>,
    status_prefix: Option<String>,
    reboot_statuses: Vec<Arc<Mutex<ListStatus>>>,
    settings: Arc<SrSettings>,
    ops: Option<Arc<OpQueue>>,
}

impl AutosaveBuilder {
    pub fn new() -> Self {
        Self {
            save_sets: Vec::new(),
            global_macros: HashMap::new(),
            status_prefix: None,
            reboot_statuses: Vec::new(),
            settings: Arc::default(),
            ops: None,
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

    /// The queue the `create_*_set` commands opened, which the shell's ops
    /// were sent to before this manager's task existed.
    pub(super) fn op_queue(mut self, ops: Arc<OpQueue>) -> Self {
        self.ops = Some(ops);
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
        // A manager built with its sets has the task C's first
        // create_data_set made, and so its queue.
        let ops = self.ops.unwrap_or_else(|| {
            let ops = Arc::new(OpQueue::default());
            ops.open();
            ops
        });
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
            startup: std::sync::OnceLock::new(),
            ops,
            ca: std::sync::OnceLock::new(),
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
    /// The startup config `iocInit` built this manager from, which holds
    /// what the task's exit `save_restoreShow` prints.
    startup: std::sync::OnceLock<std::sync::Weak<Mutex<super::AutosaveStartupConfig>>>,
    /// C `opMsgQueue`, which the task takes the receiving end of.
    ops: Arc<OpQueue>,
    /// The task's CA context, recorded once; see [`Self::ca_context`].
    ca: std::sync::OnceLock<CaContext>,
}

/// C `opMsgQueue`: every op the shell asks the save task for, served in the
/// order sent between the task's passes (save_restore.c:1131-1257). C
/// creates it, with the task, at the first `create_*_set` (:2338-2351),
/// even one with no file; until then it is NULL.
pub struct OpQueue {
    opened: AtomicBool,
    tx: tokio::sync::mpsc::UnboundedSender<TaskOp>,
    rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<TaskOp>>>,
}

impl Default for OpQueue {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            opened: AtomicBool::new(false),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }
}

impl std::fmt::Debug for OpQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpQueue")
            .field("opened", &self.is_open())
            .finish()
    }
}

impl OpQueue {
    /// C `save_restore_init`'s `epicsMessageQueueCreate`.
    pub(super) fn open(&self) {
        self.opened.store(true, Ordering::Release);
    }

    /// Not C's NULL `opMsgQueue`.
    pub(super) fn is_open(&self) -> bool {
        self.opened.load(Ordering::Acquire)
    }

    /// `epicsMessageQueueSend`: `false`, and `op` dropped, for C's NULL
    /// queue.
    pub(super) fn send(&self, op: TaskOp) -> bool {
        self.is_open() && self.tx.send(op).is_ok()
    }
}

/// An `op_msg`'s `callbackFunction` and `puserPvt`.
pub(super) type OpCallback = Box<dyn FnOnce(i32) + Send>;

/// C `defaultCallback` (save_restore.c:2820), for a sender that gave none.
pub(super) fn default_callback() -> OpCallback {
    Box::new(|status| crate::epics_println!("save_restore:defaultCallback:status={status}"))
}

/// An `op_msg` the save task serves between passes (save_restore.c:1131-1251).
pub(super) enum TaskOp {
    /// `op_RestoreFromSaveFile` and `op_RestoreFromAsciiFile`: `path` is
    /// `filename` under the save path when relative. Only the ASCII one
    /// calls `callback` (:1144-1159).
    Restore {
        filename: String,
        path: PathBuf,
        from: ManualSource,
        macrostring: String,
        callback: OpCallback,
    },
    /// `op_Remove`.
    Remove { filename: String },
    /// The four `op_Reload*Set`s: `config` made again with `method`, `None`
    /// being `create_triggered_set`'s refusal of its trigger channel.
    Reload {
        filename: String,
        config: SaveSetConfig,
        method: Option<SaveStrategy>,
    },
    /// `op_SaveFile`: `filename` under `path`, or the list's own file
    /// when empty.
    Save {
        request_file: String,
        filename: String,
        path: PathBuf,
        callback: OpCallback,
    },
    /// `op_asVerify`: `path` is `filename` under the save path when relative.
    AsVerify {
        filename: String,
        path: PathBuf,
        verbose: i32,
        restore: Option<PathBuf>,
    },
    /// Not C's: wakes [`AutosaveManager::ops_served`] once the ops sent
    /// before it are served.
    Served(tokio::sync::oneshot::Sender<()>),
}

impl TaskOp {
    /// The task's case for the op (save_restore.c:1137-1248).
    async fn run(self, db: &PvDatabase, mgr: &AutosaveManager) {
        let settings = &mgr.settings;
        let debug = settings.debug();
        match self {
            TaskOp::Restore {
                filename,
                path,
                from,
                macrostring,
                callback,
            } => {
                if debug != 0 {
                    crate::epics_println!(
                        "save_restore task: calling do_manual_restore('{filename}')"
                    );
                }
                let mut status = mgr
                    .manual_restore(db, &path, &filename, from, &macrostring)
                    .await;
                if debug > 1 {
                    crate::epics_println!(
                        "save_restore: manual restore status={status} (0==success)"
                    );
                }
                if from == ManualSource::AsciiFile {
                    if status == 0 {
                        if !Path::new(&filename).is_absolute() {
                            super::request::concatenated(&path.to_string_lossy(), settings);
                        }
                        status = super::verify::do_as_verify(db, &path, -1, None).await;
                    }
                    callback(status);
                }
            }
            TaskOp::Remove { filename } => {
                mgr.remove_data_set(&filename);
            }
            TaskOp::Reload {
                filename,
                config,
                method,
            } => {
                let mut status = mgr.take_list(&filename);
                if status == 0 {
                    status = match method {
                        Some(method) => {
                            super::startup::trace_create(settings, &filename, &method);
                            match mgr.create_data_set(config, method).await {
                                Ok(()) => 0,
                                Err(_) => -1,
                            }
                        }
                        None => {
                            crate::runtime::log::errlog_vprintf(format_args!(
                                "save_restore:create_triggered_set: Error: trigger-channel name is required.\n"
                            ));
                            -1
                        }
                    };
                }
                let outcome = if status == 0 { "Succeeded" } else { "Failed" };
                mgr.note_recent(format!("Reload '{filename}' {outcome}"));
            }
            TaskOp::Save {
                request_file,
                filename,
                path,
                callback,
            } => {
                let status = mgr.save_file_op(db, &request_file, &filename, &path).await;
                callback(status);
            }
            TaskOp::AsVerify {
                filename,
                path,
                verbose,
                restore,
            } => {
                if debug != 0 {
                    crate::epics_println!("save_restore task: calling do_asVerify('{filename}')");
                }
                if !Path::new(&filename).is_absolute() {
                    super::request::concatenated(&path.to_string_lossy(), settings);
                }
                super::verify::do_as_verify(db, &path, verbose, restore.as_deref()).await;
            }
            TaskOp::Served(done) => {
                let _ = done.send(());
            }
        }
    }
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
        {
            // save_restore.c:2365-2404.
            let _list_lock = lock_list(&self.settings);
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
        // save_restore.c:2476-2482.
        let _list_lock = lock_list(&self.settings);
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
    /// The task runs on its own `save_restore` thread at `taskPriority`,
    /// as C's does (:2345). `EXEMPT` in `server/mod.rs` is what lets this
    /// file past `a_deferred_record_tail_never_uses_the_ambient_seam`: the
    /// pass delay is the ambient `runtime::task::sleep`.
    ///
    /// `None` once a task is running: an IOC serving CA and PVA hands the
    /// manager to both servers, and a second task would save each list
    /// again a moment after the first.
    pub fn start(self: Arc<Self>, db: Arc<PvDatabase>) -> Option<SaveTaskHandle> {
        use crate::runtime::task::{
            StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread,
        };
        if self.started.swap(true, Ordering::AcqRel) {
            return None;
        }
        // C's task makes its context as it starts (save_restore.c:802).
        self.ca_context(&db);
        // C's own epicsThread (save_restore.c:2345).
        let (done_tx, done) = tokio::sync::oneshot::channel::<()>();
        let mgr = self.clone();
        let spawned = spawn_dedicated_thread(
            "save_restore".into(),
            ThreadPriority::Custom(self.settings.task_priority()),
            StackSizeClass::Big,
            move || {
                let _ = block_on_sync(mgr.run_task(&db));
                drop(done_tx);
            },
        );
        if spawned.is_err() {
            crate::runtime::log::errlog_vprintf(format_args!(
                "../save_restore.c:create_data_set could not create save_restore task\n"
            ));
            return None;
        }
        Some(SaveTaskHandle {
            mgr: Arc::downgrade(&self),
            done,
        })
    }

    async fn run_task(&self, db: &PvDatabase) {
        if self.settings.debug() > 1 {
            crate::epics_println!(
                "save_restore:save_restore: entry; status_prefix='{}'",
                self.status_prefix.as_deref().unwrap_or("")
            );
        }
        self.save_task(db).await;
        if self.settings.debug() != 0 {
            // save_restore.c:1263-1266.
            if let Some(cfg) = self.startup.get().and_then(|w| w.upgrade()) {
                cfg.lock()
                    .unwrap()
                    .save_restore_show(db, &|a| crate::epics_println!("{a}"), 1);
            }
            crate::epics_println!("save_restore: exiting");
        }
    }

    /// The task's loop: a pass over every set, then the status PVs, at
    /// most once per `MIN_DELAY`.
    async fn save_task(&self, db: &PvDatabase) {
        let ca = self.ca_context(db);
        let mut shutdown_rx = self.shutdown.subscribe();
        let Some(mut ops) = self.ops.rx.lock().unwrap().take() else {
            return;
        };
        let mut lists: Vec<ListState> = Vec::new();
        let mut heartbeat = 0i16;
        // C `last_seq_check` and `lastPeriodicDatedBackup` (save_restore.c:798-799).
        let started = Instant::now();
        let mut clock = TaskClock::new(started);
        // C connects the general status PVs once, at the task's start
        // (save_restore.c:813).
        let general = self
            .status_prefix
            .as_deref()
            .filter(|_| self.settings.use_status_pvs());
        if let Some(prefix) = general {
            // save_restore.c:812-851: `ca_pend_io(0.5)` fails unless all
            // nine connect.
            let names = [
                "SR_status",
                "SR_heartbeat",
                "SR_statusStr",
                "SR_recentlyStr",
                "SR_rebootStatus",
                "SR_rebootStatusStr",
                "SR_rebootTime",
                "SR_disable",
                "SR_disableMaxSecs",
            ];
            if names
                .iter()
                .any(|n| ca_get(db, &format!("{prefix}{n}")).is_err())
            {
                crate::epics_println!("save_restore: Can't connect to all status PV(s)");
            }
            self.report_reboot(db, prefix).await;
        }
        // C reads `nfs_managed()` once, at the task's start (save_restore.c:792).
        let nfs_managed = self.with_nfs(|n| n.managed()).unwrap_or(false);
        if !self.settings.nfs_ok() && nfs_managed {
            // save_restore.c:804-810, whose mount always fails here.
            self.with_nfs(|n| n.do_mount());
        }
        let mut priority = self.settings.task_priority();
        let mut disable = Disable::default();
        loop {
            if is_stopped(&shutdown_rx) {
                return;
            }
            // C sets a running task's priority at once (save_restore.c:2644).
            let wanted = self.settings.task_priority();
            if wanted != priority {
                priority = wanted;
                crate::runtime::task::reband_current_thread(
                    crate::runtime::task::ThreadPriority::Custom(priority),
                );
            }
            let now = Instant::now();
            // save_restore.c:901: a disabled pass goes straight to the ops.
            if let Some(prefix) = general
                && disable.holds(ca, db, prefix, now).await
            {
                if !self.serve_ops(db, &mut ops, &mut shutdown_rx).await {
                    return;
                }
                continue;
            }
            self.settings.hold_seq_settings();
            clock.check_seq(now, self.settings.seq_period());
            // save_restore.c:903-921: the pass's status before the lists'.
            let mut worst = ListStatus::new(SrStatus::Ok, "Ok");
            if !self.settings.nfs_ok()
                && nfs_managed
                && let Some(status) = self.with_nfs(|n| clock.nfs_status(n, now))
            {
                worst = status;
            }
            // A set created since the last pass, before or after iocInit,
            // joins here, as C's `lptr` list is walked afresh each pass.
            let list_lock = lock_list(&self.settings);
            let sets = self.sets();
            // A list `remove_data_set` took is dropped with its monitors.
            lists.retain(|l| sets.iter().any(|(s, _)| Arc::ptr_eq(&l.set, s)));
            for (set, lock) in sets {
                if !lists.iter().any(|l| Arc::ptr_eq(&l.set, &set)) {
                    lists.push(ListState::new(set, lock, started));
                }
            }
            for list in &mut lists {
                if !list.pass(db, now, &shutdown_rx, self, &mut clock).await {
                    return;
                }
            }
            // `<=`: of equally bad lists the later one's text wins.
            for list in &lists {
                let status = list.set.list_status();
                if status.code <= worst.code {
                    worst = status;
                }
            }
            drop(list_lock);
            *self.sr_status.lock().unwrap() = worst.clone();
            heartbeat = (heartbeat + 1) % 2;
            self.report_status(
                db,
                self.status_prefix.as_deref(),
                general.is_some(),
                &mut lists,
                worst,
                heartbeat,
            )
            .await;
            if !self.serve_ops(db, &mut ops, &mut shutdown_rx).await {
                return;
            }
        }
    }

    /// C waits for an op up to MIN_DELAY from the end of the pass, and
    /// again after each it serves (save_restore.c:1129-1131), however long
    /// the pass took; `false` once stopped.
    async fn serve_ops(
        &self,
        db: &PvDatabase,
        ops: &mut tokio::sync::mpsc::UnboundedReceiver<TaskOp>,
        shutdown_rx: &mut watch::Receiver<bool>,
    ) -> bool {
        loop {
            tokio::select! {
                biased;
                _ = stopped(shutdown_rx) => return false,
                Some(op) = ops.recv() => op.run(db, self).await,
                _ = crate::runtime::task::sleep(MIN_DELAY) => return true,
            }
        }
    }

    /// C `opMsgQueue`.
    pub(super) fn op_queue(&self) -> &Arc<OpQueue> {
        &self.ops
    }

    /// Wait for the running task to serve every op sent before this call.
    pub async fn ops_served(&self) {
        let (done, served) = tokio::sync::oneshot::channel();
        if self.ops.send(TaskOp::Served(done)) {
            let _ = served.await;
        }
    }

    /// save_restore.c:854-877, once: the worst restore-file status, the first
    /// of equally bad ones.
    async fn report_reboot(&self, db: &PvDatabase, prefix: &str) {
        let ca = self.ca_context(db);
        let mut worst = ListStatus::new(SrStatus::Ok, "Ok");
        for status in &self.reboot_statuses {
            let status = status.lock().unwrap();
            if status.code < worst.code {
                worst = status.clone();
            }
        }
        let _ = ca_put(
            ca,
            db,
            &format!("{prefix}SR_rebootStatus"),
            EpicsValue::Long(worst.code as i32),
        )
        .await;
        let _ = ca_put_str(ca, db, &format!("{prefix}SR_rebootStatusStr"), &worst.text).await;
        let time = chrono::Local::now().format("%a %b %d %H:%M:%S").to_string();
        let _ = ca_put_str(ca, db, &format!("{prefix}SR_rebootTime"), &time).await;
    }

    /// The tail of each pass (save_restore.c:1059-1121): SR_status is
    /// `worst`, when `general`;
    /// the lists' own PVs while `save_restoreUseStatusPVs` is set (:1067),
    /// which have names only under a `prefix`.
    async fn report_status(
        &self,
        db: &PvDatabase,
        prefix: Option<&str>,
        general: bool,
        lists: &mut [ListState],
        worst: ListStatus,
        heartbeat: i16,
    ) {
        let ca = self.ca_context(db);
        let statuses: Vec<_> = lists.iter().map(|l| l.set.list_status()).collect();
        if let Some(prefix) = prefix.filter(|_| general) {
            let recently = lists
                .iter()
                .filter_map(|l| l.set.recent())
                .chain(self.recent.lock().unwrap().clone())
                .chain(self.settings.io_recent())
                .max_by_key(|(at, _)| *at)
                .map(|(_, text)| text)
                .unwrap_or_default();
            let _ = ca_put(
                ca,
                db,
                &format!("{prefix}SR_status"),
                EpicsValue::Long(worst.code as i32),
            )
            .await;
            let _ = ca_put(
                ca,
                db,
                &format!("{prefix}SR_heartbeat"),
                EpicsValue::Short(heartbeat),
            )
            .await;
            let _ = ca_put_str(ca, db, &format!("{prefix}SR_statusStr"), &worst.text).await;
            let _ = ca_put_str(ca, db, &format!("{prefix}SR_recentlyStr"), &recently).await;
        }
        if !self.settings.use_status_pvs() {
            return;
        }
        let _list_lock = lock_list(&self.settings);
        let Some(prefix) = prefix else {
            return;
        };
        for (list, status) in lists.iter_mut().zip(statuses) {
            if let Some(index) = list.set.status_pv_index() {
                let pv = format!("{prefix}SR_{index}_");
                // The list's first time here (save_restore.c:1077-1104):
                // `ca_pend_io(0.5)` fails unless all five connect.
                if !list.status_pvs_searched {
                    list.status_pvs_searched = true;
                    let names = ["Status", "Name", "State", "StatusStr", "Time"];
                    if names
                        .iter()
                        .any(|n| ca_get(db, &format!("{pv}{n}")).is_err())
                    {
                        crate::epics_println!(
                            "save_restore: Can't connect to status PV(s) for list '{}'",
                            list.set.save_file()
                        );
                    }
                }
                report_list(ca, db, &pv, list, status).await;
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
        let list_lock = lock_list(&self.settings);
        let Some((set, _)) = self.find_set_by_request_file(filename) else {
            crate::runtime::log::errlog_vprintf(format_args!(
                "../save_restore.c:set_savefile_name No save set enabled for {filename}\n"
            ));
            return -1;
        };
        set.set_save_file(save_filename);
        drop(list_lock);
        self.note_recent(format!("New save file: '{save_filename}'"));
        0
    }

    /// The save task's `op_SaveFile` (save_restore.c:1207-1235): the list
    /// built from `request_file` saved to its own file when `filename` is
    /// empty, else to `path`, both as [`SaveSet::save_to`], then
    /// `asVerify`'d. C's status: -1 when there
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
            crate::epics_println!("save_restore task: manual save('{filename}')");
        }
        let list_lock = lock_list(&self.settings);
        let found = self.find_set_by_request_file(request_file);
        let (mut status, mut num_errs, mut full_path) = (-1, 0, None);
        if let Some((set, lock)) = &found {
            let _guard = lock.lock().await;
            let entries = set.get_channel_values(db);
            num_errs += set.not_connected();
            if set.not_connected() == 0 || self.settings.incomplete_sets_ok() {
                // manual_save sends a NULL name as "" (save_restore.c:714)
                // and returns the queue send's 0; the "" takes
                // write_save_file's configName path to the list's own file
                // (:2029-2039): no .savB, a dated copy of an existing .sav
                // (:2081-2092), then myFileCopy to that copy's name
                // (:2112-2124), which is "" on the first save, so only that
                // save's callback status is -1.
                let written = if filename.is_empty() {
                    match set.file_path(db) {
                        Some(path) => set.save_to(entries, &path).await.map(|()| path),
                        None => {
                            set.set_list_status(ListStatus::new(SrStatus::Ok, "Ok"));
                            Ok(std::path::PathBuf::new())
                        }
                    }
                } else if set.file_pv(SAVEPATHPV).is_none() {
                    set.save_to(entries, path)
                        .await
                        .map(|()| path.to_path_buf())
                } else {
                    // The PV's directory with `filename` (save_restore.c:2018-2030).
                    let standard = path.parent().unwrap_or(std::path::Path::new(""));
                    match set.file_dir(db, standard) {
                        Some(dir) => {
                            let path = super::request::concatenate_paths(
                                &dir.to_string_lossy(),
                                filename,
                                &self.settings,
                            );
                            set.save_to(entries, path.as_ref())
                                .await
                                .map(|()| path.into())
                        }
                        None => Ok(std::path::PathBuf::new()),
                    }
                };
                match written {
                    Ok(path) => {
                        status = 0;
                        full_path = Some(path);
                    }
                    Err(_) => crate::epics_println!(
                        "save_restore: write_save_file() failed for {}",
                        set.save_file()
                    ),
                }
            }
        }
        drop(list_lock);
        if let Some(path) = full_path {
            status = super::verify::do_as_verify(db, &path, -1, None).await;
        }
        if debug > 1 {
            crate::epics_println!("save_restore: manual save status={status} (0==success)");
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
        let status = do_manual_restore(db, self, filename, path, from, macrostring).await;
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
            crate::epics_println!("save_restore task: calling do_remove_data_set('{filename}')");
        }
        let status = self.remove_list(filename);
        if debug > 1 {
            crate::epics_println!("save_restore: remove status={status} (0==success)");
        }
        status
    }

    fn remove_list(&self, filename: &str) -> i32 {
        // C walks the lists once to find it and again to unlink it
        // (save_restore.c:2671-2719).
        let found = {
            let _list_lock = lock_list(&self.settings);
            self.find_set_by_request_file(filename)
        };
        match found {
            Some((set, _)) => {
                let _list_lock = lock_list(&self.settings);
                self.sets
                    .lock()
                    .unwrap()
                    .retain(|(s, _)| !Arc::ptr_eq(s, &set));
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

    /// C's `save_restore*` globals.
    pub(super) fn settings(&self) -> &SrSettings {
        &self.settings
    }

    /// The CA context of C's task, made once as it starts: [`Self::start`]
    /// asks first, so it is the iocInit phase at the first `create_*_set`.
    pub(super) fn ca_context(&self, db: &PvDatabase) -> CaContext {
        *self.ca.get_or_init(|| CaContext::at(db))
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
    /// (`getMacroString`, `op_SaveFile`): by `strcmp`, not as paths, so
    /// `a//b.req` is not the list made from `a/b.req`.
    pub fn find_set_by_request_file(&self, request_file: &str) -> Option<SetEntry> {
        self.sets
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| {
                s.config()
                    .request_file
                    .as_deref()
                    .is_some_and(|f| f.as_os_str() == request_file)
            })
            .map(|(s, l)| (s.clone(), l.clone()))
    }

    /// A snapshot of the built sets.
    pub fn sets(&self) -> Vec<SetEntry> {
        self.sets.lock().unwrap().clone()
    }

    /// `f` on the startup config's NFS host, when there is a config.
    fn with_nfs<R>(&self, f: impl FnOnce(&super::nfs::NfsHost) -> R) -> Option<R> {
        let cfg = self.startup.get()?.upgrade()?;
        let cfg = cfg.lock().unwrap();
        Some(f(&cfg.nfs))
    }

    /// Name the startup config this manager was built from.
    pub fn attach_startup(&self, cfg: &Arc<Mutex<super::AutosaveStartupConfig>>) {
        let _ = self.startup.set(Arc::downgrade(cfg));
    }

    /// C `SR_status` and `SR_statusStr`.
    pub fn sr_status(&self) -> ListStatus {
        self.sr_status.lock().unwrap().clone()
    }

    /// The lists' part of C `save_restoreShow` (save_restore.c:2522-2572),
    /// newest first as C's `lptr` is. A member's line has no `chid`, and its
    /// value is the database's now rather than the last `ca_get`'s.
    pub fn show_lists(&self, db: &PvDatabase, out: &dyn Fn(std::fmt::Arguments<'_>), verbose: i32) {
        const TIMEFMT: &str = "%a %b %d %H:%M:%S %Y";
        // C `MIN_PERIOD`, the floor `create_data_set` leaves on a period no
        // method set.
        const MIN_PERIOD: u64 = 4;
        let _list_lock = lock_list(&self.settings);
        for (set, _) in self.sets().iter().rev() {
            let cfg = set.config();
            let status = set.list_status();
            let method = set.save_method();
            let (save_state, not_connected) = (set.task_view(), set.not_connected());
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
            out(format_args!("  {}: ", cfg.key()));
            out(format_args!(
                "    macro string: '{}'",
                cfg.macro_string.as_deref().unwrap_or_default()
            ));
            out(format_args!("    Status PV: {status_pv}"));
            out(format_args!(
                "    Status: '{}' - '{}'",
                status.code.label(),
                status.text
            ));
            out(format_args!(
                "    Last save time  :{}",
                set.save_time().format(TIMEFMT)
            ));
            out(format_args!(
                "    Last backup time:{}",
                set.backup_time().format(TIMEFMT)
            ));
            out(format_args!("    methods: {methods}"));
            // save_restore.c:2547-2556. C's ca_array_get there has no
            // ca_pend_io, so it prints tmpstr's stale "[ ... ]" (:2549,
            // :2554); a C defect, not copied: rs prints the PV's value.
            for (label, key, what) in [("path", SAVEPATHPV, "path"), ("name", SAVENAMEPV, "name")] {
                match set.file_pv(key) {
                    Some(pv) => {
                        out(format_args!("    {label} PV: {pv}"));
                        out(format_args!("        {what}: '{}'", file_pv_value(db, pv)));
                    }
                    None => out(format_args!("    {label} PV: None")),
                }
            }
            out(format_args!(
                "    backups: {}",
                if set.do_backups() { "YES" } else { "NO" }
            ));
            out(format_args!("    save_state = 0x{save_state:x}"));
            out(format_args!(
                "    period: {period}; trigger chan: '{}'; monitor period: {monitor_period}",
                set.trigger_channel().unwrap_or_default()
            ));
            out(format_args!(
                "    last saved file - {}",
                set.last_save_file()
            ));
            out(format_args!(
                "    {not_connected} channel{} not connected (or ca_get failed)",
                if not_connected == 1 { ' ' } else { 's' }
            ));
            if verbose != 0 && !*self.shutdown.borrow() {
                for (name, ch) in set.pv_names().iter().zip(set.channels().iter()) {
                    show_member(db, out, name, ch);
                }
            }
        }
    }
}

/// `ca_get(DBR_STRING)` of `name`, whose value is `live`: a menu field as
/// its choice.
pub(super) fn dbr_string(db: &PvDatabase, name: &str, live: &EpicsValue) -> String {
    let (base, field) = crate::server::database::parse_pv_name(name);
    match db.get_record(base) {
        Some(rec) => rec
            .read()
            .source_render_of(field, live)
            .dbr_string(live.clone()),
        None => value_as_dbr_string(live),
    }
    .map(|s| s.to_string())
    .unwrap_or_default()
}

/// C `channel_connected` of member `name`, `None` with no chid: what
/// `ca_connection_callback` makes of the channel `connect_list` made, once
/// the server has answered (save_restore.c:731-745). A channel the server
/// found but refused raises the disconnect that clears the chid.
pub(super) fn channel_state(db: &PvDatabase, name: &str, ch: &Channel) -> Option<bool> {
    ch.chid.as_ref()?;
    let refused = ca_connects(db)
        && db
            .get_pv(
                crate::server::database::filters::parse_channel_name(name.as_bytes()).record_path,
            )
            .is_ok()
        && crate::server::database::filters::channel::db_channel_create_unreported(
            db,
            name.as_bytes(),
        )
        .is_err();
    (!refused).then(|| ca_get(db, name).is_ok())
}

/// A list member's `save_restoreShow` line (save_restore.c:2564-2570):
/// its channel as `connect_list` and `get_channel_values` left it, and the
/// channel's state now.
fn show_member(db: &PvDatabase, out: &dyn Fn(std::fmt::Arguments<'_>), name: &str, ch: &Channel) {
    let (chid, state, connected) = match (&ch.chid, channel_state(db, name, ch)) {
        (Some(chid), Some(true)) => (format!("{:p}", &**chid), "Conn", 1),
        (Some(chid), Some(false)) => (format!("{:p}", &**chid), "Never", 0),
        _ => ("(nil)".to_string(), "noChid", 0),
    };
    let enum_val = ch.enum_val.map(|i| format!("\t{i}")).unwrap_or_default();
    out(format_args!(
        "\t{name} chid:{chid} state:{state} (max:{} curr:{} elements)\t{}   channel_connected = {connected}{enum_val}",
        ch.max, ch.curr, ch.value
    ));
}

/// `get_channel_values`' one element of `name`, whose value is `live`, as
/// C keeps it in `pchannel->value`, and the `DBR_SHORT` of an enum
/// (save_restore.c:1562-1576, :1611-1621).
pub(super) fn channel_value(
    db: &PvDatabase,
    name: &str,
    live: Option<&EpicsValue>,
) -> (String, Option<i16>) {
    let Some(live) = live else {
        // `DBR_STRING` of a long string's first `DBF_CHAR`.
        let first = super::save_set::long_string_text(db, name)
            .and_then(|t| t.first().copied())
            .unwrap_or(0);
        return ((first as i8).to_string(), None);
    };
    let native = live.dbr_type();
    let value = if matches!(native, DbFieldType::Float | DbFieldType::Double) {
        let first = match live.convert_to(DbFieldType::Double) {
            EpicsValue::Double(d) => d,
            EpicsValue::DoubleArray(a) => a.first().copied().unwrap_or_default(),
            _ => 0.0,
        };
        if native == DbFieldType::Float {
            format!("{:.7}", CFloat(first as f32 as f64).g())
        } else {
            format!("{:.14}", CFloat(first).g())
        }
    } else if live.is_array() {
        match live.get_convert(DbFieldType::String) {
            Ok(EpicsValue::StringArray(a)) => a.first().map(|s| s.to_string()).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        dbr_string(db, name, live)
    };
    let enum_val = match live {
        EpicsValue::Enum(i) => Some(*i as i16),
        _ => None,
    };
    (value, enum_val)
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
async fn report_list(
    ca: CaContext,
    db: &PvDatabase,
    pv: &str,
    list: &ListState,
    status: ListStatus,
) {
    let set = &list.set;
    let _ = ca_put(
        ca,
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
    let _ = ca_put_str(ca, db, &format!("{pv}Name"), &name).await;
    // C's `save_state` once the pass has cleared the events it served.
    let _ = ca_put(
        ca,
        db,
        &format!("{pv}State"),
        EpicsValue::Long(list.save_state as i32),
    )
    .await;
    let _ = ca_put_str(ca, db, &format!("{pv}StatusStr"), &status.text).await;
    if status.code >= SrStatus::Warn {
        let time = set.save_time().format("%a %b %d %H:%M:%S").to_string();
        let _ = ca_put_str(ca, db, &format!("{pv}Time"), &time).await;
    }
}

/// Can the task's CA channels connect? C's task, started by the first
/// `create_*_set`, may run before `iocInit`, when no channel to the IOC's own
/// records connects; each connects later, through its connection callback.
/// Every channel the task reads, puts or monitors goes through this.
pub(super) fn ca_connects(db: &PvDatabase) -> bool {
    !db.ioc_init_pending()
}

/// `ca_get` of a channel the task searched for.
pub(super) fn ca_get(db: &PvDatabase, name: &str) -> crate::error::CaResult<EpicsValue> {
    if !ca_connects(db) {
        return Err(crate::error::CaError::Timeout);
    }
    super::save_set::read_member(db, name)
        .ok_or_else(|| crate::error::CaError::ChannelNotFound(name.to_string()))
}

/// C `lockList`'s trace (save_restore.c:514-522) for a walk of the lists,
/// and `unlockList`'s (:524-530) when the walk ends. The walk's exclusion is
/// `sets`' own lock; C's wait for a held `listLock` is not modelled.
pub(super) fn lock_list(settings: &SrSettings) -> ListLock<'_> {
    if settings.debug() >= 15 {
        crate::epics_println!("lockList: listLock=1");
    }
    ListLock(settings)
}

pub(super) struct ListLock<'a>(&'a SrSettings);

impl Drop for ListLock<'_> {
    fn drop(&mut self) {
        if self.0.debug() >= 15 {
            crate::epics_println!("unlockList: listLock=0");
        }
    }
}

/// C `ca_element_count`, 0 for a channel that did not connect, one
/// `dbChannelCreate` refused included.
pub(super) fn ca_element_count(db: &PvDatabase, name: &str) -> u32 {
    let channel = crate::server::database::filters::channel::db_channel_create_unreported(
        db,
        name.as_bytes(),
    );
    match ca_connects(db) && channel.is_ok() {
        true => db.restore_target(name).map_or(0, |t| t.elements),
        false => 0,
    }
}

/// C `connect_list`'s `max_elements` (save_restore.c:1320-1360): the
/// channel's count, then for an array `SR_get_array_info`'s count of the
/// name without its `$`, -1 where `dbNameToAddr` fails. C keeps the second.
pub(super) fn max_elements(db: &PvDatabase, name: &str) -> (u32, Option<i64>) {
    let count = ca_element_count(db, name);
    let info = (count > 1).then(|| {
        db.restore_target(name.strip_suffix('$').unwrap_or(name))
            .map_or(-1, |t| i64::from(t.elements))
    });
    (count, info)
}

/// C autosave's `ca_put` on a status PV, which rsrv serves as `dbPutField`
/// (save_restore.c:1061-1062): the record processes, so an mbbo status takes
/// its state alarm and a heartbeat bo fires its FLNK. `TRY_TO_PUT` puts
/// only to a connected channel, and drops the status `ca` returns for a
/// refusal; through rsrv, libca's block reports it.
async fn ca_put(
    ca: CaContext,
    db: &PvDatabase,
    name: &str,
    value: EpicsValue,
) -> crate::error::CaResult<()> {
    ca_get(db, name)?;
    let (record, field) = crate::server::database::parse_pv_name(name);
    let dbr = value.dbr_type();
    let count = value.count();
    db.put_record_field_from_ca_no_notify(record, field, value)
        .await
        .inspect_err(|_| {
            if ca.refused_put().is_none() {
                write_exception(db, name, dbr, count);
            }
        })
}

/// libca's default exception handler on a write the server refused
/// (`oldChannelNotify.cpp:159`, `ca_client_context.cpp:361-416`): each line
/// goes through errlog, as its `printFormated` does. `ctx` is rsrv's
/// `RECORD_NAME(dbch)` (camessage.c:56): the record the channel resolved
/// to, so `AL.DESC` on an alias of `A` names `A`.
pub(super) fn write_exception(db: &PvDatabase, name: &str, dbr: DbFieldType, count: u32) {
    let (record, _) = crate::server::database::parse_pv_name(name);
    let ctx = db
        .get_record(record)
        .map_or_else(|| name.into(), |rec| rec.name().to_str_lossy().into_owned());
    let now = chrono::Local::now();
    let lines = [
        "CA.Client.Exception...............................................".to_string(),
        "    Warning: \"Channel write request failed\"".to_string(),
        format!(
            "    Context: \"op=1, channel={name}, type={}, count={count}, ctx=\"{ctx}\"\"",
            crate::types::dbr_type_to_text(dbr as u16)
        ),
        "    Source File: ../oldChannelNotify.cpp line 159".to_string(),
        format!(
            "    Current Time: {}.{:09}",
            now.format("%a %b %d %Y %H:%M:%S"),
            now.timestamp_subsec_nanos()
        ),
        "..................................................................".to_string(),
    ];
    for line in lines {
        crate::runtime::log::errlog_vprintf(format_args!("{line}\n"));
    }
}

/// [`ca_put`] of C's `DBR_STRING` from a `STATUS_STR_LEN` buffer: the record
/// keeps the first 39 characters.
async fn ca_put_str(
    ca: CaContext,
    db: &PvDatabase,
    name: &str,
    text: &str,
) -> crate::error::CaResult<()> {
    let text: String = text.chars().take(39).collect();
    ca_put(ca, db, name, EpicsValue::String(text.into())).await
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

/// The save task's own timestamps, which every list's pass reads.
struct TaskClock {
    /// C `last_seq_check`.
    last_seq_check: Instant,
    /// C `do_seq_check`, for this pass.
    seq_check: bool,
    /// C `remount_check_time`.
    remount_check: Instant,
}

impl TaskClock {
    fn new(task_started: Instant) -> Self {
        Self {
            last_seq_check: task_started,
            seq_check: false,
            remount_check: task_started,
        }
    }

    /// The pass's status while the file system is failed and managed
    /// (save_restore.c:913-921).
    fn nfs_status(&mut self, nfs: &super::nfs::NfsHost, now: Instant) -> ListStatus {
        if nfs.restore_mount(&mut self.remount_check, now) {
            ListStatus::new(SrStatus::Ok, "NFS remounted")
        } else {
            ListStatus::new(SrStatus::Fail, "NFS failed!")
        }
    }

    /// C's `do_seq_check`, once a pass for every list: half a sequence
    /// period since the last (save_restore.c:907-908).
    fn check_seq(&mut self, now: Instant, seq_period: Duration) {
        let half = Duration::from_secs(seq_period.as_secs() / 2);
        self.seq_check = now.duration_since(self.last_seq_check) > half;
        if self.seq_check {
            self.last_seq_check = now;
        }
    }
}

/// C `SR_disable`, `SR_disableMaxSecs` and `disableStart`
/// (save_restore.c:366-371), as the save task last read them.
#[derive(Default)]
struct Disable {
    on: i32,
    max_secs: i32,
    start: Option<Instant>,
}

impl Disable {
    /// save_restore.c:884-899: whether `<prefix>SR_disable` holds this pass
    /// off. A disable older than `SR_disableMaxSecs` is put back to 0 and
    /// the pass runs.
    async fn holds(&mut self, ca: CaContext, db: &PvDatabase, prefix: &str, now: Instant) -> bool {
        let read = |name: &str| {
            ca_get(db, &format!("{prefix}{name}"))
                .ok()
                .and_then(|v| v.to_f64())
                .map(|v| v as i32)
        };
        if let Some(on) = read("SR_disable") {
            self.on = on;
        }
        if let Some(max_secs) = read("SR_disableMaxSecs") {
            self.max_secs = max_secs;
        }
        if self.on == 0 {
            // C clears `disableStart` only when it times out (:896), so a
            // disable put back to 0 by hand leaves it set and the next
            // disable times out at once, a C defect: the time restarts.
            self.start = None;
            return false;
        }
        match self.start {
            None => self.start = Some(now),
            Some(start) if now.duration_since(start).as_secs_f64() > f64::from(self.max_secs) => {
                self.on = 0;
                let _ = ca_put(ca, db, &format!("{prefix}SR_disable"), EpicsValue::Long(0)).await;
                self.start = None;
            }
            Some(_) => {}
        }
        self.on != 0
    }
}

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
    /// The member monitors whose channel has not connected: C's first
    /// `on_change_save` comes with the connection, so these raise no event
    /// until the channels can connect.
    unconnected_monitors: Vec<SubscriptionActivation>,
    /// C's non-empty `status_PV`: the list's status PVs have been searched.
    status_pvs_searched: bool,
    /// C `plist->reconnect_check_time`.
    reconnect_check_time: Instant,
    /// C `lastPeriodicDatedBackup`, one per list. C keeps one for the task
    /// and resets it inside the list loop (save_restore.c:1016-1022), so
    /// only its first list with backups ever gets one, against the dated
    /// backup each list is documented to get (userFunctions.md:45-59);
    /// that C defect is not kept.
    last_dated: Instant,
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
            unconnected_monitors: Vec::new(),
            status_pvs_searched: false,
            reconnect_check_time: task_started,
            last_dated: task_started,
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
        clock: &mut TaskClock,
    ) -> bool {
        let method = self.set.save_method();
        let settings = self.set.settings().clone();
        if settings.debug() >= 30 {
            crate::epics_println!(
                "save_restore: '{}' save_state = 0x{:x}",
                self.set.config().key(),
                self.save_state
            );
        }
        if self.enabled == 0 {
            // save_restore.c:936-949.
            let unresolved = [SAVEPATHPV, SAVENAMEPV]
                .iter()
                .filter_map(|key| self.set.file_pv(key))
                .any(|pv| ca_get(db, pv).is_err());
            if unresolved {
                crate::epics_println!(
                    "save_restore: Can't connect to list-specific path/name PV(s)"
                );
                self.set.set_list_status(ListStatus::new(
                    SrStatus::Warn,
                    "List path/name PVs connection failed",
                ));
            }
            self.connect(db, mgr, true);
            self.reconnect_check_time = now;
        } else if settings.ca_reconnect()
            && self.set.not_connected() > 0
            && now.duration_since(self.reconnect_check_time) > CA_RECONNECT_TIME
        {
            // save_restore.c:958-963
            self.reconnect_check_time = now;
            self.connect(db, mgr, false);
        }
        if self.enabled != method {
            self.enable(db, now, mgr).await;
        }
        if ca_connects(db) {
            for monitor in self.unconnected_monitors.drain(..) {
                monitor.enable(db).await;
            }
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
                crate::epics_println!(
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
            let entries = self.set.get_channel_values(db);
            if self.set.not_connected() == 0 || settings.incomplete_sets_ok() {
                let _ = self.set.write_entries(db, entries).await;
            }
            // Monitor events delivered while the values were fetched are
            // cleared with the rest, as C's preemptive callbacks are.
            self.take_events(now);
        }

        // save_restore.c:1008-1012.
        if clock.seq_check
            && self.set.do_backups()
            && self.set.list_status().code > SrStatus::Fail
            && self.set.seq_due(now)
        {
            let lock = self.lock.clone();
            let _guard = lock.lock().await;
            if is_stopped(stop) {
                return false;
            }
            self.set.do_seq(db).await;
        }

        // Only a list that keeps backups (save_restore.c:1018).
        if self.set.do_backups()
            && settings
                .periodic_dated_backup()
                .is_some_and(|period| now.duration_since(self.last_dated) > period)
        {
            let lock = self.lock.clone();
            let _guard = lock.lock().await;
            if is_stopped(stop) {
                return false;
            }
            self.set.do_periodic_dated_backup().await;
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
        self.set.publish_task_view(self.save_state);
        true
    }

    /// C `connect_list(plist, verbose)` (save_restore.c:1276-1374): a search
    /// for each member not yet connected. Members are database names, so a
    /// member is connected when it resolves, as [`SaveSet::save_once`]
    /// reads it, but a channel rsrv serves connects only after the pass.
    fn connect(&mut self, db: &PvDatabase, mgr: &AutosaveManager, verbose: bool) {
        self.set.set_status_text("Connecting PVs...");
        let names = self.set.pv_names();
        let debug = self.set.settings().debug();
        if debug >= 10 {
            for name in &names {
                crate::epics_println!("save_restore:connect_list: channel '{name}'");
            }
        }
        // One channel per member, a PV listed twice included.
        let searched: Vec<usize> = {
            let channels = self.set.channels();
            (0..names.len())
                .filter(|&i| channel_state(db, &names[i], &channels[i]) != Some(true))
                .collect()
        };
        // `ca_create_channel` makes the server's `dbChannelCreate`, which
        // prints its own refusal once per create. The `ca_pend_io` after
        // it waits for no channel with a connection callback, so one rsrv
        // serves is still searching at the check (save_restore.c:1294-1318).
        let in_process = mgr.ca_context(db) == CaContext::InProcess;
        let found: Vec<bool> = searched
            .iter()
            .map(|&i| {
                ca_connects(db)
                    && crate::server::database::filters::db_channel_create(db, &names[i]).is_ok()
                    && in_process
            })
            .collect();
        // C's "not all searches successful" (save_restore.c:1304-1306) is
        // never printed: every channel has a connection callback, so
        // ca_pend_io has no request to time out on.
        if verbose {
            for (&i, &ok) in searched.iter().zip(&found) {
                if !ok {
                    crate::epics_println!(
                        "save_restore: connect failed for channel '{}'",
                        names[i]
                    );
                }
            }
        }
        let mut created = Vec::with_capacity(searched.len());
        for (&i, &ok) in searched.iter().zip(&found) {
            let name = &names[i];
            // C `ca_element_count`, 0 for a channel that did not connect.
            let (count, info) = match ok {
                true => max_elements(db, name),
                false => (0, None),
            };
            created.push((i, ok, info.unwrap_or(i64::from(count))));
            if debug >= 10 {
                crate::epics_println!(
                    "save_restore:connect_list: '{name}' has, at most, {count} elements"
                );
                match info {
                    Some(-1) => {
                        crate::epics_println!(
                            "save_restore:connect_list: array PV '{name}' is not local."
                        )
                    }
                    Some(max) => crate::epics_println!(
                        "save_restore:connect_list:(after SR_get_array_info) '{name}' has, at most, {max} elements"
                    ),
                    None => {}
                }
            }
        }
        self.set.channels_created(&created);
        let line = format!(
            "{}: {} of {} PV's connected",
            self.set.save_file(),
            found.iter().filter(|&&ok| ok).count(),
            searched.len()
        );
        if verbose {
            crate::epics_println!("{line}");
        }
        mgr.note_recent(line);
        // :1376.
        let searching: Vec<usize> = searched
            .iter()
            .zip(&found)
            .filter_map(|(&i, &ok)| (!ok).then_some(i))
            .collect();
        self.set.get_channel_values_searching(db, &searching);
    }

    /// C `enable_list` (save_restore.c:1425-1487): start what each method
    /// not yet enabled needs. A trigger that cannot be monitored stays
    /// disabled, and the next pass tries again.
    async fn enable(&mut self, db: &PvDatabase, now: Instant, mgr: &AutosaveManager) {
        let method = self.set.save_method();
        let debug = self.set.settings().debug();
        if debug >= 4 {
            crate::epics_println!("save_restore:enable_list: entry");
        }
        self.set.set_status_text("Enabling list...");
        if method & SaveStrategy::PERIODIC != 0 && self.enabled & SaveStrategy::PERIODIC == 0 {
            self.periodic_due = Some(now + self.set.period());
            self.enabled |= SaveStrategy::PERIODIC;
            self.callback_time = now;
        }
        if method & SaveStrategy::TRIGGERED != 0 && self.enabled & SaveStrategy::TRIGGERED == 0 {
            if let Some(name) = self.set.trigger_channel() {
                // A name no record answers is C's `ca_pend_io` timeout, as
                // is any name before the channels can connect.
                let sub = match ca_connects(db) {
                    true => DbSubscription::subscribe_with_mask(db, &name, 0, CA_DEFAULT_MASK)
                        .await
                        .ok(),
                    false => None,
                };
                match sub {
                    Some(sub) => {
                        // `ca_add_event`'s first callback carries the
                        // current value.
                        sub.activation_handle().enable(db).await;
                        self.trigger = Some(sub);
                        self.enabled |= SaveStrategy::TRIGGERED;
                    }
                    None => {
                        // The `ca_pend_io(2.0)` that times out holds the
                        // task, and the list lock, for its whole wait
                        // (save_restore.c:1442).
                        crate::runtime::task::sleep(Duration::from_secs(2)).await;
                        crate::epics_println!(
                            "save_restore:enable_list: timeout on search of {name}"
                        );
                    }
                }
            }
        }
        if method & SaveStrategy::MONITORED != 0 && self.enabled & SaveStrategy::MONITORED == 0 {
            let names = self.set.pv_names();
            let connected: Vec<bool> = {
                let channels = self.set.channels();
                names
                    .iter()
                    .zip(channels.iter())
                    .map(|(name, ch)| channel_state(db, name, ch) == Some(true))
                    .collect()
            };
            for (name, connected) in names.into_iter().zip(connected) {
                if debug >= 10 {
                    crate::epics_println!(
                        "save_restore:enable_list: calling ca_add_event for '{name}'"
                    );
                    crate::epics_println!(
                        "save_restore:enable_list: arg = {:p}",
                        Arc::as_ptr(&self.set)
                    );
                }
                // `ca_add_event` goes on the channel `connect_list` made, so
                // it creates none; one C refused never raises an event. Before
                // `iocInit` none has connected yet, and each waits.
                if ca_connects(db) && !connected {
                    continue;
                }
                if let Ok(sub) =
                    DbSubscription::subscribe_with_mask(db, &name, 0, CA_DEFAULT_MASK).await
                {
                    self.unconnected_monitors.push(sub.activation_handle());
                    self.members.push(sub);
                }
            }
            if debug >= 4 {
                crate::epics_println!(
                    "save_restore:enable_list: done calling ca_add_event for list channels"
                );
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

    /// :907-908: a check moves `last_seq_check`, so the next is half a
    /// period after it, not after the task started.
    #[test]
    fn a_seq_check_waits_half_a_period_after_the_last() {
        let start = Instant::now();
        let period = Duration::from_secs(10);
        let mut clock = TaskClock::new(start);
        let checks: Vec<bool> = [4, 6, 7, 10, 12]
            .into_iter()
            .map(|s| {
                clock.check_seq(start + Duration::from_secs(s), period);
                clock.seq_check
            })
            .collect();
        assert_eq!(checks, [false, true, false, false, true]);
    }

    /// A failed remount makes the pass FAIL "NFS failed!", and a pass with
    /// no try OK "NFS remounted" (save_restore.c:913-921).
    #[test]
    fn a_failed_remount_is_the_pass_status() {
        let start = Instant::now();
        let mut clock = TaskClock::new(start);
        let nfs = super::super::nfs::NfsHost::default();
        let ok = clock.nfs_status(&nfs, start + Duration::from_secs(1));
        assert_eq!((ok.code, ok.text.as_str()), (SrStatus::Ok, "NFS remounted"));
        let failed = clock.nfs_status(&nfs, start + Duration::from_secs(61));
        assert_eq!(
            (failed.code, failed.text.as_str()),
            (SrStatus::Fail, "NFS failed!")
        );
    }
    use crate::server::autosave::backup::BackupConfig;
    use crate::server::records::ao::AoRecord;

    /// Which of three lists, the first without backups, hold a dated backup
    /// after one pass a period past the task's start and another a period
    /// after that.
    async fn dated_after_passes() -> Vec<Vec<bool>> {
        let db = PvDatabase::new();
        db.add_record("A", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        let settings = Arc::new(SrSettings::default());
        settings.set_retry_seconds(1000);
        settings.set_periodic_dated_backups(1);
        let dir = tempfile::tempdir().unwrap();
        let mut builder = AutosaveBuilder::new().settings(settings);
        for (name, enable_savb) in [("n", false), ("a", true), ("b", true)] {
            builder = builder.add_set(
                SaveSetConfig {
                    name: name.into(),
                    save_path: dir.path().join(format!("{name}.sav")),
                    request_file: None,
                    request_pvs: vec!["A".into()],
                    backup: BackupConfig { enable_savb },
                    macros: HashMap::new(),
                    macro_string: None,
                    search_paths: Vec::new(),
                },
                SaveStrategy::Manual,
            );
        }
        let mgr = builder.build().await;
        let start = Instant::now();
        let mut clock = TaskClock::new(start);
        let mut lists: Vec<ListState> = mgr
            .sets()
            .into_iter()
            .map(|(set, lock)| ListState::new(set, lock, start))
            .collect();
        let (_tx, stop) = watch::channel(false);
        let mut out = Vec::new();
        for secs in [61, 122] {
            for list in &mut lists {
                let now = start + Duration::from_secs(secs);
                assert!(list.pass(&db, now, &stop, &mgr, &mut clock).await);
            }
            let dated = |name: &str| {
                std::fs::read_dir(dir.path())
                    .unwrap()
                    .filter(|e| {
                        let file = e.as_ref().unwrap().file_name();
                        file.to_string_lossy()
                            .starts_with(&format!("{name}.sav_b_"))
                    })
                    .count()
            };
            out.push(["n", "a", "b"].map(|n| dated(n) > 0).to_vec());
            for entry in std::fs::read_dir(dir.path()).unwrap() {
                let path = entry.unwrap().path();
                if path.to_string_lossy().contains("_b_") {
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
        out
    }

    /// C's `do_seq_check && plist->do_backups` (save_restore.c:1008): a
    /// saved list writes its sequence file only when it keeps backups.
    #[crate::epics_test]
    async fn a_list_without_backups_writes_no_sequence_file() {
        let db = PvDatabase::new();
        db.add_record("A", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        let settings = Arc::new(SrSettings::default());
        settings.set_retry_seconds(1000);
        settings.set_num_seq_files(3);
        let dir = tempfile::tempdir().unwrap();
        let mut builder = AutosaveBuilder::new().settings(settings.clone());
        for (name, enable_savb) in [("n", false), ("a", true)] {
            builder = builder.add_set(
                SaveSetConfig {
                    name: name.into(),
                    save_path: dir.path().join(format!("{name}.sav")),
                    request_file: None,
                    request_pvs: vec!["A".into()],
                    backup: BackupConfig { enable_savb },
                    macros: HashMap::new(),
                    macro_string: None,
                    search_paths: Vec::new(),
                },
                SaveStrategy::Manual,
            );
        }
        let mgr = builder.build().await;
        let start = Instant::now();
        let mut clock = TaskClock::new(start);
        let (_tx, stop) = watch::channel(false);
        let now = start + Duration::from_secs(100);
        clock.check_seq(now, settings.seq_period());
        for (set, lock) in mgr.sets() {
            set.save_once(&db).await.unwrap();
            let mut list = ListState::new(set, lock, start);
            assert!(list.pass(&db, now, &stop, &mgr, &mut clock).await);
        }
        assert!(!dir.path().join("n.sav0").exists());
        assert!(dir.path().join("a.sav0").exists());
    }

    /// Each list with backups gets one each period, and a list without
    /// backups none (save_restore.c:1018); C's one task-wide clock, which
    /// backs up only the first, is not kept.
    #[crate::epics_test]
    async fn a_dated_backup_period_backs_up_every_list_with_backups() {
        assert_eq!(
            dated_after_passes().await,
            [[false, true, true], [false, true, true]]
        );
    }

    /// The list's SR_recentlyStr and statusStr after a pass at `start` and
    /// passes at `start + secs` for each of `later`, a record `B` loaded
    /// once the pass at `start + b_at` has run.
    async fn after_passes(ca_reconnect: i32, later: &[u64], b_at: u64) -> Vec<(String, String)> {
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
                    macro_string: None,
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
        let mut clock = TaskClock::new(start);
        let mut out = Vec::new();
        for secs in std::iter::once(&0).chain(later) {
            assert!(
                list.pass(
                    &db,
                    start + Duration::from_secs(*secs),
                    &stop,
                    &mgr,
                    &mut clock,
                )
                .await
            );
            out.push(read());
            if *secs == b_at {
                db.add_record("B", Box::new(AoRecord::new(0.0)))
                    .await
                    .unwrap();
            }
        }
        out
    }

    fn row(recent: &str, status: &str) -> (String, String) {
        (recent.into(), status.into())
    }

    // RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --lib server::autosave::manager::tests` (2/2).
    /// save_restore.c:958-963: a search for the channel that failed, once
    /// CA_RECONNECT_TIME_SECONDS has passed. One its connection callback
    /// has connected since is not searched again (:1290), so the next
    /// search makes no channel.
    #[tokio::test]
    async fn a_failed_member_is_searched_again_after_a_minute() {
        let enabled = row("list 's.sav' enabled", "Enabling list...");
        assert_eq!(
            after_passes(1, &[60, 61, 200], 61).await,
            [
                enabled.clone(),
                enabled,
                row("s.sav: 0 of 1 PV's connected", "Connecting PVs..."),
                row("s.sav: 0 of 0 PV's connected", "Connecting PVs..."),
            ]
        );
    }

    #[tokio::test]
    async fn without_ca_reconnect_it_is_not() {
        assert_eq!(
            after_passes(0, &[61], 0).await,
            [
                row("list 's.sav' enabled", "Enabling list..."),
                row("list 's.sav' enabled", "Enabling list..."),
            ]
        );
    }
}
