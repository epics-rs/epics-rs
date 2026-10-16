use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::server::database::PvDatabase;
use crate::types::EpicsValue;
use tokio::sync::watch;

use crate::runtime::task::TaskHandle;

use super::error::{AutosaveError, AutosaveResult};
use super::format::CompatMode;
use super::save_set::{
    ListStatus, RestoreMode, RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy,
    SrStatus,
};
use crate::server::database::db_access::DbSubscription;
use crate::server::recgbl::EventMask;
use std::time::{Duration, Instant};

/// Builder for constructing an AutosaveManager.
pub struct AutosaveBuilder {
    save_sets: Vec<(SaveSetConfig, SaveStrategy)>,
    global_macros: HashMap<String, String>,
    status_prefix: Option<String>,
    compat: CompatMode,
}

impl AutosaveBuilder {
    pub fn new() -> Self {
        Self {
            save_sets: Vec::new(),
            global_macros: HashMap::new(),
            status_prefix: None,
            compat: CompatMode::Native,
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

    /// Select the on-disk save-file format for every save set built
    /// by this builder. Default [`CompatMode::Native`]; use
    /// [`CompatMode::CRead`] so a C IOC can read the produced `.sav`
    /// files.
    pub fn compat(mut self, mode: CompatMode) -> Self {
        self.compat = mode;
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
    /// and kept as that set's [`SaveSetStatus::Error`], which is what
    /// `fdblist` and `asStatus` read.
    pub async fn build(self) -> AutosaveManager {
        let (shutdown_tx, _) = watch::channel(false);
        let manager = AutosaveManager {
            sets: Mutex::new(Vec::new()),
            adding: tokio::sync::Mutex::new(()),
            status_prefix: self.status_prefix,
            shutdown: shutdown_tx,
            compat: self.compat,
            global_macros: self.global_macros,
            recent: Mutex::new(None),
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
    compat: CompatMode,
    global_macros: HashMap<String, String>,
    /// The manager's own latest SR_recentlyStr event; see [`SaveSet::recent`].
    recent: Mutex<Option<(std::time::Instant, String)>>,
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
        let set = match SaveSet::new_with_mode(cfg, self.compat).await {
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
        self.sets.lock().unwrap().push((set, lock));
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
    pub fn start(
        self: Arc<Self>,
        reactor: &crate::runtime::task::Reactor,
        db: Arc<PvDatabase>,
    ) -> TaskHandle<()> {
        reactor.spawn(async move { self.save_task(&db).await })
    }

    /// The task's loop: a pass over every set, then the status PVs, at
    /// most once per `MIN_DELAY`.
    async fn save_task(&self, db: &PvDatabase) {
        let mut shutdown_rx = self.shutdown.subscribe();
        let mut lists: Vec<ListState> = Vec::new();
        let mut heartbeat = 0i16;
        loop {
            if is_stopped(&shutdown_rx) {
                return;
            }
            let now = Instant::now();
            // A set created since the last pass, before or after iocInit,
            // joins here, as C's `lptr` list is walked afresh each pass.
            for (set, lock) in self.sets() {
                if !lists.iter().any(|l| Arc::ptr_eq(&l.set, &set)) {
                    lists.push(ListState::new(set, lock));
                }
            }
            for list in &mut lists {
                if !list.pass(db, now, &shutdown_rx, self).await {
                    return;
                }
            }
            if let Some(prefix) = &self.status_prefix {
                heartbeat = (heartbeat + 1) % 2;
                self.report_status(db, prefix, &lists, heartbeat).await;
            }
            tokio::select! {
                biased;
                _ = stopped(&mut shutdown_rx) => return,
                _ = crate::runtime::task::sleep(MIN_DELAY.saturating_sub(now.elapsed())) => {}
            }
        }
    }

    /// The tail of each pass (save_restore.c:1059-1121): SR_status is the
    /// worst list status, starting from OK (:903, :1044).
    async fn report_status(
        &self,
        db: &PvDatabase,
        prefix: &str,
        lists: &[ListState],
        heartbeat: i16,
    ) {
        let statuses: Vec<_> = lists.iter().map(|l| l.set.list_status()).collect();
        // `<=`: of equally bad lists the later one's text wins (:1043-1046).
        let mut worst = ListStatus {
            code: SrStatus::Ok,
            text: "Ok".into(),
        };
        for status in &statuses {
            if status.code <= worst.code {
                worst = status.clone();
            }
        }
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
        // The first NUM_STATUS_PV_SETS lists, in creation order
        // (save_restore.c:2466-2470); a set is never removed.
        for (index, (list, status)) in lists.iter().zip(statuses).enumerate().take(8) {
            report_list(db, &format!("{prefix}SR_{index}_"), list, status).await;
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

    /// Manual restore for a specific set.
    pub async fn manual_restore(
        &self,
        set_name: &str,
        db: &PvDatabase,
    ) -> AutosaveResult<RestoreResult> {
        let (set, _lock) = self
            .find_set(set_name)
            .ok_or_else(|| AutosaveError::PvNotFound(format!("save set '{set_name}' not found")))?;

        // C `do_manual_restore` writes each value with `ca_put`, i.e.
        // `dbPutField` (save_restore.c:3245), unlike the boot restore's `dbPut`.
        let result = set.restore_once_with_mode(db, RestoreMode::Process).await;
        self.note_recent(match &result {
            Ok(r) if r.failed_puts.is_empty() => "Manual restore succeeded".into(),
            Ok(r) => format!("{} errors during manual restore", r.failed_puts.len()),
            Err(_) => "Manual restore failed".into(),
        });
        result
    }

    fn note_recent(&self, text: String) {
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
/// C `save_restoreRetrySeconds` (:378): a failed list's next attempt.
const RETRY_SECONDS: Duration = Duration::from_secs(60);
/// C `save_restoreCallbackTimeout` (:382-383): a periodic or monitored
/// list that no timer has re-armed for this long saves anyway.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(600);
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
}

impl ListState {
    fn new(set: Arc<SaveSet>, lock: Arc<tokio::sync::Mutex<()>>) -> Self {
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
        if self.enabled == 0 {
            self.connect(db, mgr);
        }
        if self.enabled != method {
            self.enable(db, now, mgr).await;
        }
        self.take_events(now);

        if (method & SaveStrategy::PERIODIC != 0
            || method & SaveStrategy::MONITORED == SaveStrategy::MONITORED)
            && now.duration_since(self.callback_time) > CALLBACK_TIMEOUT
        {
            self.save_state = method;
        }

        let save_needed = self.save_state & SINGLE_EVENTS != 0
            || self.save_state & SaveStrategy::MONITORED == SaveStrategy::MONITORED
            || (self.set.list_status().code <= SrStatus::Fail
                && now.duration_since(self.save_attempt_time) > RETRY_SECONDS);
        if save_needed {
            let lock = self.lock.clone();
            let _guard = lock.lock().await;
            if is_stopped(stop) {
                return false;
            }
            self.save_attempt_time = now;
            let _ = self.set.save_once(db).await;
            // Monitor events delivered while the values were fetched are
            // cleared with the rest, as C's preemptive callbacks are.
            self.take_events(now);
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
        true
    }

    /// C `connect_list(plist, 1)` (save_restore.c:1276-1374), run while no
    /// method is enabled. Members are database names, so a member is
    /// connected when it resolves, as [`SaveSet::save_once`] reads it.
    fn connect(&self, db: &PvDatabase, mgr: &AutosaveManager) {
        self.set.set_status_text("Connecting PVs...");
        let names = self.set.pv_names();
        let failed: Vec<&String> = names
            .iter()
            .filter(|n| db.get_pv(n.as_str()).is_err())
            .collect();
        if !failed.is_empty() {
            println!("save_restore:connect_list: not all searches successful");
            for name in &failed {
                println!("save_restore: connect failed for channel '{name}'");
            }
        }
        let line = format!(
            "{}: {} of {} PV's connected",
            self.set.save_file(),
            names.len() - failed.len(),
            names.len()
        );
        println!("{line}");
        mgr.note_recent(line);
    }

    /// C `enable_list` (save_restore.c:1425-1487): start what each method
    /// not yet enabled needs. A trigger that cannot be monitored stays
    /// disabled, and the next pass tries again.
    async fn enable(&mut self, db: &PvDatabase, now: Instant, mgr: &AutosaveManager) {
        let method = self.set.save_method();
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
                // An unconnected channel's `ca_add_event` succeeds and
                // waits; a member with no record raises no event here.
                if let Ok(sub) =
                    DbSubscription::subscribe_with_mask(db, &name, 0, CA_DEFAULT_MASK).await
                {
                    sub.activation_handle().enable(db).await;
                    self.members.push(sub);
                }
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
        for member in &mut self.members {
            while member.try_recv_event().is_ok() {
                self.save_state |= CHANGE;
            }
        }
        if self.periodic_due.is_some_and(|due| due <= now) {
            self.periodic_due = None;
            self.save_state |= SaveStrategy::PERIODIC;
        }
        if self.monitor_due.is_some_and(|due| due <= now) {
            self.monitor_due = None;
            self.save_state |= TIMER;
        }
    }
}
