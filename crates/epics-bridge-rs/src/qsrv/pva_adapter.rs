//! Adapter that exposes a [`BridgeProvider`] (qsrv) through the native
//! [`epics_pva_rs::server_native::ChannelSource`] trait so that the native
//! PVA server can serve EPICS records (single-record and group composite
//! PVs) plus NTNDArray plugin PVs over pvAccess.
//!
//! All values flow through [`epics_pva_rs::pvdata::PvField`] end-to-end —
//! no spvirit_* types appear in this module.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};

use epics_pva_rs::pvdata::{PvField, PvStructure};

use super::provider::{AnyChannel, BridgeProvider, Channel, ChannelProvider, PvaMonitor};

/// Handle for a native PVA PV: latest snapshot + subscriber list.
///
/// Registered via [`QsrvPvStore::register_pva_pv`] so the native PVA
/// server can serve structure-shaped values produced directly by IOC code
/// (for example NTNDArray or aggregate benchmark PVs). Snapshots and
/// notifications use native [`PvField`] values; no spvirit dependency.
#[derive(Clone)]
pub struct PvaPvHandle {
    pub latest: Arc<parking_lot::Mutex<Option<PvField>>>,
    pub subscribers: Arc<parking_lot::Mutex<Vec<mpsc::Sender<PvField>>>>,
}

// ---------------------------------------------------------------------------
// Global PVA PV registry — IOC code stores handles here during startup,
// the CA+PVA runner reads them at server startup.
// ---------------------------------------------------------------------------

static PVA_PV_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PvaPvHandle>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Register a native PVA PV before the CA+PVA runner starts.
pub fn register_pva_pv_global(pv_name: &str, handle: PvaPvHandle) {
    PVA_PV_REGISTRY
        .lock()
        .unwrap()
        .insert(pv_name.to_string(), handle);
}

/// Take all registered native PVA PVs. Called by [`run_ca_pva_qsrv_ioc`]
/// to wire them into `QsrvPvStore`.
pub fn take_registered_pva_pvs() -> std::collections::HashMap<String, PvaPvHandle> {
    std::mem::take(&mut *PVA_PV_REGISTRY.lock().unwrap())
}

/// PvStore implementation backed by a qsrv [`BridgeProvider`].
///
/// Handles single-record PVs, group composite PVs, and native PVA PVs
/// (NTNDArray from areaDetector). Group PVs ride on the
/// `NtPayload::Generic` variant with a recursive `PvValue` tree.
pub struct QsrvPvStore {
    provider: Arc<BridgeProvider>,
    /// Per-PV cache of opened channels.
    channels: RwLock<HashMap<String, Arc<AnyChannel>>>,
    /// Native PVA PVs (e.g., NTNDArray from NDPluginPva).
    pva_pvs: Arc<RwLock<HashMap<String, PvaPvHandle>>>,
}

impl QsrvPvStore {
    pub fn new(provider: Arc<BridgeProvider>) -> Self {
        Self {
            provider,
            channels: RwLock::new(HashMap::new()),
            pva_pvs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn provider(&self) -> &Arc<BridgeProvider> {
        &self.provider
    }

    /// Register a native PVA PV (e.g., NTNDArray from NDPluginPva).
    ///
    /// After registration, the PV is discoverable via `has_pv`, readable
    /// via `get_snapshot`, and subscribable via `subscribe`.
    pub async fn register_pva_pv(
        &self,
        pv_name: &str,
        latest: Arc<parking_lot::Mutex<Option<PvField>>>,
        subscribers: Arc<parking_lot::Mutex<Vec<mpsc::Sender<PvField>>>>,
    ) {
        self.pva_pvs.write().await.insert(
            pv_name.to_string(),
            PvaPvHandle {
                latest,
                subscribers,
            },
        );
    }

    async fn channel(&self, name: &str) -> Option<Arc<AnyChannel>> {
        if let Some(c) = self.channels.read().await.get(name) {
            return Some(c.clone());
        }
        let fresh = self.provider.create_channel(name).await.ok()?;
        let arc = Arc::new(fresh);
        self.channels
            .write()
            .await
            .insert(name.to_string(), arc.clone());
        Some(arc)
    }
}

// ── ChannelSource impl (native PvAccess server) ──────────────────────────
//
// In addition to the legacy spvirit `PvStore` impl above, expose the same
// data via the native [`epics_pva_rs::server_native::ChannelSource`] trait.
// This is the path used by `epics_pva_rs::server::PvaServer::run_with_source`
// (no spvirit_server runtime involvement).

impl epics_pva_rs::server_native::ChannelSource for QsrvPvStore {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            let mut names = provider.channel_list().await;
            for key in pva_pvs.read().await.keys() {
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
            names.sort();
            names
        }
    }

    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        let name = name.to_string();
        async move {
            if pva_pvs.read().await.contains_key(&name) {
                return true;
            }
            provider.channel_find(&name).await
        }
    }

    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<epics_pva_rs::pvdata::FieldDesc>> + Send {
        let name_owned = name.to_string();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            // Native-registered PVA PVs (NTNDArray etc.) live only in
            // pva_pvs — the BridgeProvider has no record for them, so
            // self.channel() would return None and the descriptor
            // would be lost. Probe the PVA registry first.
            if let Some(handle) = pva_pvs.read().await.get(&name_owned).cloned()
                && let Some(value) = handle.latest.lock().clone()
            {
                return Some(value.descriptor());
            }
            let channel = self.channel(&name_owned).await?;
            channel.get_field().await.ok()
        }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let name_owned = name.to_string();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            if let Some(handle) = pva_pvs.read().await.get(&name_owned).cloned()
                && let Some(value) = handle.latest.lock().clone()
            {
                return Some(value);
            }
            let channel = self.channel(&name_owned).await?;
            let empty_request = PvStructure::new("");
            match channel.get(&empty_request).await {
                Ok(pv) => Some(PvField::Structure(pv)),
                Err(e) => {
                    tracing::debug!("qsrv get_value({name_owned}) failed: {e}");
                    None
                }
            }
        }
    }

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let name_owned = name.to_string();
        async move {
            let channel = self
                .channel(&name_owned)
                .await
                .ok_or_else(|| format!("PV not found: {name_owned}"))?;
            let pv = match value {
                PvField::Structure(s) => s,
                other => return Err(format!("qsrv PUT expects a structure value, got {other}")),
            };
            channel.put(&pv).await.map_err(|e| e.to_string())
        }
    }

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        let name = name.to_string();
        async move {
            // F-G3: previously returned `true` for any existing PV via
            // channel_find, lying for read-only records (DISP=1) and
            // delaying the PUT refusal until the actual write attempt.
            // Now consult provider.is_writable (DISP-aware), and treat
            // PVA-plugin PVs (NTNDArray cache from NDPluginPva) as
            // read-only — they're produced server-side, not driven by
            // downstream PUTs.
            if pva_pvs.read().await.contains_key(&name) {
                return false;
            }
            provider.is_writable(&name).await
        }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let name_owned = name.to_string();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            // Native-registered PVA PVs publish into their own
            // subscriber list maintained by the registering plugin
            // (NDPluginPva); the QSRV side just appends a tx so the
            // plugin's `post()` fans out into the PVA server.
            // Reap any subscriber whose receiver was already dropped
            // before pushing — without this, an idle camera that
            // never calls `process_array` (the only existing reaper)
            // accumulates one closed Sender per subscribe-and-disconnect
            // cycle, growing the Vec without bound.
            if let Some(handle) = pva_pvs.read().await.get(&name_owned).cloned() {
                let (tx, rx) = mpsc::channel::<PvField>(64);
                {
                    let mut subs = handle.subscribers.lock();
                    subs.retain(|s| !s.is_closed());
                    subs.push(tx);
                }
                return Some(rx);
            }
            let channel = self.channel(&name_owned).await?;
            let mut monitor = channel.create_monitor().await.ok()?;
            monitor.start().await.ok()?;
            let (tx, rx) = mpsc::channel::<PvField>(64);
            tokio::spawn(async move {
                while let Some(snapshot) = monitor.poll().await {
                    if tx.send(PvField::Structure(snapshot)).await.is_err() {
                        break;
                    }
                }
                monitor.stop().await;
            });
            Some(rx)
        }
    }
}

// ---------------------------------------------------------------------------
// CA + PVA dual-protocol runner for IocApplication
// ---------------------------------------------------------------------------

/// Runs a combined CA + PVA IOC with QSRV bridge.
///
/// Designed as a protocol runner for [`IocApplication::run`]. Starts a CA
/// server in the background, creates a `QsrvPvStore` wrapping the database,
/// registers any native PVA PVs (NTNDArray from NDPluginPva), then runs the
/// PVA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// AdIoc::new()
///     .run_with_script_and_runner("st.cmd", run_ca_pva_qsrv_ioc)
///     .await
/// ```
pub async fn run_ca_pva_qsrv_ioc(
    config: epics_base_rs::server::ioc_app::IocRunConfig,
) -> epics_base_rs::error::CaResult<()> {
    use epics_base_rs::error::CaError;

    let db = config.db.clone();
    let ca_port = config.port;
    let pva_port: u16 = std::env::var("EPICS_PVA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5075);

    // ── QSRV bridge ──
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let store = Arc::new(QsrvPvStore::new(provider));

    // Register native PVA PVs (NTNDArray from NDPvaConfigure, etc.).
    // Handles were stored in the global registry during st.cmd execution.
    let pva_pvs = take_registered_pva_pvs();
    for (pv_name, handle) in pva_pvs {
        tracing::info!(pv = %pv_name, "registering native PVA PV");
        store
            .register_pva_pv(&pv_name, handle.latest, handle.subscribers)
            .await;
    }

    // ── CA server (background) ──
    let ca_server = epics_ca_rs::server::CaServer::from_parts(
        db.clone(),
        ca_port,
        config.acf.clone(),
        config.autosave_config.clone(),
        config.autosave_manager.clone(),
    );
    epics_base_rs::runtime::task::spawn(async move {
        if let Err(e) = ca_server.run().await {
            eprintln!("CA server error: {e}");
        }
    });

    // ── PVA server (foreground with iocsh) ──
    let pva_server = epics_pva_rs::server::PvaServer::from_parts(
        db,
        pva_port,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    );

    let shell_commands = config.shell_commands;
    pva_server
        .run_with_source_and_shell(store, move |shell| {
            for cmd in shell_commands {
                shell.register(cmd);
            }
        })
        .await
        .map_err(|e| CaError::InvalidValue(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn has_pv_falls_through_to_provider() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::server_native::ChannelSource;
        let db = Arc::new(PvDatabase::new());
        db.add_pv("TEST:X", epics_base_rs::types::EpicsValue::Double(1.0))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);
        assert!(store.has_pv("TEST:X").await);
        assert!(!store.has_pv("NOT:THERE").await);
    }
}
