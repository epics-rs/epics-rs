use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::client::CaChannel;
use tokio::sync::{Mutex as TokioMutex, Notify};

use crate::channel::Channel;
use crate::channel_store::ChannelStore;
use crate::error::{PvOpResult, PvStat, SeqError, SeqResult};
use crate::event_flag::EventFlagSet;
use crate::variables::ProgramVars;

/// Completion type for pvGet/pvPut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompType {
    /// Default: synchronous for pvGet, fire-and-forget for pvPut.
    Default,
    /// Synchronous: block until complete.
    Sync,
    /// Asynchronous: start in background, check with pvGetComplete/pvPutComplete.
    Async,
}

/// Per-channel async operation slot.
struct AsyncOpSlot {
    pending: bool,
    completed: bool,
    result: Option<PvOpResult>,
}

impl AsyncOpSlot {
    fn new() -> Self {
        Self {
            pending: false,
            completed: false,
            result: None,
        }
    }

    fn reset(&mut self) {
        self.pending = false;
        self.completed = false;
        self.result = None;
    }
}

/// Context for a single state set's execution.
///
/// Each state set runs as an independent tokio task with its own
/// local variable snapshot. The main loop evaluates `when` conditions
/// and performs state transitions.
pub struct StateSetContext<V: ProgramVars> {
    /// Local variable snapshot — updated from channel store at sync points.
    pub local_vars: V,
    /// State set index.
    pub ss_id: usize,
    /// Current state index.
    current_state: usize,
    /// Transition target (set by `transition_to`, consumed by main loop).
    next_state: Option<usize>,
    /// Previous state (for entry/exit guard).
    prev_state: Option<usize>,
    /// Time when current state was entered.
    time_entered: Instant,
    /// Minimum wakeup timeout for next iteration.
    next_wakeup: Option<Duration>,
    /// Per-channel dirty flags for this state set.
    dirty: Arc<Vec<AtomicBool>>,
    /// Wakeup notification for this state set.
    wakeup: Arc<Notify>,
    /// Shared channel store.
    store: Arc<ChannelStore>,
    /// Shared channels (for pvGet/pvPut).
    channels: Arc<Vec<Channel>>,
    /// Event flag set.
    event_flags: Arc<EventFlagSet>,
    /// Shutdown signal.
    shutdown: Arc<AtomicBool>,
    /// Per-channel async GET operation slots.
    get_slots: Vec<Arc<TokioMutex<AsyncOpSlot>>>,
    /// Per-channel async PUT operation slots.
    put_slots: Vec<Arc<TokioMutex<AsyncOpSlot>>>,
    /// Per-channel last operation result (shared by get/put, whichever completed last).
    last_op_result: Vec<PvOpResult>,
}

impl<V: ProgramVars> StateSetContext<V> {
    pub fn new(
        initial_vars: V,
        ss_id: usize,
        num_channels: usize,
        wakeup: Arc<Notify>,
        store: Arc<ChannelStore>,
        channels: Arc<Vec<Channel>>,
        event_flags: Arc<EventFlagSet>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let dirty: Vec<AtomicBool> = (0..num_channels).map(|_| AtomicBool::new(false)).collect();
        let get_slots = (0..num_channels)
            .map(|_| Arc::new(TokioMutex::new(AsyncOpSlot::new())))
            .collect();
        let put_slots = (0..num_channels)
            .map(|_| Arc::new(TokioMutex::new(AsyncOpSlot::new())))
            .collect();
        let last_op_result = (0..num_channels).map(|_| PvOpResult::default()).collect();

        Self {
            local_vars: initial_vars,
            ss_id,
            current_state: 0,
            next_state: None,
            prev_state: None,
            time_entered: Instant::now(),
            next_wakeup: None,
            dirty: Arc::new(dirty),
            wakeup,
            store,
            channels,
            event_flags,
            shutdown,
            get_slots,
            put_slots,
            last_op_result,
        }
    }

    /// Get a clone of the dirty flags Arc (needed for monitor setup).
    pub fn dirty_flags(&self) -> Arc<Vec<AtomicBool>> {
        self.dirty.clone()
    }

    // --- State machine control ---

    /// Get the current state index.
    pub fn current_state(&self) -> usize {
        self.current_state
    }

    /// Signal a transition to a new state. The transition completes
    /// after the current when-action finishes.
    pub fn transition_to(&mut self, state: usize) {
        self.next_state = Some(state);
    }

    /// Check if a transition is pending.
    pub fn has_transition(&self) -> bool {
        self.next_state.is_some()
    }

    /// Check if shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    // --- delay ---

    /// SNL `delay(t)` — returns true if enough time has elapsed since
    /// entering the current state.
    ///
    /// If not yet elapsed, registers a wakeup at the remaining time so
    /// the state set re-evaluates when the delay expires.
    pub fn delay(&mut self, seconds: f64) -> bool {
        let target = Duration::from_secs_f64(seconds);
        let elapsed = self.time_entered.elapsed();
        if elapsed >= target {
            true
        } else {
            let remaining = target - elapsed;
            self.next_wakeup = Some(match self.next_wakeup {
                Some(current) => current.min(remaining),
                None => remaining,
            });
            false
        }
    }

    // --- PV operations ---

    /// pvGet: read value from IOC via CA.
    ///
    /// - `Default`/`Sync`: synchronous with 5s timeout. Returns PvStat.
    /// - `Async`: starts background task. Check with `pv_get_complete()`.
    pub async fn pv_get(&mut self, ch_id: usize, comp: CompType) -> PvStat {
        match comp {
            CompType::Async => self.pv_get_async(ch_id).await,
            CompType::Default | CompType::Sync => self.pv_get_sync(ch_id).await,
        }
    }

    async fn pv_get_sync(&mut self, ch_id: usize) -> PvStat {
        let ca_ch = match self.get_ca_channel(ch_id) {
            Ok(ch) => ch,
            Err(_) => {
                let result = PvOpResult {
                    stat: PvStat::Disconnected,
                    severity: 3,
                    message: Some("channel not connected".into()),
                };
                self.update_last_op_result(ch_id, result);
                return PvStat::Disconnected;
            }
        };

        let timeout = tokio::time::timeout(Duration::from_secs(5), ca_ch.get()).await;
        match timeout {
            Ok(Ok((_dbr, value))) => {
                self.store.set(ch_id, value.clone());
                self.local_vars.set_channel_value(ch_id, &value);
                let result = PvOpResult::default();
                self.update_last_op_result(ch_id, result);
                PvStat::Ok
            }
            Ok(Err(e)) => {
                let result = PvOpResult {
                    stat: PvStat::Error,
                    severity: 3,
                    message: Some(format!("{e}")),
                };
                self.update_last_op_result(ch_id, result);
                PvStat::Error
            }
            Err(_) => {
                let result = PvOpResult {
                    stat: PvStat::Timeout,
                    severity: 3,
                    message: Some("pvGet timeout (5s)".into()),
                };
                self.update_last_op_result(ch_id, result);
                PvStat::Timeout
            }
        }
    }

    async fn pv_get_async(&mut self, ch_id: usize) -> PvStat {
        let slot = match self.get_slots.get(ch_id) {
            Some(s) => s.clone(),
            None => return PvStat::Error,
        };

        {
            let mut s = slot.lock().await;
            if s.pending {
                return PvStat::Error; // already pending
            }
            s.pending = true;
            s.completed = false;
            s.result = None;
        }

        let ca_ch = match self.get_ca_channel(ch_id) {
            Ok(ch) => ch.clone(),
            Err(_) => {
                let mut s = slot.lock().await;
                s.pending = false;
                s.completed = true;
                s.result = Some(PvOpResult {
                    stat: PvStat::Disconnected,
                    severity: 3,
                    message: Some("channel not connected".into()),
                });
                return PvStat::Disconnected;
            }
        };

        let store = self.store.clone();
        let wakeup = self.wakeup.clone();

        tokio::spawn(async move {
            let result = ca_ch.get().await;
            let mut s = slot.lock().await;
            if !s.pending {
                return; // cancelled
            }
            match result {
                Ok((_dbr, value)) => {
                    store.set(ch_id, value);
                    s.result = Some(PvOpResult::default());
                }
                Err(e) => {
                    s.result = Some(PvOpResult {
                        stat: PvStat::Error,
                        severity: 3,
                        message: Some(format!("{e}")),
                    });
                }
            }
            s.pending = false;
            s.completed = true;
            wakeup.notify_one();
        });

        PvStat::Ok
    }

    /// pvPut: write local var value to IOC via CA.
    ///
    /// - `Default`: fire-and-forget (start put, don't wait). Returns PvStat.
    /// - `Sync`: synchronous with 5s timeout.
    /// - `Async`: starts background task. Check with `pv_put_complete()`.
    pub async fn pv_put(&mut self, ch_id: usize, comp: CompType) -> PvStat {
        match comp {
            CompType::Async => self.pv_put_async(ch_id).await,
            CompType::Sync => self.pv_put_sync(ch_id).await,
            CompType::Default => self.pv_put_default(ch_id).await,
        }
    }

    async fn pv_put_default(&mut self, ch_id: usize) -> PvStat {
        let value = self.local_vars.get_channel_value(ch_id);
        self.store.set(ch_id, value.clone());

        let ca_ch = match self.get_ca_channel(ch_id) {
            Ok(ch) => ch,
            Err(_) => {
                let result = PvOpResult {
                    stat: PvStat::Disconnected,
                    severity: 3,
                    message: Some("channel not connected".into()),
                };
                self.update_last_op_result(ch_id, result);
                return PvStat::Disconnected;
            }
        };

        match ca_ch.put(&value).await {
            Ok(()) => {
                let result = PvOpResult::default();
                self.update_last_op_result(ch_id, result);
                PvStat::Ok
            }
            Err(e) => {
                let result = PvOpResult {
                    stat: PvStat::Error,
                    severity: 3,
                    message: Some(format!("{e}")),
                };
                self.update_last_op_result(ch_id, result);
                PvStat::Error
            }
        }
    }

    async fn pv_put_sync(&mut self, ch_id: usize) -> PvStat {
        let value = self.local_vars.get_channel_value(ch_id);
        self.store.set(ch_id, value.clone());

        let ca_ch = match self.get_ca_channel(ch_id) {
            Ok(ch) => ch,
            Err(_) => {
                let result = PvOpResult {
                    stat: PvStat::Disconnected,
                    severity: 3,
                    message: Some("channel not connected".into()),
                };
                self.update_last_op_result(ch_id, result);
                return PvStat::Disconnected;
            }
        };

        let timeout = tokio::time::timeout(Duration::from_secs(5), ca_ch.put(&value)).await;
        match timeout {
            Ok(Ok(())) => {
                let result = PvOpResult::default();
                self.update_last_op_result(ch_id, result);
                PvStat::Ok
            }
            Ok(Err(e)) => {
                let result = PvOpResult {
                    stat: PvStat::Error,
                    severity: 3,
                    message: Some(format!("{e}")),
                };
                self.update_last_op_result(ch_id, result);
                PvStat::Error
            }
            Err(_) => {
                let result = PvOpResult {
                    stat: PvStat::Timeout,
                    severity: 3,
                    message: Some("pvPut timeout (5s)".into()),
                };
                self.update_last_op_result(ch_id, result);
                PvStat::Timeout
            }
        }
    }

    async fn pv_put_async(&mut self, ch_id: usize) -> PvStat {
        let slot = match self.put_slots.get(ch_id) {
            Some(s) => s.clone(),
            None => return PvStat::Error,
        };

        let value = self.local_vars.get_channel_value(ch_id);
        self.store.set(ch_id, value.clone());

        {
            let mut s = slot.lock().await;
            if s.pending {
                return PvStat::Error; // already pending
            }
            s.pending = true;
            s.completed = false;
            s.result = None;
        }

        let ca_ch = match self.get_ca_channel(ch_id) {
            Ok(ch) => ch.clone(),
            Err(_) => {
                let mut s = slot.lock().await;
                s.pending = false;
                s.completed = true;
                s.result = Some(PvOpResult {
                    stat: PvStat::Disconnected,
                    severity: 3,
                    message: Some("channel not connected".into()),
                });
                return PvStat::Disconnected;
            }
        };

        let wakeup = self.wakeup.clone();

        tokio::spawn(async move {
            let result = ca_ch.put(&value).await;
            let mut s = slot.lock().await;
            if !s.pending {
                return; // cancelled
            }
            match result {
                Ok(()) => {
                    s.result = Some(PvOpResult::default());
                }
                Err(e) => {
                    s.result = Some(PvOpResult {
                        stat: PvStat::Error,
                        severity: 3,
                        message: Some(format!("{e}")),
                    });
                }
            }
            s.pending = false;
            s.completed = true;
            wakeup.notify_one();
        });

        PvStat::Ok
    }

    /// Check if async pvGet has completed. If safe mode, copies value from store.
    /// Returns true if complete (idempotent — can be called multiple times).
    pub async fn pv_get_complete(&mut self, ch_id: usize) -> bool {
        let slot = match self.get_slots.get(ch_id) {
            Some(s) => s.clone(),
            None => return true, // invalid ch_id, treat as complete
        };

        let s = slot.lock().await;
        if s.pending {
            return false;
        }
        if s.completed {
            // Copy result to last_op_result
            if let Some(ref r) = s.result {
                self.update_last_op_result(ch_id, r.clone());
            }
            // In safe mode, copy value from store to local vars
            if let Some(value) = self.store.get(ch_id) {
                self.local_vars.set_channel_value(ch_id, &value);
            }
            return true;
        }
        true // neither pending nor completed = never started, treat as complete
    }

    /// Check if async pvPut has completed.
    pub async fn pv_put_complete(&mut self, ch_id: usize) -> bool {
        let slot = match self.put_slots.get(ch_id) {
            Some(s) => s.clone(),
            None => return true,
        };

        let s = slot.lock().await;
        if s.pending {
            return false;
        }
        if s.completed {
            if let Some(ref r) = s.result {
                self.update_last_op_result(ch_id, r.clone());
            }
            return true;
        }
        true
    }

    /// Cancel pending async pvGet.
    pub async fn pv_get_cancel(&mut self, ch_id: usize) {
        if let Some(slot) = self.get_slots.get(ch_id) {
            let mut s = slot.lock().await;
            s.reset();
        }
    }

    /// Cancel pending async pvPut.
    pub async fn pv_put_cancel(&mut self, ch_id: usize) {
        if let Some(slot) = self.put_slots.get(ch_id) {
            let mut s = slot.lock().await;
            s.reset();
        }
    }

    /// Get the PvStat of the last completed operation on a channel.
    pub fn pv_status(&self, ch_id: usize) -> PvStat {
        self.last_op_result
            .get(ch_id)
            .map_or(PvStat::Ok, |r| r.stat)
    }

    /// Get the severity of the last completed operation on a channel.
    pub fn pv_severity(&self, ch_id: usize) -> i16 {
        self.last_op_result
            .get(ch_id)
            .map_or(0, |r| r.severity)
    }

    /// Get the message of the last completed operation on a channel.
    pub fn pv_message(&self, ch_id: usize) -> Option<&str> {
        self.last_op_result
            .get(ch_id)
            .and_then(|r| r.message.as_deref())
    }

    fn update_last_op_result(&mut self, ch_id: usize, result: PvOpResult) {
        if ch_id < self.last_op_result.len() {
            self.last_op_result[ch_id] = result;
        }
    }

    fn get_ca_channel(&self, ch_id: usize) -> SeqResult<&CaChannel> {
        let channel = self
            .channels
            .get(ch_id)
            .ok_or(SeqError::InvalidChannelId(ch_id))?;
        channel
            .ca_channel()
            .ok_or_else(|| SeqError::NotConnected(channel.def.pv_name.clone()))
    }

    // --- Event flags ---

    /// Set an event flag.
    pub fn ef_set(&self, ef_id: usize) {
        self.event_flags.set(ef_id);
    }

    /// Test an event flag. If true, also performs selective sync of
    /// channels synced to this flag.
    pub fn ef_test(&mut self, ef_id: usize) -> bool {
        let result = self.event_flags.test(ef_id);
        if result {
            self.sync_channels_for_flag(ef_id);
        }
        result
    }

    /// Clear an event flag.
    pub fn ef_clear(&self, ef_id: usize) -> bool {
        self.event_flags.clear(ef_id)
    }

    /// Test and clear an event flag atomically. If was set,
    /// also performs selective sync.
    pub fn ef_test_and_clear(&mut self, ef_id: usize) -> bool {
        let was_set = self.event_flags.test_and_clear(ef_id);
        if was_set {
            self.sync_channels_for_flag(ef_id);
        }
        was_set
    }

    /// Sync only the channels associated with a given event flag.
    fn sync_channels_for_flag(&mut self, ef_id: usize) {
        let ch_ids = self.event_flags.synced_channels(ef_id).to_vec();
        for ch_id in ch_ids {
            if let Some(value) = self.store.get(ch_id) {
                self.local_vars.set_channel_value(ch_id, &value);
            }
        }
    }

    // --- Connection status ---

    /// Check if a specific channel is connected.
    pub fn pv_connected(&self, ch_id: usize) -> bool {
        self.channels
            .get(ch_id)
            .map_or(false, |ch| ch.is_connected())
    }

    /// Count of connected channels.
    pub fn pv_connect_count(&self) -> usize {
        self.channels.iter().filter(|ch| ch.is_connected()).count()
    }

    /// Total channel count.
    pub fn pv_channel_count(&self) -> usize {
        self.channels.len()
    }

    // --- Dirty sync ---

    /// Synchronize all dirty channel values from the store into local_vars.
    /// Called once per when-evaluation cycle to maintain snapshot atomicity.
    pub fn sync_dirty_vars(&mut self) {
        for ch_id in 0..self.dirty.len() {
            if let Some(flag) = self.dirty.get(ch_id) {
                if flag.swap(false, Ordering::AcqRel) {
                    if let Some(value) = self.store.get(ch_id) {
                        self.local_vars.set_channel_value(ch_id, &value);
                    }
                }
            }
        }
    }

    // --- Main loop helpers ---

    /// Reset wakeup timer for a new evaluation cycle.
    pub fn reset_wakeup(&mut self) {
        self.next_wakeup = None;
    }

    /// Wait for the next wakeup event (notification, timeout, or shutdown).
    pub async fn wait_for_wakeup(&self) {
        match self.next_wakeup {
            Some(timeout) => {
                tokio::select! {
                    _ = self.wakeup.notified() => {}
                    _ = tokio::time::sleep(timeout) => {}
                }
            }
            None => {
                // No delay pending — wait indefinitely for a notification
                self.wakeup.notified().await;
            }
        }
    }

    /// Enter a new state: reset time_entered and clear transition.
    pub fn enter_state(&mut self, state: usize) {
        self.prev_state = if state != self.current_state {
            Some(self.current_state)
        } else {
            self.prev_state
        };
        self.current_state = state;
        self.next_state = None;
        self.time_entered = Instant::now();
    }

    /// Should entry actions run? (prev_state != current_state)
    pub fn should_run_entry(&self) -> bool {
        self.prev_state.map_or(true, |prev| prev != self.current_state)
    }

    /// Should exit actions run? (next_state != current_state)
    pub fn should_run_exit(&self) -> bool {
        self.next_state.map_or(false, |next| next != self.current_state)
    }

    /// Consume the pending transition, returning the target state.
    pub fn take_transition(&mut self) -> Option<usize> {
        self.next_state.take()
    }

    /// Get the wakeup Notify (for initial trigger).
    pub fn wakeup(&self) -> &Arc<Notify> {
        &self.wakeup
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;
    use crate::channel_store::ChannelStore;
    use crate::event_flag::EventFlagSet;
    use crate::variables::ProgramVars;
    use epics_base_rs::types::EpicsValue;

    #[derive(Clone)]
    struct TestVars {
        values: Vec<f64>,
    }

    impl ProgramVars for TestVars {
        fn get_channel_value(&self, ch_id: usize) -> EpicsValue {
            EpicsValue::Double(self.values.get(ch_id).copied().unwrap_or(0.0))
        }
        fn set_channel_value(&mut self, ch_id: usize, value: &EpicsValue) {
            if let Some(v) = value.to_f64() {
                if ch_id < self.values.len() {
                    self.values[ch_id] = v;
                }
            }
        }
    }

    fn make_ctx(num_channels: usize) -> StateSetContext<TestVars> {
        let vars = TestVars {
            values: vec![0.0; num_channels],
        };
        let wakeup = Arc::new(Notify::new());
        let store = Arc::new(ChannelStore::new(num_channels));
        let channels = Arc::new(Vec::<Channel>::new());
        let efs = Arc::new(EventFlagSet::new(
            1,
            vec![vec![0]],
            vec![wakeup.clone()],
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        StateSetContext::new(vars, 0, num_channels, wakeup, store, channels, efs, shutdown)
    }

    #[test]
    fn test_delay_not_elapsed() {
        let mut ctx = make_ctx(0);
        assert!(!ctx.delay(10.0));
        assert!(ctx.next_wakeup.is_some());
    }

    #[test]
    fn test_delay_elapsed() {
        let mut ctx = make_ctx(0);
        // Artificially set time_entered to the past
        ctx.time_entered = Instant::now() - std::time::Duration::from_secs(5);
        assert!(ctx.delay(3.0));
    }

    #[test]
    fn test_state_transitions() {
        let mut ctx = make_ctx(0);
        ctx.enter_state(0);
        assert_eq!(ctx.current_state(), 0);
        assert!(ctx.should_run_entry()); // first time: prev_state is None

        ctx.transition_to(1);
        assert!(ctx.has_transition());
        assert!(ctx.should_run_exit()); // 1 != 0

        let next = ctx.take_transition().unwrap();
        assert_eq!(next, 1);
        ctx.enter_state(next);
        assert_eq!(ctx.current_state(), 1);
        assert!(ctx.should_run_entry()); // prev=0, current=1
    }

    #[test]
    fn test_self_transition_no_entry_exit() {
        let mut ctx = make_ctx(0);
        ctx.enter_state(0);
        // simulate a self-transition
        ctx.transition_to(0);
        assert!(!ctx.should_run_exit()); // next(0) == current(0)
    }

    #[test]
    fn test_sync_dirty_vars() {
        let mut ctx = make_ctx(2);
        // Simulate a monitor update
        ctx.store.set(0, EpicsValue::Double(42.0));
        ctx.dirty.get(0).unwrap().store(true, Ordering::Release);

        assert!((ctx.local_vars.values[0] - 0.0).abs() < 1e-10);
        ctx.sync_dirty_vars();
        assert!((ctx.local_vars.values[0] - 42.0).abs() < 1e-10);
        // Channel 1 should be unchanged
        assert!((ctx.local_vars.values[1] - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_ef_set_and_test() {
        let mut ctx = make_ctx(1);
        assert!(!ctx.ef_test(0));
        ctx.ef_set(0);
        assert!(ctx.ef_test(0));
    }

    #[test]
    fn test_ef_test_and_clear() {
        let mut ctx = make_ctx(1);
        ctx.ef_set(0);
        // Store a value to test selective sync
        ctx.store.set(0, EpicsValue::Double(99.0));
        assert!(ctx.ef_test_and_clear(0));
        // Flag should be cleared
        assert!(!ctx.ef_test(0));
        // Channel 0 should have been synced
        assert!((ctx.local_vars.values[0] - 99.0).abs() < 1e-10);
    }

    #[test]
    fn test_shutdown() {
        let ctx = make_ctx(0);
        assert!(!ctx.is_shutdown());
        ctx.shutdown.store(true, Ordering::Release);
        assert!(ctx.is_shutdown());
    }

    #[test]
    fn test_pv_status_default() {
        let ctx = make_ctx(2);
        assert_eq!(ctx.pv_status(0), PvStat::Ok);
        assert_eq!(ctx.pv_severity(0), 0);
        assert_eq!(ctx.pv_message(0), None);
    }

    #[test]
    fn test_pv_status_after_update() {
        let mut ctx = make_ctx(2);
        ctx.update_last_op_result(
            0,
            PvOpResult {
                stat: PvStat::Timeout,
                severity: 3,
                message: Some("timeout".into()),
            },
        );
        assert_eq!(ctx.pv_status(0), PvStat::Timeout);
        assert_eq!(ctx.pv_severity(0), 3);
        assert_eq!(ctx.pv_message(0), Some("timeout"));
        // Channel 1 should be unaffected
        assert_eq!(ctx.pv_status(1), PvStat::Ok);
    }

    #[test]
    fn test_pv_status_invalid_channel() {
        let ctx = make_ctx(1);
        assert_eq!(ctx.pv_status(99), PvStat::Ok);
        assert_eq!(ctx.pv_severity(99), 0);
        assert_eq!(ctx.pv_message(99), None);
    }

    #[tokio::test]
    async fn test_pv_get_disconnected() {
        let mut ctx = make_ctx(1);
        // No CA channels connected, so pv_get should return Disconnected
        let stat = ctx.pv_get(0, CompType::Sync).await;
        assert_eq!(stat, PvStat::Disconnected);
        assert_eq!(ctx.pv_status(0), PvStat::Disconnected);
    }

    #[tokio::test]
    async fn test_pv_put_disconnected() {
        let mut ctx = make_ctx(1);
        let stat = ctx.pv_put(0, CompType::Default).await;
        assert_eq!(stat, PvStat::Disconnected);
    }

    #[tokio::test]
    async fn test_async_get_complete_no_channel() {
        let mut ctx = make_ctx(1);
        // Async get on disconnected channel should immediately complete
        let stat = ctx.pv_get(0, CompType::Async).await;
        assert_eq!(stat, PvStat::Disconnected);
        // Should be complete
        assert!(ctx.pv_get_complete(0).await);
    }

    #[tokio::test]
    async fn test_async_get_cancel() {
        let mut ctx = make_ctx(1);
        // Cancel a non-pending op should be safe
        ctx.pv_get_cancel(0).await;
        assert!(ctx.pv_get_complete(0).await);
    }

    #[tokio::test]
    async fn test_async_put_complete_no_channel() {
        let mut ctx = make_ctx(1);
        let stat = ctx.pv_put(0, CompType::Async).await;
        assert_eq!(stat, PvStat::Disconnected);
        assert!(ctx.pv_put_complete(0).await);
    }
}
