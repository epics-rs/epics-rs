use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::server::database::PvDatabase;
use crate::types::EpicsValue;
use tokio::sync::watch;

use crate::runtime::task::TaskHandle;

use super::error::{AutosaveError, AutosaveResult};
use super::format::CompatMode;
use super::save_file;
use super::save_set::{
    RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, TriggerMode,
};
use super::watermark::{ChangeWatermark, Tick};

/// Builder for constructing an AutosaveManager.
pub struct AutosaveBuilder {
    save_sets: Vec<SaveSetConfig>,
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

    pub fn add_set(mut self, config: SaveSetConfig) -> Self {
        self.save_sets.push(config);
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
            failed: Mutex::new(Vec::new()),
            status_prefix: self.status_prefix,
            shutdown: shutdown_tx,
            compat: self.compat,
            global_macros: self.global_macros,
            runtime: OnceLock::new(),
        };
        for cfg in self.save_sets {
            // Announced and recorded by `add_set`; nothing is left to say.
            let _ = manager.add_set(cfg).await;
        }
        manager
    }
}

/// A built save set and the lock that serialises its saves.
type SetEntry = (Arc<SaveSet>, Arc<tokio::sync::Mutex<()>>);

/// What a set's task needs, captured once by [`AutosaveManager::start`].
struct Runtime {
    reactor: crate::runtime::task::Reactor,
    db: Arc<PvDatabase>,
}

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
    /// The configured sets that never became a [`SaveSet`], by name and
    /// reason. They hold no state to save or restore, so they exist only
    /// to be counted: [`Self::status_all`] is the one place a set that
    /// failed and a set that is working are listed together.
    failed: Mutex<Vec<(String, String)>>,
    status_prefix: Option<String>,
    shutdown: watch::Sender<bool>,
    compat: CompatMode,
    global_macros: HashMap<String, String>,
    runtime: OnceLock<Runtime>,
}

impl std::fmt::Debug for AutosaveManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutosaveManager")
            .field("sets", &self.set_names())
            .field("started", &self.runtime.get().is_some())
            .finish()
    }
}

impl AutosaveManager {
    /// Build one more save set and, once [`Self::start`] has run, start
    /// its task. C `create_data_set`.
    ///
    /// A set that cannot be built is announced on the error log and kept
    /// as [`SaveSetStatus::Error`] for `fdblist`, exactly as one that failed
    /// at build time; the error is also returned so an iocsh caller can
    /// print it.
    pub async fn add_set(&self, mut cfg: SaveSetConfig) -> AutosaveResult<()> {
        // Merge global macros (config overrides globals)
        for (k, v) in &self.global_macros {
            cfg.macros.entry(k.clone()).or_insert_with(|| v.clone());
        }
        let name = cfg.name.clone();
        let set = match SaveSet::new_with_mode(cfg, self.compat).await {
            Ok(set) => Arc::new(set),
            Err(e) => {
                crate::runtime::log::errlog_printf(&format!(
                    "autosave: save set '{name}' not built: {e} - its PVs are not saved\n"
                ));
                self.failed.lock().unwrap().push((name, e.to_string()));
                return Err(e);
            }
        };
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        self.sets.lock().unwrap().push((set.clone(), lock.clone()));
        if let Some(rt) = self.runtime.get() {
            // Detached like the tasks `start` spawns: only `shutdown` ends
            // a set's task, and the outer join `start` returned is the
            // servers' abort target, not the set tasks' owner.
            let _ = self.spawn_set_task(&rt.reactor, set, lock, rt.db.clone());
        }
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

    /// Start all periodic/triggered/onchange tasks. Returns a join handle.
    ///
    /// Takes the [`Reactor`](crate::runtime::task::Reactor), unlike every
    /// deferred record tail: a save writes through `runtime::fs`, whose hosted
    /// implementation is `tokio::task::spawn_blocking` and panics off a runtime
    /// thread, and each strategy is a poller on `runtime::task::interval`,
    /// which is `tokio::time` there. Autosave is started from
    /// `IocApplication::run`, which owns that runtime; it is not on the
    /// synchronous record-processing chain that reaches a runtime-less
    /// connection thread. The parameter is what says so for the *spawns*,
    /// which used to rest on this paragraph alone and which a caller could
    /// not be stopped from ignoring.
    ///
    /// It does not retire the census waiver, and this comment claimed
    /// otherwise until it was checked. The three `runtime::task::interval`
    /// tickers below (`:161`, `:191`, `:221`) still name the ambient seam,
    /// `interval` is on `record_seam_gate::BANNED`, and `EXEMPT` in
    /// `server/mod.rs` is still what lets this file past
    /// `a_deferred_record_tail_never_uses_the_ambient_seam`. Retiring it
    /// needs the delay half carried on the capability too, not another
    /// sentence here.
    pub fn start(
        self: Arc<Self>,
        reactor: &crate::runtime::task::Reactor,
        db: Arc<PvDatabase>,
    ) -> TaskHandle<()> {
        // The first `start` owns the runtime the set tasks spawn onto; a
        // set added after this point is started from `add_set` with it.
        let _ = self.runtime.set(Runtime {
            reactor: reactor.clone(),
            db: db.clone(),
        });
        let outer = reactor.clone();
        reactor.spawn(async move {
            let handles: Vec<_> = self
                .sets()
                .into_iter()
                .filter_map(|(set, lock)| self.spawn_set_task(&outer, set, lock, db.clone()))
                .collect();

            // Wait for all tasks to complete
            for h in handles {
                let _ = h.await;
            }
        })
    }

    /// Spawn the poller for one set; `None` for a [`SaveStrategy::Manual`]
    /// set, which has no task.
    fn spawn_set_task(
        &self,
        reactor: &crate::runtime::task::Reactor,
        set: Arc<SaveSet>,
        lock: Arc<tokio::sync::Mutex<()>>,
        db: Arc<PvDatabase>,
    ) -> Option<TaskHandle<()>> {
        let outer = reactor;
        let mut shutdown_rx = self.shutdown.subscribe();
        let status_prefix = self.status_prefix.clone();

        let handle = match set.config().strategy.clone() {
            SaveStrategy::Periodic { interval } => {
                outer.spawn(async move {
                    let mut ticker = crate::runtime::task::interval(interval);
                    ticker.tick().await; // first tick is immediate, skip it
                    loop {
                        tokio::select! {
                            _ = ticker.tick() => {
                                // No watermark to gate: a Periodic set
                                // re-saves the whole set every interval,
                                // so the next tick retries a failed save
                                // by construction.
                                let _guard = lock.lock().await;
                                let result = set.save_once(&db).await;
                                if let Some(ref prefix) = status_prefix {
                                    update_status_pvs(&db, prefix, &set, &result).await;
                                }
                            }
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                        }
                    }
                })
            }
            SaveStrategy::Triggered {
                trigger_pv,
                mode,
                poll_interval,
            } => {
                outer.spawn(async move {
                    let mut ticker = crate::runtime::task::interval(poll_interval);
                    let mut watermark = ChangeWatermark::new(
                        TriggerState {
                            last_value: None,
                            armed: true, // For NonZero: armed when last was 0
                        },
                        set,
                        lock,
                        db,
                        status_prefix,
                    );

                    loop {
                        tokio::select! {
                            _ = ticker.tick() => {
                                triggered_poll(&mut watermark, &trigger_pv, mode).await;
                            }
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                        }
                    }
                })
            }
            SaveStrategy::OnChange {
                min_interval,
                float_epsilon,
            } => outer.spawn(async move {
                let mut ticker = crate::runtime::task::interval(min_interval);
                let mut watermark =
                    ChangeWatermark::new(HashMap::new(), set, lock, db, status_prefix);

                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            onchange_poll(&mut watermark, float_epsilon).await;
                        }
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
            }),
            SaveStrategy::Manual => {
                // No background task for manual sets
                return None;
            }
        };

        Some(handle)
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

        set.restore_once(db).await
    }

    /// Get status of all save sets, built and unbuilt alike.
    ///
    /// A set whose construction failed reports [`SaveSetStatus::Error`]
    /// permanently - nothing retries it, and omitting it is what let an
    /// IOC that had dropped a save set read as a healthy one.
    pub async fn status_all(&self) -> Vec<(String, SaveSetStatus)> {
        let mut results = Vec::new();
        for (set, _) in self.sets() {
            results.push((set.config().name.clone(), set.status().await));
        }
        for (name, reason) in self.failed.lock().unwrap().iter() {
            results.push((name.clone(), SaveSetStatus::Error(reason.clone())));
        }
        results
    }

    /// Send shutdown signal to all tasks.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
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

    /// A snapshot of the built sets.
    pub fn sets(&self) -> Vec<SetEntry> {
        self.sets.lock().unwrap().clone()
    }
}

/// Compare two stringified values with float epsilon tolerance.
fn values_equal_str(a: &str, b: &str, epsilon: f64) -> bool {
    if a == b {
        return true;
    }
    // Try numeric comparison with epsilon
    if let (Ok(fa), Ok(fb)) = (a.parse::<f64>(), b.parse::<f64>()) {
        return (fa - fb).abs() <= epsilon;
    }
    false
}

/// The `Triggered` arm's watermark: the trigger reading that has been
/// accounted for, plus — for [`TriggerMode::NonZero`] — whether the
/// next non-zero reading is a rising edge.
struct TriggerState {
    last_value: Option<String>,
    armed: bool,
}

/// One poll of a `Triggered` save set.
async fn triggered_poll(
    watermark: &mut ChangeWatermark<TriggerState>,
    trigger_pv: &str,
    mode: TriggerMode,
) {
    let current = watermark.db().get_pv(trigger_pv).ok();
    let current_str = current.as_ref().map(|v| v.to_string());

    let tick = match mode {
        TriggerMode::AnyChange => {
            // L5: the first poll only establishes the baseline
            // (`last_value` is `None`), so no save fires until the
            // trigger PV is next seen to change. This is intentional —
            // it avoids a spurious save at IOC startup — and the
            // latency is bounded by `poll_interval` (1s by default for
            // a `create_triggered_set`).
            let changed = watermark.state().last_value.as_ref() != current_str.as_ref()
                && watermark.state().last_value.is_some();
            let next = TriggerState {
                last_value: current_str,
                armed: watermark.state().armed,
            };
            if changed {
                Tick::Changed(next)
            } else {
                Tick::Unchanged(next)
            }
        }
        TriggerMode::NonZero => {
            let is_nonzero = current
                .as_ref()
                .and_then(|v| v.to_f64())
                .is_some_and(|v| v != 0.0);
            if !is_nonzero {
                Tick::Unchanged(TriggerState {
                    last_value: current_str,
                    armed: true,
                })
            } else if watermark.state().armed && watermark.state().last_value.is_some() {
                Tick::Changed(TriggerState {
                    last_value: current_str,
                    armed: false,
                })
            } else {
                Tick::Unchanged(TriggerState {
                    last_value: current_str,
                    armed: watermark.state().armed,
                })
            }
        }
    };

    watermark.advance(tick).await;
}

/// One poll of an `OnChange` save set.
async fn onchange_poll(
    watermark: &mut ChangeWatermark<HashMap<String, String>>,
    float_epsilon: f64,
) {
    let pv_names = watermark.set().pv_names();
    let mut current_snapshot = HashMap::new();
    let mut changed = false;

    for pv in &pv_names {
        if let Ok(val) = watermark.db().get_pv(pv) {
            let val_str = save_file::value_to_save_str(&val);
            if let Some(old_str) = watermark.state().get(pv)
                && !values_equal_str(old_str, &val_str, float_epsilon)
            {
                changed = true;
            }
            current_snapshot.insert(pv.clone(), val_str);
        }
    }

    // An empty watermark is the pre-baseline state of a freshly
    // started set, not a set whose PVs all vanished.
    let tick = if changed && !watermark.state().is_empty() {
        Tick::Changed(current_snapshot)
    } else {
        Tick::Unchanged(current_snapshot)
    };

    watermark.advance(tick).await;
}

/// Update status PVs after a save cycle.
pub(super) async fn update_status_pvs(
    db: &PvDatabase,
    prefix: &str,
    set: &SaveSet,
    result: &AutosaveResult<usize>,
) {
    let set_name = &set.config().name;
    let status_code = match result {
        Ok(_) => 0i32,
        Err(_) => 2i32,
    };

    let _ = db
        .put_pv_no_process(
            &format!("{prefix}SR_{set_name}_status"),
            EpicsValue::Long(status_code),
        )
        .await;

    if let Ok(count) = result {
        let _ = db
            .put_pv_no_process(
                &format!("{prefix}SR_{set_name}_savedCount"),
                EpicsValue::Long(*count as i32),
            )
            .await;
    }

    let _ = db
        .put_pv_no_process(&format!("{prefix}SR_status"), EpicsValue::Long(status_code))
        .await;

    // Heartbeat
    if let Ok(current) = db.get_pv(&format!("{prefix}SR_heartbeat")) {
        if let Some(v) = current.to_f64() {
            let _ = db
                .put_pv_no_process(
                    &format!("{prefix}SR_heartbeat"),
                    EpicsValue::Long(v as i32 + 1),
                )
                .await;
        }
    }
}
