use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use epics_base_rs::client::CaClient;
use tokio::sync::Notify;

use crate::channel::Channel;
use crate::channel_store::ChannelStore;
use crate::error::SeqResult;
use crate::event_flag::EventFlagSet;
use crate::state_set::StateSetContext;
use crate::variables::{ProgramMeta, ProgramVars};

/// Shared state across all state sets in a program.
pub struct ProgramShared<V: ProgramVars> {
    pub store: Arc<ChannelStore>,
    pub channels: Arc<Vec<Channel>>,
    pub event_flags: Arc<EventFlagSet>,
    pub shutdown: Arc<AtomicBool>,
    pub ss_wakeups: Vec<Arc<Notify>>,
    pub _phantom: std::marker::PhantomData<V>,
}

/// Type alias for a state set function.
///
/// Each state set is an async function that takes a `StateSetContext`
/// and runs the state machine loop until shutdown.
pub type StateSetFn<V> =
    Box<dyn Fn(StateSetContext<V>) -> Pin<Box<dyn Future<Output = SeqResult<()>> + Send>> + Send + Sync>;

/// Builder for constructing and running a sequencer program.
pub struct ProgramBuilder<V: ProgramVars, M: ProgramMeta> {
    pub name: String,
    pub initial_vars: V,
    pub macros: HashMap<String, String>,
    pub state_set_fns: Vec<StateSetFn<V>>,
    _meta: std::marker::PhantomData<M>,
}

impl<V: ProgramVars, M: ProgramMeta> ProgramBuilder<V, M> {
    pub fn new(name: &str, initial_vars: V) -> Self {
        Self {
            name: name.to_string(),
            initial_vars,
            macros: HashMap::new(),
            state_set_fns: Vec::new(),
            _meta: std::marker::PhantomData,
        }
    }

    pub fn macros(mut self, macro_str: &str) -> Self {
        self.macros = crate::macros::parse_macros(macro_str);
        self
    }

    pub fn add_ss(mut self, f: StateSetFn<V>) -> Self {
        self.state_set_fns.push(f);
        self
    }

    /// Build and run the program. Blocks until all state sets finish or shutdown.
    pub async fn run(self) -> SeqResult<()> {
        let num_channels = M::NUM_CHANNELS;
        let num_flags = M::NUM_EVENT_FLAGS;
        let num_ss = self.state_set_fns.len();

        tracing::info!("starting program '{}' with {} state sets, {} channels, {} event flags",
            self.name, num_ss, num_channels, num_flags);

        // Create CA client
        let ca_client = CaClient::new()
            .await
            .map_err(|e| crate::error::SeqError::Other(format!("CA init failed: {e}")))?;

        // Create shared state
        let store = Arc::new(ChannelStore::new(num_channels));
        let shutdown = Arc::new(AtomicBool::new(false));

        // Create per-SS wakeup notifiers
        let ss_wakeups: Vec<Arc<Notify>> = (0..num_ss).map(|_| Arc::new(Notify::new())).collect();

        // Create event flag set
        let sync_map = M::event_flag_sync_map();
        let event_flags = Arc::new(EventFlagSet::new(num_flags, sync_map, ss_wakeups.clone()));

        // Create per-SS dirty flags
        let dirty_per_ss: Vec<Arc<Vec<AtomicBool>>> = (0..num_ss)
            .map(|_| {
                Arc::new(
                    (0..num_channels)
                        .map(|_| AtomicBool::new(false))
                        .collect(),
                )
            })
            .collect();

        // Create and connect channels
        let channel_defs = M::channel_defs();
        let mut channels: Vec<Channel> = channel_defs
            .into_iter()
            .enumerate()
            .map(|(id, def)| Channel::new(def, id))
            .collect();

        for ch in &mut channels {
            ch.connect(
                &ca_client,
                &self.macros,
                store.clone(),
                dirty_per_ss.clone(),
                ss_wakeups.clone(),
                Some(event_flags.clone()),
            )
            .await;
        }

        let channels = Arc::new(channels);

        // Spawn state set tasks
        let mut handles = Vec::new();
        for (ss_id, ss_fn) in self.state_set_fns.into_iter().enumerate() {
            let ctx = StateSetContext::new(
                self.initial_vars.clone(),
                ss_id,
                num_channels,
                ss_wakeups[ss_id].clone(),
                store.clone(),
                channels.clone(),
                event_flags.clone(),
                shutdown.clone(),
            );

            let handle = tokio::spawn(async move {
                if let Err(e) = ss_fn(ctx).await {
                    tracing::error!("state set {ss_id} error: {e}");
                }
            });
            handles.push(handle);
        }

        // Wait for all state sets to complete
        for handle in handles {
            let _ = handle.await;
        }

        tracing::info!("program '{}' finished", self.name);
        Ok(())
    }
}
