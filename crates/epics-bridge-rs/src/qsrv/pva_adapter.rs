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
use epics_pva_rs::server_native::source::OpError;

use super::provider::{
    AnyChannel, BridgeProvider, Channel, ChannelProvider, ClientCreds, PvaMonitor,
};

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

/// Convert a native-PVA [`ChannelContext`] to a [`ClientCreds`] for
/// `BridgeProvider::create_channel_with_creds`.
///
/// Carries method/authority/roles through so `AcfAccessControl` can
/// evaluate METHOD/AUTHORITY rules and role-based UAG entries — fixing the
/// defect where these were hardcoded to `"anonymous"` / `""`.
fn ctx_to_creds(ctx: &epics_pva_rs::server_native::source::ChannelContext) -> ClientCreds {
    ClientCreds {
        user: ctx.account.clone(),
        host: ctx.host.clone(),
        method: ctx.method.clone(),
        authority: ctx.authority.clone(),
        // forward the native PVA peer's parsed role/group
        // claims so `AcfAccessControl` can evaluate role-based UAG
        // entries (`R member group:ops`). Previously hardcoded empty,
        // so role-scoped ACF rules denied real over-the-wire clients.
        roles: ctx.roles.clone(),
    }
}

/// PvStore implementation backed by a qsrv [`BridgeProvider`].
///
/// Handles single-record PVs, group composite PVs, and native PVA PVs
/// (NTNDArray from areaDetector). Group PVs ride on the
/// `NtPayload::Generic` variant with a recursive `PvValue` tree.
pub struct QsrvPvStore {
    provider: Arc<BridgeProvider>,
    /// Native PVA PVs (e.g., NTNDArray from NDPluginPva).
    pva_pvs: Arc<RwLock<HashMap<String, PvaPvHandle>>>,
}

impl QsrvPvStore {
    pub fn new(provider: Arc<BridgeProvider>) -> Self {
        Self {
            provider,
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

    /// Legacy ctx-less channel path — used only by the default
    /// trait methods (`get_value` / `put_value` / `is_writable` /
    /// `subscribe`) that lack a `ChannelContext`. Anonymous
    /// identity, no per-op caching. The `_checked` overrides
    /// below carry the real `user/host` through to ACF.
    async fn channel(&self, name: &str) -> Option<AnyChannel> {
        self.channel_for(name, "", "").await
    }

    /// create a channel for the supplied identity. No
    /// `channels` cache — caching an `AnyChannel` keyed by PV
    /// name only reused one peer's `AccessContext` for every
    /// subsequent peer and silently bypassed any ACF policy the
    /// IOC runner installed. Metadata is still cached inside the
    /// `BridgeProvider` (record_cache) so the per-call cost
    /// stays low.
    async fn channel_for(&self, name: &str, user: &str, host: &str) -> Option<AnyChannel> {
        self.provider
            .create_channel_for(name, user, host)
            .await
            .ok()
    }
}

/// A read-authorized monitor resolved by [`open_monitor`] — either a
/// native-PVA fan-in receiver (plain values, no `+trigger` graph) or a
/// started DB/group monitor whose `poll()` carries the per-event marked
/// set. Shared by the cooked `subscribe_checked` and the marked-aware
/// `subscribe_checked_opts_marked` so the channel-creation logic lives once.
enum OpenedMonitor {
    /// Native PVA PV (NDPluginPva etc.): a `tx` was appended to the
    /// handle's subscriber list; values fan out as plain `PvField`s.
    Native(mpsc::Receiver<PvField>),
    /// A started single-record / group monitor.
    Db(super::group::AnyMonitor),
}

/// resolve a started monitor for a read-authorized PV.
/// Factored out of `subscribe_checked` so `subscribe_checked_opts_marked`
/// (which carries the `+trigger` marked set to the wire) reuses the
/// exact same native-PVA / channel / DBE / queue-depth resolution.
async fn open_monitor(
    provider: Arc<BridgeProvider>,
    pva_pvs: Arc<RwLock<HashMap<String, PvaPvHandle>>>,
    checked: epics_pva_rs::server_native::source::AccessChecked,
    ctx: epics_pva_rs::server_native::source::ChannelContext,
) -> Option<OpenedMonitor> {
    if !checked.allows_read() {
        return None;
    }
    let name = checked.pv_name().to_string();
    if let Some(handle) = pva_pvs.read().await.get(&name).cloned() {
        let (tx, rx) = mpsc::channel::<PvField>(64);
        {
            let mut subs = handle.subscribers.lock();
            subs.retain(|s| !s.is_closed());
            subs.push(tx);
        }
        return Some(OpenedMonitor::Native(rx));
    }
    let channel = provider
        .create_channel_with_creds(&name, ctx_to_creds(&ctx))
        .await
        .ok()?;
    // honor `record._options.DBE` from the MONITOR INIT
    // pvRequest. Single-record channels route through
    // `BridgeChannel::create_monitor_with_value_mask`; group
    // and pva_pv-registered channels fall through to the
    // default mask (their DBE selection is not yet wired).
    let dbe_mask = match ctx.pv_request {
        Some(PvField::Structure(ref req)) => crate::qsrv::channel::dbe_mask_from_pv_request(req),
        _ => None,
    };
    // resolve the per-operation negotiated monitor
    // queue depth from the MONITOR INIT pvRequest's
    // `record._options.queueSize` — pvxs `servermon.cpp:533` then
    // `groupsource.cpp:359` stamps `stats.limitQueue`.
    let queue_size = match ctx.pv_request {
        Some(PvField::Structure(ref req)) => crate::qsrv::group::negotiated_queue_size(req),
        _ => crate::qsrv::group::GROUP_DEFAULT_QUEUE_SIZE,
    };
    let mut monitor = match channel {
        crate::qsrv::AnyChannel::Single(single) => {
            single.create_monitor_with_value_mask(dbe_mask).await.ok()?
        }
        other => other
            .create_monitor()
            .await
            .ok()?
            .with_queue_size(queue_size),
    };
    monitor.start().await.ok()?;
    Some(OpenedMonitor::Db(monitor))
}

// ── ChannelSource impl (native PvAccess server) ──────────────────────────
//
// In addition to the legacy spvirit `PvStore` impl above, expose the same
// data via the native [`epics_pva_rs::server_native::ChannelSource`] trait.
// This is the path used by `epics_pva_rs::server::PvaServer::run_with_source`
// (no spvirit_server runtime involvement).

impl epics_pva_rs::server_native::ChannelSource for QsrvPvStore {
    // thread identity through the type-state
    // `_checked` overrides so every wire op runs against the
    // ACF policy with the correct user/host.

    fn get_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let pva_pvs = self.pva_pvs.clone();
        let provider = self.provider.clone();
        async move {
            if !checked.allows_read() {
                return None;
            }
            let name = checked.pv_name().to_string();
            // Native PVA-pushed PVs (NDPluginPva) carry no
            // per-record ACF — they are always readable.
            if let Some(handle) = pva_pvs.read().await.get(&name).cloned()
                && let Some(value) = handle.latest.lock().clone()
            {
                return Some(value);
            }
            let channel = provider
                .create_channel_with_creds(&name, ctx_to_creds(&ctx))
                .await
                .ok()?;
            // forward the decoded INIT pvRequest so QSRV group
            // GET honors `record._options` (e.g. `atomic`). The native
            // wire layer now threads `init_pv_request` into the GET /
            // PUT-readback `ChannelContext`. Fall back to an empty
            // request only when no pvRequest structure was captured.
            let empty_request = PvStructure::new("");
            let request = match &ctx.pv_request {
                Some(PvField::Structure(s)) => s,
                _ => &empty_request,
            };
            match channel.get(request).await {
                Ok(pv) => Some(PvField::Structure(pv)),
                Err(e) => {
                    tracing::debug!(
                        "qsrv get_value_checked({name}) {} from {}@{}: {e}",
                        ctx.account,
                        ctx.method,
                        ctx.host
                    );
                    None
                }
            }
        }
    }

    fn put_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        value: PvField,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let provider = self.provider.clone();
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "PUT denied by access security: '{}' from {}@{}",
                    checked.pv_name(),
                    ctx.account,
                    ctx.host
                )));
            }
            let name = checked.pv_name().to_string();
            let pv = match value {
                PvField::Structure(s) => s,
                other => {
                    return Err(OpError::failed(format!(
                        "qsrv PUT expects a structure value, got {other}"
                    )));
                }
            };
            // prefer the INIT pvRequest for `record._options`,
            // matching pvxs (`iocsource.cpp:429`). The data-phase value
            // is just the delta; per-operation options live in the
            // INIT pvRequest and reach us via `ChannelContext`. Fall
            // back to value-embedded options for callers that did not
            // come through the wire (in-process tests, gateway).
            //
            // the group arm previously called `other.put(&pv)`,
            // which re-parses options from the data-phase value — so a
            // native PVA group PUT/PUT_GET whose `record._options.atomic`
            // lives only in the INIT pvRequest was silently ignored.
            // Route the group through `put_with_options` with the same
            // INIT-pvRequest-derived options, matching pvxs
            // `groupsource.cpp:540`.
            let init_req = match ctx.pv_request {
                Some(PvField::Structure(ref req)) => Some(req),
                _ => None,
            };
            let opts = match init_req {
                Some(req) => crate::qsrv::channel::PutOptions::from_pv_request(req),
                None => crate::qsrv::channel::PutOptions::from_pv_request(&pv),
            };
            let channel = provider
                .create_channel_with_creds(&name, ctx_to_creds(&ctx))
                .await
                .map_err(|e| OpError::failed(e.to_string()))?;
            match channel {
                crate::qsrv::AnyChannel::Single(single) => single
                    .put_with_options(&pv, opts)
                    .await
                    .map_err(|e| OpError::failed(e.to_string())),
                crate::qsrv::AnyChannel::Group(group) => {
                    // `atomic` lives in the INIT pvRequest on the wire
                    // path; fall back to the value for in-process
                    // callers, then to the group default inside
                    // `put_with_options` when neither set it.
                    let atomic_override = match init_req {
                        Some(req) => crate::qsrv::channel::atomic_from_pv_request(req),
                        None => crate::qsrv::channel::atomic_from_pv_request(&pv),
                    };
                    group
                        .put_with_options(&pv, opts, atomic_override)
                        .await
                        .map_err(|e| OpError::failed(e.to_string()))
                }
            }
        }
    }

    /// PVA `PROCESS` against a QSRV record runs the record's
    /// `dbProcess`-equivalent path. The default `ChannelSource::process`
    /// returns `Ok(())` unconditionally — a client calling
    /// `pvput -P` (or any wire-level PROCESS) would observe a false
    /// success even though no processing fired. Route through the
    /// provider's resolved record so the operation has the same
    /// observable effect as PUT with `record._options.process=true`:
    /// the record is processed via `PvDatabase::process_record`,
    /// alarms / FLNK / output links all run. Single-record-only —
    /// group / native PVA PVs have no processing semantics and are
    /// returned an unsupported-op error so callers don't silently
    /// trust a no-op.
    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        let name = name.to_string();
        async move {
            if pva_pvs.read().await.contains_key(&name) {
                return Err(OpError::failed(format!(
                    "PROCESS not supported for native PVA PV '{name}' (no processing chain)"
                )));
            }
            if provider.groups().contains_key(&name) {
                return Err(OpError::failed(format!(
                    "PROCESS not supported for group PV '{name}' (no record-level chain)"
                )));
            }
            let (record_name, _field) = epics_base_rs::server::database::parse_pv_name(&name);
            provider
                .database()
                .process_record(record_name)
                .await
                .map_err(|e| OpError::failed(format!("PROCESS on '{name}': {e}")))
        }
    }

    /// RPC-with-query is a record WRITE in QSRV's
    /// "set fields, read back" idiom. The default trait
    /// `rpc_checked` gates only on READ access, so a READ-only
    /// client could mutate records through `pvcall PV
    /// field=value`. Override to require WRITE access whenever
    /// the request carries any query fields (NTURI.query or a
    /// bare structure with members).
    #[allow(clippy::manual_async_fn)]
    fn rpc_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), OpError>> + Send {
        async move {
            if !checked.allows_read() {
                return Err(OpError::denied(format!(
                    "RPC denied by access security: '{}' from {}@{}",
                    checked.pv_name(),
                    ctx.account,
                    ctx.host,
                )));
            }
            // Inspect the request to determine whether it carries
            // query arguments that would land as a PUT inside
            // `rpc()`. NTURI shape: `query` substructure with one
            // or more fields. Bare-structure shape: any non-empty
            // top-level fields list.
            let has_writes = match &request_value {
                PvField::Structure(root) => {
                    if let Some((_, PvField::Structure(q))) =
                        root.fields.iter().find(|(n, _)| n == "query")
                    {
                        !q.fields.is_empty()
                    } else if root.struct_id == "epics:nt/NTURI:1.0" {
                        false
                    } else {
                        root.fields
                            .iter()
                            .any(|(n, _)| !n.starts_with("scheme") && !n.starts_with("path"))
                    }
                }
                _ => false,
            };
            if has_writes && !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "RPC write denied by access security: '{}' from {}@{} \
                     (RPC query arguments require WRITE access in QSRV)",
                    checked.pv_name(),
                    ctx.account,
                    ctx.host,
                )));
            }
            self.rpc(checked.pv_name(), request_desc, request_value)
                .await
        }
    }

    fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            // Legacy cooked path: plain `PvField`s, no marked set. The
            // PVA layer's `subscribe_checked_opts_marked` (below) is what
            // carries the `+trigger` marked set to the wire.
            match open_monitor(provider, pva_pvs, checked, ctx).await? {
                OpenedMonitor::Native(rx) => Some(rx),
                OpenedMonitor::Db(mut monitor) => {
                    let (tx, rx) = mpsc::channel::<PvField>(64);
                    tokio::spawn(async move {
                        while let Some(poll) = monitor.poll().await {
                            if tx.send(PvField::Structure(poll.value)).await.is_err() {
                                break;
                            }
                        }
                        monitor.stop().await;
                    });
                    Some(rx)
                }
            }
        }
    }

    /// the cooked MONITOR entry the native PVA server
    /// actually uses. QSRV overrides it to carry each event's resolved
    /// `+trigger` target set (`MonitorPoll::marked`) into the
    /// `MonitorUpdate` stream, so the server emits a wire changed-bitset
    /// matching the configured trigger graph instead of re-deriving a
    /// full mask. A native-PVA PV has no trigger graph, so it stays on
    /// the plain `marked: None` path.
    ///
    /// `opts` (pipeline / queueSize / server-filter) is applied by the
    /// PVA server layer on this same stream — QSRV owns the records
    /// directly — so there is nothing to reject here.
    fn subscribe_checked_opts_marked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
        _opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<mpsc::Receiver<epics_pva_rs::server_native::MonitorUpdate>>,
    > + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            match open_monitor(provider, pva_pvs, checked, ctx).await? {
                OpenedMonitor::Native(rx) => {
                    Some(epics_pva_rs::server_native::plain_monitor_updates(rx))
                }
                OpenedMonitor::Db(mut monitor) => {
                    let (tx, rx) = mpsc::channel::<epics_pva_rs::server_native::MonitorUpdate>(64);
                    tokio::spawn(async move {
                        while let Some(poll) = monitor.poll().await {
                            let update = epics_pva_rs::server_native::MonitorUpdate {
                                value: PvField::Structure(poll.value),
                                marked: poll.marked,
                            };
                            if tx.send(update).await.is_err() {
                                break;
                            }
                        }
                        monitor.stop().await;
                    });
                    Some(rx)
                }
            }
        }
    }

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
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name_owned = name.to_string();
        async move {
            let channel = self
                .channel(&name_owned)
                .await
                .ok_or_else(|| format!("PV not found: {name_owned}"))?;
            let pv = match value {
                PvField::Structure(s) => s,
                other => {
                    return Err(OpError::failed(format!(
                        "qsrv PUT expects a structure value, got {other}"
                    )));
                }
            };
            channel
                .put(&pv)
                .await
                .map_err(|e| OpError::failed(e.to_string()))
        }
    }

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        let name = name.to_string();
        async move {
            // previously returned `true` for any existing PV via
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

    /// PVA RPC against a QSRV-served record or group PV.
    ///
    /// pvxs QSRV2 supports RPC on records and group PVs; the prior
    /// `QsrvPvStore` left `ChannelSource::rpc` at its default
    /// ("RPC not supported"). This override gives QSRV records and
    /// group PVs the standard NTURI-shaped RPC contract:
    ///
    /// * The request is parsed as `epics:nt/NTURI:1.0` — RPC arguments
    ///   live in the `query` substructure (`pvcall PV field=value`).
    ///   A non-NTURI top-level structure is also accepted: its own
    ///   fields are treated as the query set.
    /// * Every query field is PUT into the target PV before the
    ///   response is assembled. A query field named `value` (the
    ///   common `pvcall PV value=...` case) maps onto the record's
    ///   canonical `value` subfield; a field whose name is a known
    ///   group member is routed to that member. This makes RPC a
    ///   process-and-read primitive — pvxs `pvxcall`-style.
    /// * The response is the PV's current value after the writes
    ///   (`(FieldDesc, PvField::Structure)`), so a parameterless RPC
    ///   degenerates to a plain GET. Native PVA plugin PVs
    ///   (NTNDArray etc.) are read-only: their RPC is GET-only and a
    ///   non-empty query is rejected.
    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), OpError>> + Send {
        let name_owned = name.to_string();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            let _ = request_desc;
            // Pull RPC arguments: NTURI puts them under `query`,
            // a bare structure exposes them at top level.
            let query: Vec<(String, PvField)> = match &request_value {
                PvField::Structure(s) => match s.get_field("query") {
                    Some(PvField::Structure(q)) => q.fields.clone(),
                    _ => s.fields.clone(),
                },
                PvField::Null => Vec::new(),
                other => {
                    return Err(OpError::failed(format!(
                        "qsrv RPC on {name_owned:?}: expected an NTURI or structure \
                         request, got {other}"
                    )));
                }
            };

            // Native PVA plugin PVs (NTNDArray from NDPluginPva) are
            // produced server-side and have no record to write into.
            if let Some(handle) = pva_pvs.read().await.get(&name_owned).cloned() {
                if !query.is_empty() {
                    return Err(OpError::failed(format!(
                        "qsrv RPC on {name_owned:?}: native PVA PV is read-only, \
                         RPC query arguments are not accepted"
                    )));
                }
                let value = handle.latest.lock().clone().ok_or_else(|| {
                    format!("qsrv RPC on {name_owned:?}: native PVA PV has no value yet")
                })?;
                let desc = handle
                    .descriptor
                    .clone()
                    .unwrap_or_else(|| value.descriptor());
                return Ok((desc, value));
            }

            let channel = self
                .channel(&name_owned)
                .await
                .ok_or_else(|| format!("qsrv RPC: PV not found: {name_owned}"))?;

            // Apply the query as a single PUT before the read-back.
            // Each query field is placed at its own path in the put
            // structure: for a single-record channel the only writable
            // path is `value` (`pvcall PV value=...`); for a group PV
            // every query field name is a member field path, resolved
            // the same way `GroupChannel::put` reads members via
            // `get_nested_field`. One `put` call so atomic groups stay
            // atomic.
            //
            // B1: a single-record channel's put extracts only the
            // `value` field (`pv_structure_to_epics`). A query field
            // named anything else would be placed at a dead path and
            // silently dropped — reject it with a clear error so the
            // caller learns the contract instead of seeing a no-op.
            // Group channels accept arbitrary member paths, so the
            // check is scoped to `AnyChannel::Single`.
            if !query.is_empty() {
                if let AnyChannel::Single(_) = &channel {
                    for (field, _) in &query {
                        let top = field.split('.').next().unwrap_or(field);
                        if top != "value" {
                            return Err(OpError::failed(format!(
                                "qsrv RPC on {name_owned:?}: single-record channel \
                                 accepts only the `value` query field, got {field:?}"
                            )));
                        }
                    }
                }
                let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
                for (field, field_value) in &query {
                    super::group::set_nested_field(&mut put, field, field_value.clone());
                }
                channel
                    .put(&put)
                    .await
                    .map_err(|e| format!("qsrv RPC put on {name_owned} failed: {e}"))?;
            }

            // Read back the post-write value as the RPC response.
            let empty_request = PvStructure::new("");
            let value = channel
                .get(&empty_request)
                .await
                .map_err(|e| format!("qsrv RPC get-back on {name_owned} failed: {e}"))?;
            let desc = channel
                .get_field()
                .await
                .map_err(|e| format!("qsrv RPC introspection on {name_owned} failed: {e}"))?;
            Ok((desc, PvField::Structure(value)))
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
                // Legacy ctx-less path: plain values, marked set dropped
                // (the marked-aware cooked entry is `subscribe_checked_opts_marked`).
                while let Some(poll) = monitor.poll().await {
                    if tx.send(PvField::Structure(poll.value)).await.is_err() {
                        break;
                    }
                }
                monitor.stop().await;
            });
            Some(rx)
        }
    }

    /// a QSRV *pure self-trigger* group monitor emits partial
    /// updates — each event re-reads only the member whose record
    /// processed, so the PVA server narrows the wire changed-bitset
    /// by diffing consecutive snapshots. Returns `true` only for
    /// groups whose every member uses the default `+trigger`
    /// (self-trigger); single-record / native-PVA PVs and groups with
    /// explicit `+trigger` members keep the full request mask.
    fn monitor_emits_partial(&self, name: &str) -> bool {
        self.provider.group_is_pure_self_trigger(name)
    }
}

// ---------------------------------------------------------------------------
// CA + PVA dual-protocol runner for IocApplication
// ---------------------------------------------------------------------------

/// The pvxs `enable2()` decision: whether QSRV2 serving is enabled, plus
/// the startup diagnostics pvxs prints synchronously (iochooks.cpp:401-448).
struct Qsrv2Decision {
    enabled: bool,
    /// Invalid `PVXS_QSRV_ENABLE` value — pvxs prints this to stderr
    /// immediately (iochooks.cpp:421-426).
    error: Option<String>,
    /// The `INFO: PVXS QSRV2 ...` status line pvxs prints to stdout
    /// (iochooks.cpp:441-443). `None` when the operator explicitly
    /// disabled QSRV2 (`quiet` — the "shut up, I know what I'm doing" path).
    info: Option<String>,
}

/// Pure core of pvxs `enable2()` (iochooks.cpp:401-448), parameterized on
/// the two environment strings so it is testable without mutating the
/// process environment.
///
/// pvxs gates on `permit && request`. `permit` is false only when QSRV1
/// (`qsrv.dbd`) is co-linked; the Rust IOC has no QSRV1 coexistence, so
/// `permit` is always true and only `request` varies. `EPICS_IOC_IGNORE_
/// SERVERS=qsrv2` and `PVXS_QSRV_ENABLE=NO` disable quietly; `=YES`
/// enables; any other value keeps the default (enabled) and emits the
/// error diagnostic. The ignore-servers check takes precedence over
/// `PVXS_QSRV_ENABLE` (pvxs evaluates it first).
fn resolve_qsrv2_enable(ignore_servers: Option<&str>, qsrv_enable: Option<&str>) -> Qsrv2Decision {
    let mut request = true;
    let mut quiet = false;
    let mut error = None;

    if ignore_servers.is_some_and(|s| s.contains("qsrv2")) {
        request = false;
        quiet = true;
    } else if qsrv_enable.is_some_and(|s| s.eq_ignore_ascii_case("YES")) {
        request = true;
    } else if qsrv_enable.is_some_and(|s| s.eq_ignore_ascii_case("NO")) {
        request = false;
        quiet = true;
    } else if let Some(v) = qsrv_enable {
        // Unknown value: keep the default (`request` unchanged) and report.
        error = Some(format!(
            "ERROR: PVXS_QSRV_ENABLE={v} not YES/NO.  Defaulting to {}.",
            if request { "YES" } else { "NO" }
        ));
    }

    // permit == true, so enable == request.
    let enabled = request;
    let info = if quiet {
        None
    } else {
        Some(format!(
            "INFO: PVXS QSRV2 is loaded, permitted, and {}.",
            if enabled { "ENABLED" } else { "disabled" }
        ))
    };
    Qsrv2Decision {
        enabled,
        error,
        info,
    }
}

/// Read `EPICS_IOC_IGNORE_SERVERS` / `PVXS_QSRV_ENABLE`, apply the pvxs
/// `enable2()` rule, print the same synchronous startup diagnostics, and
/// return whether QSRV2 database/group serving is enabled.
fn qsrv2_enabled() -> bool {
    let ignore = std::env::var("EPICS_IOC_IGNORE_SERVERS").ok();
    let enable = std::env::var("PVXS_QSRV_ENABLE").ok();
    let decision = resolve_qsrv2_enable(ignore.as_deref(), enable.as_deref());
    if let Some(e) = &decision.error {
        eprintln!("{e}");
    }
    if let Some(i) = &decision.info {
        println!("{i}");
    }
    decision.enabled
}

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

    // ── QSRV2 enable gate (pvxs enable2(), iochooks.cpp:401-496) ──
    // Honor PVXS_QSRV_ENABLE / EPICS_IOC_IGNORE_SERVERS=qsrv2 before
    // standing up the database/group sources and pvalink. When disabled,
    // the PVA server still runs and serves native PVA PVs (NDPluginPva),
    // but the BridgeProvider answers "absent" for every DB/group channel
    // — matching a pvxs IOC where enable2() returned false.
    let qsrv2_on = qsrv2_enabled();

    // ── QSRV bridge ──
    let provider = Arc::new(BridgeProvider::new_with_serving(db.clone(), qsrv2_on));

    // install the IOC-wide ACF on the QSRV bridge so PVA
    // single-record / group operations enforce the same policy as
    // the CA server (which gets `config.acf` via
    // `CaServer::from_parts` below). Without this, an IOC launched
    // with `config.acf = Some(...)` would protect CA but leave the
    // PVA QSRV path on `AllowAllAccess` — the documented
    // configuration trap.
    if let Some(acf_cfg) = config.acf.clone() {
        let acf = Arc::new(super::provider::AcfAccessControl::new(db.clone(), acf_cfg));
        provider.set_access_control(acf);
        tracing::info!("qsrv: ACF installed on BridgeProvider");
    }

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

    // ── External links (`calink` / `pvalink` features) ──
    // Install the `"ca"` and `"pva"` link sets so a loaded database
    // with `INP="somepv CA"` / `ca://somepv` or `pva://somepv` links
    // resolves through the monitor-backed caches, and register the
    // matching `caxr` / `dbcaxr` and `pvxr` / `dbpvxr` /
    // `pvalink_enable` / `pvalink_disable` iocsh commands.
    #[cfg(any(feature = "calink", feature = "pvalink"))]
    let mut shell_commands = config.shell_commands;
    #[cfg(not(any(feature = "calink", feature = "pvalink")))]
    let shell_commands = config.shell_commands;
    #[cfg(feature = "calink")]
    {
        match crate::calink::install_calink_resolver(&db, tokio::runtime::Handle::current()).await {
            Ok(resolver) => {
                shell_commands.extend(crate::calink::register_calink_commands(resolver));
                tracing::info!("calink: `ca` link set installed");
            }
            Err(e) => {
                // A CA-client init failure must not abort the IOC: a
                // database with no CA links is still fully serviceable.
                tracing::warn!("calink: CA link set NOT installed: {e}");
            }
        }
    }
    #[cfg(feature = "pvalink")]
    if qsrv2_on {
        // pvxs calls `pvalink_enable()` only when `enable2()` is true
        // (iochooks.cpp:461-496); a QSRV2-disabled IOC leaves pvalink
        // inactive and registers no pvalink iocsh commands.
        //
        // `install_pvalink_resolver` is infallible — the PVA client is
        // created lazily per link — so there is no init failure to
        // abort the IOC on. A database with no PVA links is still
        // fully serviceable. This is the PVA-side counterpart of the
        // `"ca"` link set installed above.
        let resolver =
            crate::pvalink::install_pvalink_resolver(&db, tokio::runtime::Handle::current()).await;
        shell_commands.extend(crate::pvalink::register_pvalink_commands(resolver));
        tracing::info!("pvalink: `pva` link set installed");
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

    // ── pvxs enable2() decision (resolve_qsrv2_enable) ──

    #[test]
    fn qsrv2_enable_default_is_on() {
        let d = resolve_qsrv2_enable(None, None);
        assert!(d.enabled, "QSRV2 defaults to enabled when no env set");
        assert!(d.error.is_none());
        assert!(d.info.as_deref().unwrap().contains("ENABLED"));
    }

    #[test]
    fn qsrv2_enable_yes_case_insensitive() {
        assert!(resolve_qsrv2_enable(None, Some("YES")).enabled);
        assert!(resolve_qsrv2_enable(None, Some("yes")).enabled);
    }

    #[test]
    fn qsrv2_enable_no_disables_quietly() {
        let d = resolve_qsrv2_enable(None, Some("NO"));
        assert!(!d.enabled, "PVXS_QSRV_ENABLE=NO disables QSRV2");
        assert!(d.error.is_none());
        assert!(d.info.is_none(), "explicit NO is the quiet path");
    }

    #[test]
    fn qsrv2_enable_ignore_servers_qsrv2_overrides_yes() {
        // EPICS_IOC_IGNORE_SERVERS=qsrv2 takes precedence over an explicit
        // PVXS_QSRV_ENABLE=YES (pvxs checks ignore-servers first).
        let d = resolve_qsrv2_enable(Some("qsrv1 qsrv2"), Some("YES"));
        assert!(!d.enabled, "qsrv2 in EPICS_IOC_IGNORE_SERVERS disables");
        assert!(d.info.is_none(), "ignore-servers disable is quiet");
    }

    #[test]
    fn qsrv2_enable_invalid_value_defaults_on_with_error() {
        let d = resolve_qsrv2_enable(None, Some("maybe"));
        assert!(d.enabled, "invalid PVXS_QSRV_ENABLE keeps the default (on)");
        let err = d.error.expect("invalid value must report an error");
        assert!(err.contains("not YES/NO"));
        assert!(err.contains("Defaulting to YES"));
        // pvxs still prints the INFO status line after the inline error.
        assert!(d.info.as_deref().unwrap().contains("ENABLED"));
    }

    /// A QSRV2-disabled provider serves no DB-backed channel: channel_find
    /// is false, create_channel errors, and channel_list is empty even
    /// though the database holds a record. Mirrors a pvxs IOC where
    /// enable2() returned false (no single/group source registered).
    #[tokio::test]
    async fn disabled_provider_serves_no_db_channel() {
        use crate::qsrv::provider::ChannelProvider;
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        db.add_pv("TEST:Y", epics_base_rs::types::EpicsValue::Double(2.0))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new_with_serving(db, false));

        assert!(
            !provider.channel_find("TEST:Y").await,
            "disabled provider must not find a DB record"
        );
        assert!(
            provider.create_channel("TEST:Y").await.is_err(),
            "disabled provider must refuse to create a DB channel"
        );
        assert!(
            ChannelProvider::channel_list(provider.as_ref())
                .await
                .is_empty(),
            "disabled provider lists no DB names"
        );

        // The store over a disabled provider hides the DB record but still
        // serves a natively registered PVA PV (NDPluginPva equivalent).
        let store = QsrvPvStore::new(provider);
        assert!(
            !store.has_pv("TEST:Y").await,
            "DB record hidden when disabled"
        );

        let latest = Arc::new(parking_lot::Mutex::new(Some(PvField::Scalar(
            epics_pva_rs::pvdata::ScalarValue::Int(7),
        ))));
        let subscribers = Arc::new(parking_lot::Mutex::new(Vec::new()));
        store
            .register_pva_pv("NATIVE:PV", latest, subscribers, None)
            .await;
        assert!(
            store.has_pv("NATIVE:PV").await,
            "native PVA PV stays served even when QSRV2 DB serving is off"
        );
    }

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
        let value = PvField::UnionArray(vec![Some(UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(ScalarValue::Int(7)),
        })]);

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

        let value = PvField::UnionArray(vec![Some(UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(7)),
        })]);
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

    /// B1: PVA RPC against a QSRV-served record. A parameterless RPC
    /// degenerates to a GET — the response is the record's current
    /// NTScalar value. Closes the doc gap "QsrvPvStore missing rpc".
    #[tokio::test]
    async fn rpc_on_qsrv_record_without_args_returns_current_value() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::{PvField, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        db.add_record("RPC:AI", Box::new(AiRecord::new(2.5)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        let (desc, value) = store
            .rpc("RPC:AI", FieldDesc::Variant, PvField::Null)
            .await
            .expect("RPC on a QSRV record must succeed");

        assert!(
            matches!(desc, FieldDesc::Structure { .. }),
            "RPC response descriptor must be the record's NT structure"
        );
        let s = match value {
            PvField::Structure(s) => s,
            other => panic!("RPC response must be a structure, got {other}"),
        };
        match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => {
                assert_eq!(*v, 2.5, "RPC must read back the record's current value")
            }
            other => panic!("expected scalar value field, got {other:?}"),
        }
    }

    /// B1: an RPC carrying NTURI query arguments writes them into the
    /// record before reading back — process-and-read, pvxs `pvxcall`
    /// style. `pvcall RPC:AO value=7.0` must leave the record at 7.0.
    #[tokio::test]
    async fn rpc_on_qsrv_record_with_query_args_writes_then_reads() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ao::AoRecord;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        db.add_record("RPC:AO", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        // Build an NTURI request: query.value = 7.0.
        let mut query = PvStructure::new("");
        query
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        let mut nturi = PvStructure::new("epics:nt/NTURI:1.0");
        nturi.fields.push((
            "scheme".into(),
            PvField::Scalar(ScalarValue::String("pva".into())),
        ));
        nturi.fields.push((
            "path".into(),
            PvField::Scalar(ScalarValue::String("RPC:AO".into())),
        ));
        nturi
            .fields
            .push(("query".into(), PvField::Structure(query)));

        let request = PvField::Structure(nturi);
        let (_desc, value) = store
            .rpc("RPC:AO", request.descriptor(), request)
            .await
            .expect("RPC with query args must succeed");

        let s = match value {
            PvField::Structure(s) => s,
            other => panic!("RPC response must be a structure, got {other}"),
        };
        match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => {
                assert_eq!(
                    *v, 7.0,
                    "RPC query arg must have been written to the record"
                )
            }
            other => panic!("expected scalar value field, got {other:?}"),
        }
    }

    /// B1: RPC against an unknown PV is a clean error, not a panic.
    #[tokio::test]
    async fn rpc_on_unknown_pv_errors() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        let err = store
            .rpc("NOPE:NOPV", FieldDesc::Variant, PvField::Null)
            .await
            .expect_err("RPC on a missing PV must error");
        assert!(
            err.message.contains("PV not found"),
            "error must name the missing PV: {err}"
        );
    }

    /// B1: an RPC query field named anything other than `value` on a
    /// single-record channel must be rejected — `pv_structure_to_epics`
    /// only extracts `value`, so a stray field name would otherwise be
    /// silently dropped (a no-op put). The caller deserves a clear
    /// error naming the offending field.
    #[tokio::test]
    async fn rpc_single_record_rejects_non_value_query_field() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ao::AoRecord;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        db.add_record("RPC:AO2", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        // NTURI with query.freq=1.0 — `freq` is not a record field.
        let mut query = PvStructure::new("");
        query
            .fields
            .push(("freq".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        let mut nturi = PvStructure::new("epics:nt/NTURI:1.0");
        nturi
            .fields
            .push(("query".into(), PvField::Structure(query)));
        let request = PvField::Structure(nturi);

        let err = store
            .rpc("RPC:AO2", request.descriptor(), request)
            .await
            .expect_err("non-`value` query field must be rejected");
        assert!(
            err.message.contains("freq") && err.message.contains("value"),
            "error must name the bad field and the accepted one: {err}"
        );
    }

    /// B1: an RPC against a group PV accepts member field names as
    /// query arguments (the `Single`-only `value` restriction must not
    /// fire for `AnyChannel::Group`). `pvcall GRP level=.. count=..`
    /// writes both members and reads the group back.
    #[tokio::test]
    async fn rpc_on_group_pv_writes_members() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

        // members must declare `+putorder` to be writable
        // through group PUT/RPC. Without it pvxs (and now Rust)
        // skip the write silently with a warning.
        const GROUP_JSON: &str = r#"{
            "RPC:GRP": {
                "+id": "epics:nt/NTGroup:1.0",
                "+atomic": true,
                "level": { "+channel": "RPC:GRP:level.VAL", "+type": "plain", "+putorder": 0 },
                "count": { "+channel": "RPC:GRP:count.VAL", "+type": "plain", "+putorder": 1 }
            }
        }"#;

        let db = Arc::new(PvDatabase::new());
        db.add_record("RPC:GRP:level", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("RPC:GRP:count", Box::new(LonginRecord::new(2)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        provider.load_group_config(GROUP_JSON).expect("load group");
        provider.process_groups();
        let store = QsrvPvStore::new(provider);

        // NTURI: query.level=9.0, query.count=8 — both are member paths.
        let mut query = PvStructure::new("");
        query
            .fields
            .push(("level".into(), PvField::Scalar(ScalarValue::Double(9.0))));
        query
            .fields
            .push(("count".into(), PvField::Scalar(ScalarValue::Long(8))));
        let mut nturi = PvStructure::new("epics:nt/NTURI:1.0");
        nturi
            .fields
            .push(("query".into(), PvField::Structure(query)));
        let request = PvField::Structure(nturi);

        let (_desc, value) = store
            .rpc("RPC:GRP", request.descriptor(), request)
            .await
            .expect("RPC on a group PV must accept member query fields");

        let s = match value {
            PvField::Structure(s) => s,
            other => panic!("group RPC response must be a structure, got {other}"),
        };
        assert_eq!(
            s.get_field("level"),
            Some(&PvField::Scalar(ScalarValue::Double(9.0))),
            "group member `level` must reflect the RPC write"
        );
        match s.get_field("count") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => assert_eq!(*v, 8),
            Some(PvField::Scalar(ScalarValue::Int(v))) => assert_eq!(*v as i64, 8),
            other => panic!("group member `count` mismatch: {other:?}"),
        }
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
        let value = PvField::UnionArray(vec![Some(UnionItem {
            selector: 0,
            variant_name: "as_int".into(),
            value: PvField::Scalar(ScalarValue::Int(7)),
        })]);
        store
            .register_pva_pv(
                "TEST:WIRE:UARR",
                Arc::new(parking_lot::Mutex::new(Some(value))),
                Arc::new(parking_lot::Mutex::new(Vec::new())),
                Some(canonical.clone()),
            )
            .await;

        let server =
            PvaServer::start(store, PvaServerConfig::isolated()).expect("test server must start");
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

    /// PUT INIT pvRequest `record._options.process=true` reaches
    /// the bridge via `ChannelContext::pv_request` and is honored as
    /// `ProcessMode::Force`, even when the data-phase value carries no
    /// `_options` substructure. Regression for the prior shape where
    /// options were parsed from the value (the data-phase payload),
    /// which standard PVA clients never put there.
    #[tokio::test]
    async fn put_value_checked_honors_pv_request_process_force() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        let db = Arc::new(PvDatabase::new());
        db.add_record("TEST:proc", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        let store = QsrvPvStore::new(provider);

        // Build an NTScalar PUT value WITHOUT any `record._options`
        // sub-structure (the realistic wire shape — pvxs strips
        // options from the data-phase value).
        let mut value = PvStructure::new("epics:nt/NTScalar:1.0");
        value
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.5))));

        // Build the INIT pvRequest carrying `record._options.process=true`.
        let mut opts = PvStructure::new("");
        opts.fields.push((
            "process".into(),
            PvField::Scalar(ScalarValue::String("true".into())),
        ));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(opts)));
        let mut req = PvStructure::new("");
        req.fields
            .push(("record".into(), PvField::Structure(record)));

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "anonymous".into(),
            method: "anonymous".into(),
            host: "127.0.0.1".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: Some(PvField::Structure(req)),
        };

        let checked = AccessGate::open()
            .check("TEST:proc", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        // Sanity: VAL starts at 0.0.
        let val0 = {
            let rec = db.get_record("TEST:proc").await.unwrap();
            let inst = rec.read().await;
            inst.snapshot_for_field("VAL").map(|s| s.value)
        };
        assert!(matches!(val0, Some(EpicsValue::Double(v)) if v == 0.0));

        store
            .put_value_checked(checked, PvField::Structure(value), ctx)
            .await
            .expect("put_value_checked must succeed");

        // VAL must reflect the put. ProcessMode::Force routes through
        // put_pv + process_record; under either ProcessMode the value
        // lands at 2.5 here — the per-mode semantic divergence is
        // exercised in dedicated tests. The point of THIS test is that
        // option routing from ctx.pv_request reached the bridge: a
        // process=true with no record._options in the value resolves
        // to Force, not silently degraded to Passive.
        let val1 = {
            let rec = db.get_record("TEST:proc").await.unwrap();
            let inst = rec.read().await;
            inst.snapshot_for_field("VAL").map(|s| s.value)
        };
        assert!(
            matches!(val1, Some(EpicsValue::Double(v)) if (v - 2.5).abs() < 1e-9),
            "post-put VAL must be 2.5, got {val1:?}"
        );
    }

    /// Regression: `get_value_checked` must forward the INIT
    /// pvRequest carried on `ChannelContext::pv_request` into
    /// `channel.get(...)`. Before the fix it always passed an empty
    /// `PvStructure::new("")`, so the request's `field` projection (and
    /// group `record._options`) were silently dropped: a GET asking
    /// only for `value` still received every NTScalar sub-field.
    #[tokio::test]
    async fn mr_r13_get_value_checked_forwards_pv_request() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::{PvField, PvStructure};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        let db = Arc::new(PvDatabase::new());
        db.add_record("TEST:mrr13", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        let store = QsrvPvStore::new(provider);

        // INIT pvRequest selecting ONLY the `value` field:
        //   field { value }
        let value_sel = PvStructure::new("");
        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("value".into(), PvField::Structure(value_sel)));
        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let checked = AccessGate::open()
            .check("TEST:mrr13", "127.0.0.1", "anonymous", "anonymous", "")
            .await;
        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "anonymous".into(),
            method: "anonymous".into(),
            host: "127.0.0.1".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: Some(PvField::Structure(req)),
        };

        let got = store
            .get_value_checked(checked, ctx)
            .await
            .expect("get_value_checked must return a value");
        let PvField::Structure(s) = got else {
            panic!("expected a structure result");
        };
        // The field projection must have been honored: only `value`
        // survives. Before the fix the empty request returned the full
        // NTScalar (value + alarm + timeStamp + display + ...).
        assert!(
            s.get_field("value").is_some(),
            "projected `value` field must be present"
        );
        assert_eq!(
            s.fields.len(),
            1,
            "pvRequest `field {{ value }}` must filter the GET to one field, got: {:?}",
            s.fields.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
    }

    /// Regression: a native PVA peer's parsed role/group claims
    /// must survive `ChannelContext` -> `ClientCreds` conversion so
    /// `AcfAccessControl` can enforce role-scoped UAG rules. Before the
    /// fix `ctx_to_creds` hardcoded `roles: Vec::new()`, so role-based
    /// ACF rules denied every real over-the-wire client.
    #[test]
    fn mr_r11_ctx_to_creds_forwards_roles() {
        use epics_pva_rs::server_native::source::ChannelContext;

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "alice".into(),
            method: "ca".into(),
            host: "ws01".into(),
            authority: String::new(),
            roles: vec!["operators".into(), "experts".into()],
            pv_request: None,
        };
        let creds = ctx_to_creds(&ctx);
        assert_eq!(
            creds.roles,
            vec!["operators".to_string(), "experts".to_string()],
            "ctx_to_creds must forward ChannelContext.roles into ClientCreds"
        );
        assert_eq!(creds.user, "alice");
        assert_eq!(creds.method, "ca");
    }

    /// PVA `PROCESS` against a QSRV-backed record actually
    /// runs the record's processing chain (regression vs. the default
    /// trait that silently returned Ok without processing). We verify
    /// by counting the change in `processed_count` after PROCESS.
    #[tokio::test]
    async fn process_runs_record_processing_for_single_record_pvs() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        db.add_record("TEST:proc_call", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        let store = QsrvPvStore::new(provider);

        let before = {
            let rec = db.get_record("TEST:proc_call").await.unwrap();
            let inst = rec.read().await;
            inst.common.time
        };
        // Sleep briefly so the post-process timestamp can be strictly
        // greater than the pre-process timestamp on systems with a
        // coarse clock granularity.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        store
            .process("TEST:proc_call")
            .await
            .expect("PROCESS must run");
        let after = {
            let rec = db.get_record("TEST:proc_call").await.unwrap();
            let inst = rec.read().await;
            inst.common.time
        };
        assert!(
            after >= before,
            "PROCESS must touch the record's TIME (post-process \
             timestamp must be >= pre): before={before:?}, after={after:?}"
        );
        // If TIME is unchanged the clock granularity hid the
        // processing; in that case the test cannot discriminate the
        // fix from the silent-Ok bug. Treat that as a hard
        // fail so a flaky clock doesn't silently lose the regression.
        assert!(
            after > before,
            "TIME must strictly advance after PROCESS (clock too \
             coarse to discriminate the fix): {before:?} -> {after:?}"
        );
    }

    /// PROCESS on a group PV / unknown PV must NOT pretend to
    /// succeed — operators using PROCESS for side-effects need an
    /// honest failure when the operation has no effect.
    #[tokio::test]
    async fn process_rejects_unknown_or_group_pv() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        let err = store
            .process("UNKNOWN:PV")
            .await
            .expect_err("PROCESS on unknown PV must error");
        assert!(
            err.message.contains("UNKNOWN:PV"),
            "error must name the PV; got: {err}"
        );
    }

    /// a native PVA group PUT must honor INIT pvRequest
    /// options. pvxs reads `record._options` from
    /// `putOperation->pvRequest()` (`groupsource.cpp:540`,
    /// `:181` `setForceProcessingFlag`), not from the data-phase
    /// value. The prior bridge `put_value_checked` group arm called
    /// `other.put(&pv)`, which re-parses options from the data-phase
    /// value — so an INIT-only `record._options.process=false` was
    /// silently dropped and every member record was processed anyway.
    ///
    /// Discriminator: a member record's `common.time` advances only
    /// when the member is *processed* (`put_record_field_from_ca`);
    /// `process=false` routes through `put_pv`, which writes the
    /// field without processing. The data-phase value below carries
    /// NO `_options` substructure (the realistic wire shape), so the
    /// option can only reach the group through `ctx.pv_request`.
    #[tokio::test]
    async fn mr_r10_group_put_honors_init_pv_request_process() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        const GROUP_JSON: &str = r#"{
            "MRR10:grp": {
                "+atomic": false,
                "a": { "+channel": "MRR10:a.VAL", "+type": "plain", "+putorder": 0 },
                "b": { "+channel": "MRR10:b.VAL", "+type": "plain", "+putorder": 1 }
            }
        }"#;

        let db = Arc::new(PvDatabase::new());
        db.add_record("MRR10:a", Box::new(LonginRecord::new(0)))
            .await
            .unwrap();
        db.add_record("MRR10:b", Box::new(LonginRecord::new(0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load group");
        let store = QsrvPvStore::new(provider);

        let member_time = |db: Arc<PvDatabase>, rec_name: &'static str| async move {
            let rec = db.get_record(rec_name).await.unwrap();
            let inst = rec.read().await;
            inst.common.time
        };

        let a_before = member_time(db.clone(), "MRR10:a").await;
        let b_before = member_time(db.clone(), "MRR10:b").await;

        // Sleep so a post-processing timestamp would be strictly
        // greater than the pre-write timestamp on a coarse clock.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Data-phase group value: member fields only, NO `_options`.
        let mut value = PvStructure::new("structure");
        value
            .fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Long(11))));
        value
            .fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Long(22))));

        // INIT pvRequest: record._options.process = "false".
        let mut opts = PvStructure::new("");
        opts.fields.push((
            "process".into(),
            PvField::Scalar(ScalarValue::String("false".into())),
        ));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(opts)));
        let mut req = PvStructure::new("");
        req.fields
            .push(("record".into(), PvField::Structure(record)));

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "anonymous".into(),
            method: "anonymous".into(),
            host: "127.0.0.1".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: Some(PvField::Structure(req)),
        };

        let checked = AccessGate::open()
            .check("MRR10:grp", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        store
            .put_value_checked(checked, PvField::Structure(value), ctx)
            .await
            .expect("group put_value_checked must succeed");

        // The values must land regardless of the process option.
        let a_val = {
            let rec = db.get_record("MRR10:a").await.unwrap();
            let inst = rec.read().await;
            inst.snapshot_for_field("VAL").map(|s| s.value)
        };
        assert!(
            matches!(a_val, Some(epics_base_rs::types::EpicsValue::Long(11))),
            "member a VAL must be 11, got {a_val:?}"
        );

        // With `process=false` honored, neither member is processed:
        // `put_pv` writes the field without touching `common.time`.
        // Pre-fix the option was dropped, the members were processed,
        // and the timestamps advanced.
        let a_after = member_time(db.clone(), "MRR10:a").await;
        let b_after = member_time(db.clone(), "MRR10:b").await;
        assert_eq!(
            a_after, a_before,
            "member a TIME must NOT advance: process=false in the INIT \
             pvRequest must suppress member processing (got {a_before:?} \
             -> {a_after:?})"
        );
        assert_eq!(
            b_after, b_before,
            "member b TIME must NOT advance: process=false in the INIT \
             pvRequest must suppress member processing (got {b_before:?} \
             -> {b_after:?})"
        );
    }
}
