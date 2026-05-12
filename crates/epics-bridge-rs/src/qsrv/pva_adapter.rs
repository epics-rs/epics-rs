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

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure};

use super::provider::{AnyChannel, BridgeProvider, Channel, ChannelProvider, PvaMonitor};

/// Handle for a native PVA PV: latest snapshot + subscriber list +
/// (optional) canonical introspection descriptor.
///
/// Registered via [`QsrvPvStore::register_pva_pv`] so the native PVA
/// server can serve structure-shaped values produced directly by IOC code
/// (for example NTNDArray or aggregate benchmark PVs). Snapshots and
/// notifications use native [`PvField`] values; no spvirit dependency.
///
/// `descriptor` lets the producer pass the authoritative wire shape,
/// bypassing the lossy [`PvField::descriptor`] recovery for types it
/// cannot reconstruct from the value alone (top-level `UnionArray`,
/// `Union` with sibling variants, empty `ScalarArray`/`StructureArray`).
/// When `None`, `get_introspection` falls back to value-derived recovery
/// — sufficient for structure-rooted normative types where every field
/// is exercised in the value.
#[derive(Clone)]
pub struct PvaPvHandle {
    pub latest: Arc<parking_lot::Mutex<Option<PvField>>>,
    pub subscribers: Arc<parking_lot::Mutex<Vec<mpsc::Sender<PvField>>>>,
    /// Canonical introspection descriptor supplied by the producer.
    /// Preferred over value-derived recovery when present.
    pub descriptor: Option<FieldDesc>,
}

// ---------------------------------------------------------------------------
// Global PVA PV registry — IOC code stores handles here during startup,
// the CA+PVA runner reads them at server startup.
// ---------------------------------------------------------------------------

static PVA_PV_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PvaPvHandle>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Register a native PVA PV before the CA+PVA runner starts.
///
/// Same root-kind invariant as [`QsrvPvStore::register_pva_pv`]: if the
/// handle carries both a value and a descriptor, their root kinds must
/// agree, otherwise this panics. Enforcing it here too (not just on the
/// later runner-side `register_pva_pv` call) means the failure surfaces
/// at the producer call site, where the wrong descriptor was assembled,
/// rather than deep inside the CA+PVA runner.
pub fn register_pva_pv_global(pv_name: &str, handle: PvaPvHandle) {
    assert_handle_root_kind(pv_name, &handle);
    PVA_PV_REGISTRY
        .lock()
        .unwrap()
        .insert(pv_name.to_string(), handle);
}

/// Invariant gate shared by both registration paths
/// (`register_pva_pv_global` and `QsrvPvStore::register_pva_pv`).
fn assert_handle_root_kind(pv_name: &str, handle: &PvaPvHandle) {
    if let Some(desc) = &handle.descriptor {
        let guard = handle.latest.lock();
        if let Some(value) = guard.as_ref() {
            assert!(
                root_kind_matches(value, desc),
                "PvaPvHandle for {pv_name:?}: supplied descriptor root kind \
                 does not match value root kind ({value_kind} vs {desc_kind}) — \
                 introspection would disagree with served values",
                value_kind = root_kind_name_value(value),
                desc_kind = root_kind_name_desc(desc),
            );
        }
    }
}

/// Take all registered native PVA PVs. Called by [`run_ca_pva_qsrv_ioc`]
/// to wire them into `QsrvPvStore`.
pub fn take_registered_pva_pvs() -> std::collections::HashMap<String, PvaPvHandle> {
    std::mem::take(&mut *PVA_PV_REGISTRY.lock().unwrap())
}

/// Root-level kind compatibility between a value and a descriptor.
///
/// This is the weakest invariant we can cheaply verify at registration
/// time: deeper structural agreement (field names, scalar types,
/// variants list) is the producer's responsibility. The root check
/// catches the obvious misconfiguration — wiring a `Structure` value
/// with a `UnionArray` descriptor — without forcing a full recursive
/// walk on a potentially large `latest` snapshot under the mutex.
fn root_kind_matches(value: &PvField, desc: &FieldDesc) -> bool {
    matches!(
        (value, desc),
        (PvField::Scalar(_), FieldDesc::Scalar(_))
            | (
                PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_),
                FieldDesc::ScalarArray(_)
            )
            | (PvField::Structure(_), FieldDesc::Structure { .. })
            | (PvField::StructureArray(_), FieldDesc::StructureArray { .. })
            | (PvField::Union { .. }, FieldDesc::Union { .. })
            | (PvField::UnionArray(_), FieldDesc::UnionArray { .. })
            | (PvField::Variant(_) | PvField::Null, FieldDesc::Variant)
            | (PvField::VariantArray(_), FieldDesc::VariantArray)
    )
}

fn root_kind_name_value(v: &PvField) -> &'static str {
    match v {
        PvField::Scalar(_) => "Scalar",
        PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => "ScalarArray",
        PvField::Structure(_) => "Structure",
        PvField::StructureArray(_) => "StructureArray",
        PvField::Union { .. } => "Union",
        PvField::UnionArray(_) => "UnionArray",
        PvField::Variant(_) => "Variant",
        PvField::VariantArray(_) => "VariantArray",
        PvField::Null => "Null",
    }
}

fn root_kind_name_desc(d: &FieldDesc) -> &'static str {
    match d {
        FieldDesc::Scalar(_) => "Scalar",
        FieldDesc::ScalarArray(_) => "ScalarArray",
        FieldDesc::Structure { .. } => "Structure",
        FieldDesc::StructureArray { .. } => "StructureArray",
        FieldDesc::Union { .. } => "Union",
        FieldDesc::UnionArray { .. } => "UnionArray",
        FieldDesc::Variant => "Variant",
        FieldDesc::VariantArray => "VariantArray",
        FieldDesc::BoundedString(_) => "BoundedString",
    }
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
    /// via `get_value`, and subscribable via `subscribe`.
    ///
    /// Pass `descriptor = Some(...)` when the producer knows the
    /// canonical wire shape (e.g. `nt_nd_array_desc()` for NTNDArray, or
    /// a custom top-level `UnionArray`); pass `None` to use value-derived
    /// recovery on each introspection query (lossy for some types — see
    /// [`PvField::descriptor`]).
    ///
    /// **Panics** if `descriptor` is supplied and a value is already
    /// present in `latest` whose root kind disagrees (e.g. supplying a
    /// `FieldDesc::UnionArray` alongside a `PvField::Structure` value).
    /// This catches misconfigured producers at startup rather than
    /// shipping clients an introspection that disagrees with the values
    /// they receive. Producers whose first value arrives after
    /// registration (NDPluginPva: first NDArray hasn't been processed
    /// yet) trivially pass this check.
    pub async fn register_pva_pv(
        &self,
        pv_name: &str,
        latest: Arc<parking_lot::Mutex<Option<PvField>>>,
        subscribers: Arc<parking_lot::Mutex<Vec<mpsc::Sender<PvField>>>>,
        descriptor: Option<FieldDesc>,
    ) {
        let handle = PvaPvHandle {
            latest,
            subscribers,
            descriptor,
        };
        assert_handle_root_kind(pv_name, &handle);
        self.pva_pvs
            .write()
            .await
            .insert(pv_name.to_string(), handle);
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
            if let Some(handle) = pva_pvs.read().await.get(&name_owned).cloned() {
                // Prefer the canonical descriptor supplied at registration
                // (wire-faithful for `UnionArray` and other types that
                // `PvField::descriptor` cannot losslessly reconstruct).
                if let Some(desc) = handle.descriptor.clone() {
                    return Some(desc);
                }
                if let Some(value) = handle.latest.lock().clone() {
                    return Some(value.descriptor());
                }
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
            .register_pva_pv(
                &pv_name,
                handle.latest,
                handle.subscribers,
                handle.descriptor,
            )
            .await;
    }

    // ── CA server (background) ──
    let ca_server = epics_ca_rs::server::CaServer::from_parts(
        db.clone(),
        ca_port,
        None,
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

    /// Top-level `UnionArray` PV: when the producer hands the canonical
    /// descriptor through `register_pva_pv`, introspection returns the
    /// full variants list — not the empty-variants degradation that
    /// [`PvField::descriptor`] would produce on its own. Regression for
    /// the lossy `UnionArray` recovery path documented on
    /// `PvField::descriptor`.
    #[tokio::test]
    async fn get_introspection_uses_supplied_descriptor_for_union_array() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue, UnionItem};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        // Top-level UnionArray with two named variants. Only the first is
        // exercised in the value below, so value-derived recovery would
        // lose the `as_double` variant entirely.
        let canonical = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![
                ("as_int".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("as_double".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let value = PvField::UnionArray(vec![UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(ScalarValue::Int(7)),
        }]);

        let latest = Arc::new(parking_lot::Mutex::new(Some(value)));
        let subscribers = Arc::new(parking_lot::Mutex::new(Vec::new()));
        store
            .register_pva_pv("TEST:UARR", latest, subscribers, Some(canonical.clone()))
            .await;

        let got = store.get_introspection("TEST:UARR").await.unwrap();
        assert_eq!(got, canonical, "supplied descriptor must round-trip");
    }

    /// When the producer omits the canonical descriptor, introspection
    /// falls back to value-derived recovery — locking in the documented
    /// lossy behavior so future refactors can't silently invert it.
    #[tokio::test]
    async fn get_introspection_falls_back_to_value_descriptor_when_unset() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, UnionItem};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        let value = PvField::UnionArray(vec![UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(7)),
        }]);
        let latest = Arc::new(parking_lot::Mutex::new(Some(value)));
        let subscribers = Arc::new(parking_lot::Mutex::new(Vec::new()));
        store
            .register_pva_pv("TEST:UARR_LOSSY", latest, subscribers, None)
            .await;

        let got = store.get_introspection("TEST:UARR_LOSSY").await.unwrap();
        assert_eq!(
            got,
            FieldDesc::UnionArray {
                struct_id: String::new(),
                variants: Vec::new(),
            },
            "documented lossy recovery: variants list must be empty"
        );
    }

    /// Symmetric invariant guard at the global-registry entry point.
    /// `register_pva_pv_global` is the producer-side path (called from
    /// IOC startup before the runner exists); it must enforce the same
    /// root-kind invariant as the runner-side `register_pva_pv`,
    /// otherwise misconfigured handles silently sit in the global map
    /// and surface only when the runner drains it later.
    #[test]
    #[should_panic(expected = "root kind")]
    fn register_pva_pv_global_panics_on_root_mismatch() {
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType};

        let value = PvField::Structure(PvStructure::new("x"));
        let bogus_desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![("as_int".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        register_pva_pv_global(
            "TEST:BOGUS_GLOBAL",
            PvaPvHandle {
                latest: Arc::new(parking_lot::Mutex::new(Some(value))),
                subscribers: Arc::new(parking_lot::Mutex::new(Vec::new())),
                descriptor: Some(bogus_desc),
            },
        );
    }

    /// Invariant guard: registering a descriptor whose root kind
    /// disagrees with the current value panics. Mirrors the
    /// `PvaPvHandle` contract — introspection must not diverge from
    /// the values served.
    #[tokio::test]
    #[should_panic(expected = "root kind")]
    async fn register_pva_pv_panics_on_descriptor_value_root_mismatch() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType};

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        // Value is a Structure, descriptor is a UnionArray — root mismatch.
        let value = PvField::Structure(PvStructure::new("x"));
        let bogus_desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![("as_int".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        let latest = Arc::new(parking_lot::Mutex::new(Some(value)));
        let subscribers = Arc::new(parking_lot::Mutex::new(Vec::new()));

        store
            .register_pva_pv("TEST:BOGUS", latest, subscribers, Some(bogus_desc))
            .await;
    }

    /// End-to-end wire test: a top-level `UnionArray` PV registered
    /// with a canonical descriptor is served over real PVA, and the
    /// client recovers the full variants list via `GET_FIELD`. Closes
    /// the loop on the doc claim that wire-faithful round-tripping
    /// now works — the previous unit tests only validated the
    /// `ChannelSource` contract.
    #[tokio::test]
    async fn pva_server_serves_canonical_union_array_descriptor_over_wire() {
        use std::time::Duration;

        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue, UnionItem};
        use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = Arc::new(QsrvPvStore::new(provider));

        let canonical = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![
                ("as_int".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("as_double".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let value = PvField::UnionArray(vec![UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(ScalarValue::Int(7)),
        }]);
        store
            .register_pva_pv(
                "TEST:WIRE:UARR",
                Arc::new(parking_lot::Mutex::new(Some(value))),
                Arc::new(parking_lot::Mutex::new(Vec::new())),
                Some(canonical.clone()),
            )
            .await;

        let server = PvaServer::start(store, PvaServerConfig::isolated());
        let client = server.client_config();

        let got = tokio::time::timeout(Duration::from_secs(5), client.pvinfo("TEST:WIRE:UARR"))
            .await
            .expect("pvinfo timeout")
            .expect("pvinfo failed");

        assert_eq!(
            got, canonical,
            "client-side introspection must recover the producer's UnionArray variants over the wire"
        );
    }
}
