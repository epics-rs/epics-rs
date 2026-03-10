use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::client::{CaChannel, CaClient, ConnectionEvent};
use tokio::sync::Notify;

use crate::channel_store::ChannelStore;
use crate::variables::ChannelDef;

/// Active runtime channel: manages CA lifecycle, monitor, and reconnect.
pub struct Channel {
    pub def: ChannelDef,
    pub ch_id: usize,
    ca_channel: Option<CaChannel>,
    connected: Arc<AtomicBool>,
}

impl Channel {
    pub fn new(def: ChannelDef, ch_id: usize) -> Self {
        Self {
            def,
            ch_id,
            ca_channel: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Resolve macro substitutions in the PV name.
    fn resolve_pv_name(&self, macros: &HashMap<String, String>) -> String {
        let mut name = self.def.pv_name.clone();
        for (key, val) in macros {
            name = name.replace(&format!("{{{key}}}"), val);
        }
        name
    }

    /// Connect to the CA server and optionally start monitoring.
    pub async fn connect(
        &mut self,
        ca_client: &CaClient,
        macros: &HashMap<String, String>,
        store: Arc<ChannelStore>,
        dirty_flags: Vec<Arc<Vec<AtomicBool>>>,
        ss_wakeups: Vec<Arc<Notify>>,
        event_flags: Option<Arc<crate::event_flag::EventFlagSet>>,
    ) {
        let pv_name = self.resolve_pv_name(macros);
        if pv_name.is_empty() {
            return;
        }

        let ca_channel = ca_client.create_channel(&pv_name);

        // Wait for initial connection
        if ca_channel
            .wait_connected(Duration::from_secs(5))
            .await
            .is_ok()
        {
            self.connected.store(true, Ordering::Release);
        }

        // Spawn monitor task if this channel is monitored
        if self.def.monitored {
            let ch_id = self.ch_id;
            let connected = self.connected.clone();
            let sync_ef = self.def.sync_ef;

            // Spawn connection watcher
            let mut conn_rx = ca_channel.connection_events();
            let connected_watcher = connected.clone();
            tokio::spawn(async move {
                while let Ok(event) = conn_rx.recv().await {
                    match event {
                        ConnectionEvent::Connected => {
                            connected_watcher.store(true, Ordering::Release);
                        }
                        ConnectionEvent::Disconnected => {
                            connected_watcher.store(false, Ordering::Release);
                        }
                    }
                }
            });

            // Spawn monitor task
            match ca_channel.subscribe().await {
                Ok(mut monitor) => {
                    tokio::spawn(async move {
                        while let Some(result) = monitor.recv().await {
                            if let Ok(value) = result {
                                // Update channel store
                                store.set(ch_id, value);

                                // Mark dirty for all state sets
                                for ss_dirty in &dirty_flags {
                                    if let Some(flag) = ss_dirty.get(ch_id) {
                                        flag.store(true, Ordering::Release);
                                    }
                                }

                                // Set synced event flag if configured
                                if let Some(ef_id) = sync_ef {
                                    if let Some(efs) = &event_flags {
                                        efs.set(ef_id);
                                    }
                                }

                                // Wake all state sets
                                for notify in &ss_wakeups {
                                    notify.notify_one();
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to subscribe to {pv_name}: {e}");
                }
            }
        }

        self.ca_channel = Some(ca_channel);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn ca_channel(&self) -> Option<&CaChannel> {
        self.ca_channel.as_ref()
    }
}
