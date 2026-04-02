use std::collections::HashMap;
use std::sync::Arc;

use crate::server::database::PvDatabase;
use crate::types::EpicsValue;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::error::{AutosaveError, AutosaveResult};
use super::save_file;
use super::save_set::{
    RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, TriggerMode,
};

/// Builder for constructing an AutosaveManager.
pub struct AutosaveBuilder {
    save_sets: Vec<SaveSetConfig>,
    global_macros: HashMap<String, String>,
    status_prefix: Option<String>,
}

impl AutosaveBuilder {
    pub fn new() -> Self {
        Self {
            save_sets: Vec::new(),
            global_macros: HashMap::new(),
            status_prefix: None,
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

    pub async fn build(self) -> AutosaveResult<AutosaveManager> {
        let mut sets = Vec::new();
        for mut cfg in self.save_sets {
            // Merge global macros (config overrides globals)
            for (k, v) in &self.global_macros {
                cfg.macros.entry(k.clone()).or_insert_with(|| v.clone());
            }
            let save_set = SaveSet::new(cfg).await?;
            sets.push((Arc::new(save_set), Arc::new(tokio::sync::Mutex::new(()))));
        }

        let (shutdown_tx, _) = watch::channel(false);

        Ok(AutosaveManager {
            sets,
            status_prefix: self.status_prefix,
            shutdown: shutdown_tx,
        })
    }
}

/// Manages multiple save sets with task orchestration.
pub struct AutosaveManager {
    sets: Vec<(Arc<SaveSet>, Arc<tokio::sync::Mutex<()>>)>,
    status_prefix: Option<String>,
    shutdown: watch::Sender<bool>,
}

impl AutosaveManager {
    /// Restore all save sets. Returns results per set.
    pub async fn restore_all(
        &self,
        db: &PvDatabase,
    ) -> Vec<(String, AutosaveResult<RestoreResult>)> {
        let mut results = Vec::new();
        for (set, _lock) in &self.sets {
            let name = set.config().name.clone();
            let result = set.restore_once(db).await;
            results.push((name, result));
        }
        results
    }

    /// Start all periodic/triggered/onchange tasks. Returns a join handle.
    pub fn start(self: Arc<Self>, db: Arc<PvDatabase>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut handles = Vec::new();

            for (set, lock) in &self.sets {
                let mut shutdown_rx = self.shutdown.subscribe();
                let set = set.clone();
                let lock = lock.clone();
                let db = db.clone();
                let status_prefix = self.status_prefix.clone();

                let handle = match set.config().strategy.clone() {
                    SaveStrategy::Periodic { interval } => {
                        tokio::spawn(async move {
                            let mut ticker = tokio::time::interval(interval);
                            ticker.tick().await; // first tick is immediate, skip it
                            loop {
                                tokio::select! {
                                    _ = ticker.tick() => {
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
                        tokio::spawn(async move {
                            let mut ticker = tokio::time::interval(poll_interval);
                            let mut last_value: Option<String> = None;
                            let mut armed = true; // For NonZero: armed when last was 0

                            loop {
                                tokio::select! {
                                    _ = ticker.tick() => {
                                        let current = db.get_pv(&trigger_pv).await.ok();
                                        let current_str = current.as_ref().map(|v| v.to_string());

                                        let should_save = match mode {
                                            TriggerMode::AnyChange => {
                                                let changed = last_value.as_ref() != current_str.as_ref()
                                                    && last_value.is_some();
                                                last_value = current_str;
                                                changed
                                            }
                                            TriggerMode::NonZero => {
                                                let is_nonzero = current.as_ref()
                                                    .and_then(|v| v.to_f64())
                                                    .map_or(false, |v| v != 0.0);
                                                if !is_nonzero {
                                                    armed = true;
                                                    last_value = current_str;
                                                    false
                                                } else if armed && last_value.is_some() {
                                                    armed = false;
                                                    last_value = current_str;
                                                    true
                                                } else {
                                                    last_value = current_str;
                                                    false
                                                }
                                            }
                                        };

                                        if should_save {
                                            let _guard = lock.lock().await;
                                            let result = set.save_once(&db).await;
                                            if let Some(ref prefix) = status_prefix {
                                                update_status_pvs(&db, prefix, &set, &result).await;
                                            }
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
                    SaveStrategy::OnChange {
                        min_interval,
                        float_epsilon,
                    } => tokio::spawn(async move {
                        let mut ticker = tokio::time::interval(min_interval);
                        let mut last_snapshot: HashMap<String, String> = HashMap::new();

                        loop {
                            tokio::select! {
                                _ = ticker.tick() => {
                                    let pv_names = set.pv_names();
                                    let mut current_snapshot = HashMap::new();
                                    let mut changed = false;

                                    for pv in &pv_names {
                                        if let Ok(val) = db.get_pv(pv).await {
                                            let val_str = save_file::value_to_save_str(&val);
                                            if let Some(old_str) = last_snapshot.get(pv) {
                                                if !values_equal_str(old_str, &val_str, float_epsilon) {
                                                    changed = true;
                                                }
                                            }
                                            current_snapshot.insert(pv.clone(), val_str);
                                        }
                                    }

                                    if changed && !last_snapshot.is_empty() {
                                        let _guard = lock.lock().await;
                                        let result = set.save_once(&db).await;
                                        if let Some(ref prefix) = status_prefix {
                                            update_status_pvs(&db, prefix, &set, &result).await;
                                        }
                                    }
                                    last_snapshot = current_snapshot;
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
                        continue;
                    }
                };

                handles.push(handle);
            }

            // Wait for all tasks to complete
            for h in handles {
                let _ = h.await;
            }
        })
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

    /// Get status of all save sets.
    pub async fn status_all(&self) -> Vec<(String, SaveSetStatus)> {
        let mut results = Vec::new();
        for (set, _) in &self.sets {
            results.push((set.config().name.clone(), set.status().await));
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
            .iter()
            .map(|(s, _)| s.config().name.clone())
            .collect()
    }

    /// Find a save set by name.
    fn find_set(&self, name: &str) -> Option<(Arc<SaveSet>, Arc<tokio::sync::Mutex<()>>)> {
        self.sets
            .iter()
            .find(|(s, _)| s.config().name == name)
            .map(|(s, l)| (s.clone(), l.clone()))
    }

    /// Access sets for testing.
    pub fn sets(&self) -> &[(Arc<SaveSet>, Arc<tokio::sync::Mutex<()>>)] {
        &self.sets
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

/// Update status PVs after a save cycle.
async fn update_status_pvs(
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
    if let Ok(current) = db.get_pv(&format!("{prefix}SR_heartbeat")).await {
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
