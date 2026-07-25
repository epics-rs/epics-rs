//! Adapter that exposes a [`BridgeProvider`] (qsrv) through the native
//! [`epics_pva_rs::server_native::ChannelSource`] trait so that the native
//! PVA server can serve EPICS records (single-record and group composite
//! PVs) plus NTNDArray plugin PVs over pvAccess.
//!
//! All values flow through [`epics_pva_rs::pvdata::PvField`] end-to-end —
//! only native types appear in this module.

// RTEMS-EXEC-MODEL-ALLOW(21): checked - these run and pass in the feature-ON suite.

use epics_pva_rs::server_native::MonitorStream;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;

use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ValueDescMismatch, value_matches_descriptor,
};
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
/// notifications use native [`PvField`] values.
///
/// `descriptor` lets the producer pass the authoritative wire shape,
/// bypassing the lossy [`PvField::descriptor`] recovery for types it
/// cannot reconstruct from the value alone (top-level `UnionArray`,
/// `Union` with sibling variants, empty `ScalarArray`/`StructureArray`).
/// When `None`, `get_introspection` falls back to value-derived recovery
/// — sufficient for structure-rooted normative types where every field
/// is exercised in the value.
///
/// The `latest` snapshot and `subscribers` list are private: the single
/// way a value enters either is [`PvaPvHandle::post`], which validates
/// against `descriptor` before storing or fanning out. This makes
/// "the served value matches the advertised descriptor" hold by
/// construction — the serving owner never needs a serve-time re-check —
/// and reproduces pvxs `SharedPV::post`, which rejects a
/// descriptor-mismatched value before it can become `current`.
#[derive(Clone)]
pub struct PvaPvHandle {
    latest: Arc<parking_lot::Mutex<Option<PvField>>>,
    subscribers: Arc<parking_lot::Mutex<Vec<mpsc::Sender<PvField>>>>,
    descriptor: Option<FieldDesc>,
}

impl PvaPvHandle {
    /// Create an empty native PVA PV handle advertising `descriptor`.
    ///
    /// `latest` starts empty; the only way a value enters it is through
    /// [`PvaPvHandle::post`], which validates against `descriptor` first.
    /// Pass `Some(..)` when the producer knows the canonical wire shape
    /// (e.g. `nt_nd_array_desc()` for NTNDArray, or a custom top-level
    /// `UnionArray`); pass `None` for a descriptor-less diagnostic PV that
    /// accepts any posted value and uses value-derived introspection
    /// (lossy for some types — see [`PvField::descriptor`]).
    pub fn new(descriptor: Option<FieldDesc>) -> Self {
        Self {
            latest: Arc::new(parking_lot::Mutex::new(None)),
            subscribers: Arc::new(parking_lot::Mutex::new(Vec::new())),
            descriptor,
        }
    }

    /// Validate a producer value against the canonical descriptor, then —
    /// only on success — store it as the current snapshot and fan it out
    /// to monitor subscribers. The single owner of every write to
    /// `latest` and every subscriber notification.
    ///
    /// pvxs `SharedPV::post` (`src/sharedpv.cpp:417-431`) checks that the
    /// posted value was built from the descriptor the PV was opened with
    /// and throws *before* `impl->current.assign(val)` on a mismatch, so a
    /// later `fetch()` still returns the last accepted value and monitors
    /// never see the bad frame. This mirrors that contract: on a
    /// descriptor mismatch the previous `latest` is left untouched and no
    /// subscriber receives a frame. A descriptor-less PV (`None`) has no
    /// canonical contract and accepts every post.
    pub fn post(&self, value: PvField) -> Result<(), ValueDescMismatch> {
        if let Some(desc) = &self.descriptor {
            value_matches_descriptor(&value, desc)?;
        }
        // Accepted. Store the snapshot before fanning out, matching pvxs
        // post order (`current.assign` then notify) so a concurrent GET
        // can never observe an older value than a monitor frame.
        *self.latest.lock() = Some(value.clone());
        let mut subs = self.subscribers.lock();
        // A bounded `try_send` fails for two reasons and they are NOT the
        // same. `Closed` means the receiver is gone — reap the subscriber.
        // `Full` means the subscriber is alive but has not drained yet;
        // the frame is lost, the SUBSCRIPTION is not. Collapsing the two
        // (`retain(|tx| tx.try_send(..).is_ok())`) silently unsubscribes a
        // live monitor the first time its queue backs up, and it never
        // receives another value. `SharedPV` already reaps on liveness
        // alone (`shared_pv.rs`: `retain(|tx| !tx.is_closed())`, full queue
        // → squash the tail, pvxs servermon.cpp:283-286); this is the same
        // rule for the QSRV fan-out.
        subs.retain(|tx| match tx.try_send(value.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        Ok(())
    }

    /// The current accepted snapshot, or `None` before the first accepted
    /// post — the analog of pvxs `SharedPV::fetch()`. Already
    /// descriptor-valid by construction (every write goes through
    /// [`PvaPvHandle::post`]), so no serve-time re-check is needed.
    pub fn current_value(&self) -> Option<PvField> {
        self.latest.lock().clone()
    }

    /// Append a monitor subscriber (reaping any already-dropped ones) and
    /// return its receiver. Frames arrive only via [`PvaPvHandle::post`],
    /// so every delivered value already matches the canonical descriptor.
    fn add_subscriber(&self) -> mpsc::Receiver<PvField> {
        let (tx, rx) = mpsc::channel::<PvField>(64);
        let mut subs = self.subscribers.lock();
        subs.retain(|s| !s.is_closed());
        subs.push(tx);
        rx
    }

    /// Canonical introspection descriptor supplied at registration.
    fn descriptor(&self) -> Option<&FieldDesc> {
        self.descriptor.as_ref()
    }
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
/// The handle's `latest` snapshot can only have been written through
/// [`PvaPvHandle::post`], which validates the value against the
/// descriptor before storing it, so a stored value always matches the
/// advertised wire shape by construction — there is no separate
/// registration-time root-kind check to perform.
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
    ///
    /// [`parking_lot::RwLock`], matching [`PvaPvHandle`]'s own interior
    /// state (`latest`, `subscribers`): every critical section here is a
    /// name→handle `contains_key` / `get(..).cloned()` / `insert` with no
    /// I/O in it, so the async lock this used to be bought nothing and cost
    /// a PI-invisible wait on the serve path (doc/qsrv-rtems-design.md §5,
    /// L-B). The guard is `!Send`, and every source method below returns
    /// `impl Future<..> + Send`, so holding one across an `.await` is a
    /// compile error rather than a review finding.
    pva_pvs: Arc<parking_lot::RwLock<HashMap<String, PvaPvHandle>>>,
}

impl QsrvPvStore {
    pub fn new(provider: Arc<BridgeProvider>) -> Self {
        Self {
            provider,
            pva_pvs: Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    pub fn provider(&self) -> &Arc<BridgeProvider> {
        &self.provider
    }

    /// Register a native PVA PV (e.g., NTNDArray from NDPluginPva).
    ///
    /// After registration, the PV is discoverable via `has_pv`, readable
    /// via `get_value`, and subscribable via `subscribe`. The handle
    /// carries its canonical descriptor (set at [`PvaPvHandle::new`]),
    /// which gates every producer [`PvaPvHandle::post`] — so a registered
    /// PV only ever serves descriptor-valid values, by construction.
    /// Synchronous: the whole body is one `parking_lot` write guard over a
    /// `HashMap::insert`, with nothing to await.
    pub fn register_pva_pv(&self, pv_name: &str, handle: PvaPvHandle) {
        self.pva_pvs.write().insert(pv_name.to_string(), handle);
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

/// The leaves a QSRV read ASSIGNS into `value` — what the server frames as
/// the GET reply's / monitor seed's changed-bitset (pvxs `to_wire_valid(R,
/// value, &pvMask)`, `serverget.cpp:104`). `None` = "everything the request
/// selected", for a natively-posted PVA PV whose snapshot is wholly assigned.
///
/// pvxs reads into a `cloneEmpty()`: `singleGet` (`singlesource.cpp:283`) and
/// `getGroupField` (`groupsource.cpp:454-460`) both run `IOCSource::
/// initialize` + `IOCSource::get(…, Everything, …)` per mapping, so only the
/// leaves those two assign carry `valid`. [`super::pvif::read_leaf_paths`] is
/// the single owner of that set; here it is composed per served channel:
///
/// * a GROUP adds `record._options.atomic`, which `onGet` assigns before the
///   field loop (`groupsource.cpp:484`), and skips `Structure`/`Proc` members;
/// * a SINGLE record is one root-level `Scalar` mapping — its NT IS the served
///   structure.
///
/// Each mapping also carries whether its channel addresses the record's VAL
/// field (`channel_is_value_field`), because `IOCSource::initialize` assigns
/// `display.form.index` for VAL only (`iocsource.cpp:53`) — the mark set must
/// not claim it for a `REC.RVAL` channel or a group member bound to a non-VAL
/// field.
///
/// `narrow_enum_value_leaves` then resolves the semantic `value` leaf against
/// the concrete value, so an NTEnum marks `value.index` (assigned by
/// `getScalarValue`) and leaves `value.choices` to the property set.
async fn read_marks(
    provider: &Arc<BridgeProvider>,
    pva_pvs: &Arc<parking_lot::RwLock<HashMap<String, PvaPvHandle>>>,
    name: &str,
    value: &PvField,
) -> Option<Vec<String>> {
    if pva_pvs.read().contains_key(name) {
        return None;
    }
    let paths = match provider.servable_group(name).await {
        Some(def) => {
            let mut paths = vec!["record._options.atomic".to_string()];
            for m in &def.members {
                paths.extend(super::pvif::read_leaf_paths(
                    &m.field_name,
                    m.mapping,
                    super::channel::channel_is_value_field(&m.channel),
                    provider.channel_property_support(&m.channel).await,
                ));
            }
            paths
        }
        // A single-record channel: the NT is the root, mapped as pvxs's
        // `MappingInfo::Scalar` (value + alarm + timeStamp + properties).
        None => super::pvif::read_leaf_paths(
            "",
            super::FieldMapping::Scalar,
            super::channel::channel_is_value_field(name),
            provider.channel_property_support(name).await,
        ),
    };
    let PvField::Structure(root) = value else {
        return Some(paths);
    };
    Some(super::pvif::narrow_enum_value_leaves(paths, root))
}

/// resolve a started monitor for a read-authorized PV.
/// Factored out of `subscribe_checked` so `subscribe_checked_opts_marked`
/// (which carries the `+trigger` marked set to the wire) reuses the
/// exact same native-PVA / channel / DBE / queue-depth resolution.
async fn open_monitor(
    provider: Arc<BridgeProvider>,
    pva_pvs: Arc<parking_lot::RwLock<HashMap<String, PvaPvHandle>>>,
    checked: epics_pva_rs::server_native::source::AccessChecked,
    ctx: epics_pva_rs::server_native::source::ChannelContext,
    opts: epics_pva_rs::server_native::MonitorOptions,
) -> Option<OpenedMonitor> {
    if !checked.allows_read() {
        return None;
    }
    let name = checked.pv_name().to_string();
    if let Some(handle) = pva_pvs.read().get(&name).cloned() {
        // Frames arrive only through `PvaPvHandle::post`, which validates
        // against the canonical descriptor before fanout (pvxs
        // `SharedPV::post`), so the monitor stream can never carry a
        // descriptor-mismatched frame — no serve-time relay needed.
        return Some(OpenedMonitor::Native(handle.add_subscriber()));
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
    // An unconvertible (array-typed) DBE never reaches here: it fails
    // `QsrvPvStore::check_monitor_request` at INIT, which errors the op (CBUG-C2
    // — pvxs resets the whole circuit there instead) before it is registered. If
    // one somehow arrives, take pvxs's `dbe = 0` fallback rather than inventing
    // a third behaviour.
    //
    // R10-37: this START-time parse resolves the MASK only. The
    // `selects empty mask` warning it can raise belongs to INIT — pvxs writes it
    // inside `onSubscribe`, before `connect()` sends the INIT reply — so
    // `check_monitor_request` (the port's INIT half of `onSubscribe`) owns the
    // reporting and this call parses against a log nobody flushes. Passing
    // `ctx.log` here too would emit the message twice, and after the reply.
    let discard = epics_pva_rs::server_native::source::RemoteLog::default();
    let dbe_mask = match ctx.pv_request {
        Some(PvField::Structure(ref req)) => {
            crate::qsrv::channel::dbe_mask_from_pv_request(req, &discard).unwrap_or(None)
        }
        _ => None,
    };
    // R10-33: the negotiated monitor queue limit is the SERVER's, not a
    // second reading of the pvRequest. pvxs `GroupSource::onSubscribe` asks
    // the subscription control what depth it actually got
    // (`subscriptionControl->stats(stats)` → `stats.limitQueue`, which is
    // `MonitorOp::limit`, servermon.cpp:313) and stamps THAT into
    // `record._options.queueSize` (groupsource.cpp:401-404); it never reads
    // the client's `queueSize` option itself. `opts.queue_size` is that
    // limit, resolved once by the server's INIT negotiation.
    let mut monitor = match channel {
        crate::qsrv::AnyChannel::Single(single) => {
            single.create_monitor_with_value_mask(dbe_mask).await.ok()?
        }
        other => other
            .create_monitor()
            .await
            .ok()?
            .with_queue_size(opts.queue_size),
    };
    monitor.start().await.ok()?;
    Some(OpenedMonitor::Db(monitor))
}

/// Spawn the forward task that drains a started DB/group monitor's
/// `poll()` into a cooked [`MonitorUpdate`] stream. Shared by
/// `subscribe_checked_opts_marked` and the gated `subscribe_seeded` so the
/// poll→update conversion lives once. The task ends (and `stop()`s the
/// monitor, dropping its `DbSubscription`s) when the downstream receiver is
/// dropped — i.e. when the PVA op tears down.
fn spawn_db_monitor_updates(
    mut monitor: super::group::AnyMonitor,
) -> mpsc::Receiver<epics_pva_rs::server_native::MonitorUpdate> {
    let (tx, rx) = mpsc::channel::<epics_pva_rs::server_native::MonitorUpdate>(64);
    epics_base_rs::runtime::task::spawn(async move {
        loop {
            // Park on poll() but stay responsive to downstream teardown.
            // An all-const / quiet monitor now *parks* in poll() (it no
            // longer self-signals finish), so a client cancel that drops
            // the downstream receiver must be observed via `tx.closed()`
            // rather than only on the next `tx.send()`. Without this select
            // a parked poll() would keep the monitor — and its member
            // DbSubscriptions — alive forever after the op tore down.
            // `poll() == None` still means a genuine source-close / read
            // error and breaks.
            tokio::select! {
                _ = tx.closed() => break,
                poll = monitor.poll() => {
                    let Some(poll) = poll else { break };
                    let update = epics_pva_rs::server_native::MonitorUpdate {
                        value: PvField::Structure(poll.value),
                        marked: poll.marked,
                        type_changed: false,
                        // direct DB poll loop — no lag/loss accounting here, so no overrun
                        overrun: Vec::new(),
                    };
                    if tx.send(update).await.is_err() {
                        break;
                    }
                }
            }
        }
        monitor.stop().await;
    });
    rx
}

// ── ChannelSource impl (native PvAccess server) ──────────────────────────
//
// In addition to the legacy `PvStore` impl above, expose the same data via
// the native [`epics_pva_rs::server_native::ChannelSource`] trait. This is
// the path used by `epics_pva_rs::server::PvaServer::run_with_source`
// (fully native, no external server runtime).

impl epics_pva_rs::server_native::ChannelSource for QsrvPvStore {
    // thread identity through the type-state
    // `_checked` overrides so every wire op runs against the
    // ACF policy with the correct user/host.

    /// pvxs `SingleSource::onSubscribe` (`ioc/singlesource.cpp:114-140`) reads
    /// `record._options.DBE` with the THROWING `as<T>()`, before `connect()`
    /// emits the INIT reply. An array-typed DBE of integer, real or string
    /// element kind raises `NoConvert` there. This is the INIT-time half of that
    /// read; the mask itself is resolved at START by `open_monitor`, which is
    /// where the port opens the subscription.
    ///
    /// DEVIATION from C++, deliberate — CBUG-C2. QSRV lets that `NoConvert`
    /// unwind out of its bare `connect()` into `conn.cpp:277-282`'s
    /// `bev.reset()`, so one client's malformed `record._options` drops the
    /// shared TCP circuit and every other channel on it. Here the failure is an
    /// `OpError`: this MONITOR gets an error INIT reply, and nothing else on the
    /// circuit notices.
    ///
    /// Scoped to SINGLE-RECORD channels, because that is the only pvxs source
    /// that reads DBE:
    /// * a `pva_pvs`-registered name is served through the PVA `SharedPV` path,
    ///   whose `onSubscribe` reads no `record._options` at all;
    /// * a group name is served by `GroupSource`, whose `onSubscribe`
    ///   (`ioc/groupsource.cpp`) reads `atomic` but never `DBE`.
    ///
    /// Throwing for either of those would be a NEW divergence — pvxs serves
    /// them.
    fn check_monitor_request(
        &self,
        checked: &epics_pva_rs::server_native::source::AccessChecked,
        ctx: &epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), epics_pva_rs::server_native::source::OpError>> + Send
    {
        let pva_pvs = self.pva_pvs.clone();
        let provider = self.provider.clone();
        let name = checked.pv_name().to_string();
        let pv_request = ctx.pv_request.clone();
        // The op's `RemoteLogger` sink — the wire layer drains it after this
        // hook returns Ok, before the INIT reply (R10-37).
        let log = ctx.log.clone();
        async move {
            let Some(PvField::Structure(req)) = pv_request else {
                return Ok(());
            };
            // Split from the `||` this used to be: an `if` condition keeps
            // its temporaries alive to the end of the condition, so a sync
            // guard on the left would still be live across the
            // `is_servable_group` await on the right.
            if pva_pvs.read().contains_key(&name) {
                return Ok(());
            }
            if provider.is_servable_group(&name).await {
                return Ok(());
            }
            // R10-37: this hook IS pvxs's `onSubscribe` DBE read, so it owns
            // BOTH of that read's outcomes — the `NoConvert` throw that resets
            // the circuit, and the `Level::Warn` "selects empty mask" logRemote
            // (`singlesource.cpp:128-130`). pvxs writes that warning before
            // `connect()` emits the INIT reply; the wire layer drains `ctx.log`
            // on this hook's Ok path, before building the reply, so the client
            // sees it in pvxs's order. The START-time re-parse in `open_monitor`
            // discards its log so this is reported exactly once.
            //
            // Scoped to single-record channels by the early return above — the
            // pvxs sources for group and native-PVA names never read DBE, so
            // neither warns. Logging from START (as this used to) warned for
            // them as well.
            crate::qsrv::channel::dbe_mask_from_pv_request(&req, &log)?;
            Ok(())
        }
    }

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
            // Native PVA-pushed PVs (NDPluginPva) carry no per-record ACF
            // — they are always readable. The snapshot is descriptor-valid
            // by construction (every write went through
            // `PvaPvHandle::post`), so serve it directly.
            if let Some(handle) = pva_pvs.read().get(&name).cloned()
                && let Some(value) = handle.current_value()
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

    /// The framed read: the value from `get_value_checked` plus the
    /// leaves QSRV actually assigned into it (`read_marks`). Without this
    /// the GET reply, the PUT_GET readback and the monitor seed all framed
    /// EVERY leaf the request selected — including the seven the port's NT
    /// carries but pvxs's `getProperties` never assigns.
    fn read_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<epics_pva_rs::server_native::source::SourceRead>> + Send
    {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            let name = checked.pv_name().to_string();
            let value = self.get_value_checked(checked, ctx).await?;
            let marked = read_marks(&provider, &pva_pvs, &name, &value).await;
            Some(epics_pva_rs::server_native::source::SourceRead { value, marked })
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
                Some(req) => crate::qsrv::channel::PutOptions::from_pv_request(req, &ctx.log),
                None => crate::qsrv::channel::PutOptions::from_pv_request(&pv, &ctx.log),
            };
            let channel = provider
                .create_channel_with_creds(&name, ctx_to_creds(&ctx))
                .await
                .map_err(|e| OpError::failed(e.to_string()))?;
            match channel {
                crate::qsrv::AnyChannel::Single(single) => single
                    .put_with_options(&pv, opts)
                    .await
                    .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e))),
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
                        .put_with_options(&pv, opts, atomic_override, &ctx.log)
                        .await
                        .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e)))
                }
            }
        }
    }

    /// BitSet-delta PUT (PVA PUT / PUT_GET data phase).
    ///
    /// The generic default (`source.rs` `put_delta_checked`) reads the
    /// PV's current full value and overlays the marked leaves
    /// (`fill_unmarked_from_prior`) before writing. That is correct for
    /// a single record, but for a QSRV *group* the merged full structure
    /// makes every putorder member look client-supplied, so
    /// `GroupChannel::put_with_options` would access-check and
    /// write/process every member — not just the ones the client marked.
    ///
    /// pvxs decodes a PUT into a value carrying mark bits
    /// (`from_wire_valid`, serverget.cpp:443-452) and the group apply
    /// writes only marked members (`marked = leafNode.isMarked(true,true)
    /// && field.value`, groupsource.cpp:547-567). Match that: for a
    /// group, prune the delta to its marked leaves so field presence ==
    /// marked, then hand that to the group apply (which now selects,
    /// access-checks, and writes only present/marked members). Single
    /// records keep the read-merge-write default so unmarked metadata
    /// leaves carry over from the prior value.
    fn put_delta_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        desc: FieldDesc,
        changed: epics_pva_rs::proto::BitSet,
        delta: PvField,
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
            let init_req = match ctx.pv_request {
                Some(PvField::Structure(ref req)) => Some(req),
                _ => None,
            };
            let channel = provider
                .create_channel_with_creds(&name, ctx_to_creds(&ctx))
                .await
                .map_err(|e| OpError::failed(e.to_string()))?;
            match channel {
                crate::qsrv::AnyChannel::Group(group) => {
                    // Prune to the client's marked members (presence ==
                    // marked). Nothing marked → empty apply, which the
                    // group's own "No fields changed" logic treats as a
                    // silent no-op (pvxs `value.isMarked` false).
                    let pv = match epics_pva_rs::pvdata::encode::prune_to_marked(
                        &desc, &changed, 0, delta,
                    ) {
                        Some(PvField::Structure(s)) => s,
                        Some(other) => {
                            return Err(OpError::failed(format!(
                                "qsrv group PUT expects a structure value, got {other}"
                            )));
                        }
                        None => PvStructure::new(""),
                    };
                    let opts = match init_req {
                        Some(req) => {
                            crate::qsrv::channel::PutOptions::from_pv_request(req, &ctx.log)
                        }
                        None => crate::qsrv::channel::PutOptions::from_pv_request(&pv, &ctx.log),
                    };
                    let atomic_override = match init_req {
                        Some(req) => crate::qsrv::channel::atomic_from_pv_request(req),
                        None => crate::qsrv::channel::atomic_from_pv_request(&pv),
                    };
                    group
                        .put_with_options(&pv, opts, atomic_override, &ctx.log)
                        .await
                        .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e)))
                }
                crate::qsrv::AnyChannel::Single(single) => {
                    // Single-record: generic read-merge-write under the
                    // same identity (the record put consumes only the
                    // value leaf, so the whole-structure write is fine).
                    let merged = match self.get_value_checked(checked.clone(), ctx.clone()).await {
                        Some(prior) => epics_pva_rs::pvdata::encode::fill_unmarked_from_prior(
                            &desc, &changed, 0, delta, &prior,
                        ),
                        None => delta,
                    };
                    let pv = match merged {
                        PvField::Structure(s) => s,
                        other => {
                            return Err(OpError::failed(format!(
                                "qsrv PUT expects a structure value, got {other}"
                            )));
                        }
                    };
                    let opts = match init_req {
                        Some(req) => {
                            crate::qsrv::channel::PutOptions::from_pv_request(req, &ctx.log)
                        }
                        None => crate::qsrv::channel::PutOptions::from_pv_request(&pv, &ctx.log),
                    };
                    single
                        .put_with_options(&pv, opts)
                        .await
                        .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e)))
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
    /// the record is processed via `PvDatabase::process_record_with_links`
    /// — the link-aware owner that runs INP/OUT/FLNK traversal, so
    /// alarms / FLNK / output links all run (pvxs routes forced PROCESS
    /// through `dbProcess`, iocsource.cpp:397-417). The bare
    /// `process_record` (process_local + notify) would report success
    /// after only the local record body ran, skipping the link chain.
    /// Single-record-only — group / native PVA PVs have no processing
    /// semantics and are returned an unsupported-op error so callers
    /// don't silently trust a no-op.
    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        let name = name.to_string();
        async move {
            if pva_pvs.read().contains_key(&name) {
                return Err(OpError::failed(format!(
                    "PROCESS not supported for native PVA PV '{name}' (no processing chain)"
                )));
            }
            // Only a *served* group (registered and not shadowed by a
            // backing record) rejects PROCESS as a group PV. A shadowed
            // name is served as the record, so it must process through the
            // record chain below — pvxs serves such a name only as the
            // record (defineGroups, groupconfigprocessor.cpp:170-181).
            if provider.is_servable_group(&name).await {
                return Err(OpError::failed(format!(
                    "PROCESS not supported for group PV '{name}' (no record-level chain)"
                )));
            }
            let (record_name, _field) = epics_base_rs::server::database::parse_pv_name(&name);
            let mut visited = std::collections::HashSet::new();
            provider
                .database()
                .process_record_with_links(record_name, &mut visited, 0)
                .await
                .map_err(|e| OpError::failed(format!("PROCESS on '{name}': {e}")))
        }
    }

    fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            // Legacy cooked path: plain `PvField`s, no marked set. The
            // PVA layer's `subscribe_checked_opts_marked` (below) is what
            // carries the `+trigger` marked set to the wire.
            // No `MonitorOptions` on this legacy entry — nothing was
            // negotiated, so the source sees the per-op defaults (pvxs
            // `MonitorOp::limit = 4u`).
            match open_monitor(
                provider,
                pva_pvs,
                checked,
                ctx,
                epics_pva_rs::server_native::MonitorOptions::default(),
            )
            .await?
            {
                OpenedMonitor::Native(rx) => Some(rx.into()),
                OpenedMonitor::Db(mut monitor) => {
                    let (tx, rx) = mpsc::channel::<PvField>(64);
                    epics_base_rs::runtime::task::spawn(async move {
                        loop {
                            // See `spawn_db_monitor_updates`: park on poll()
                            // but tear down on downstream drop so a quiet /
                            // all-const monitor does not leak after cancel.
                            tokio::select! {
                                _ = tx.closed() => break,
                                poll = monitor.poll() => {
                                    let Some(poll) = poll else { break };
                                    if tx
                                        .send(PvField::Structure(poll.value))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                        monitor.stop().await;
                    });
                    Some(rx.into())
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
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<MonitorStream<epics_pva_rs::server_native::MonitorUpdate>>,
    > + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            match open_monitor(provider, pva_pvs, checked, ctx, opts).await? {
                OpenedMonitor::Native(rx) => Some(
                    epics_pva_rs::server_native::plain_monitor_updates(rx.into()),
                ),
                OpenedMonitor::Db(monitor) => Some(spawn_db_monitor_updates(monitor).into()),
            }
        }
    }

    /// MONITOR seed entry the native PVA server actually uses. QSRV
    /// overrides it (rather than relying on the default
    /// `subscribe_checked_opts_marked` + `get_value` wrapper) so a
    /// DB/group monitor can return a per-op [`MonitorGate`] on the seed:
    /// the wire layer drives it on this op's MONITOR START/STOP edge, and
    /// the gate `db_event_disable`/`db_event_enable`s the backing
    /// `DbSubscription`s — pvxs `onStart(false)` parity
    /// (`singlesource.cpp:151`, `groupsource.cpp`). Native-registered PVA
    /// PVs and the seed for them have no suspendable upstream, so they
    /// return `on_start: None`.
    fn subscribe_seeded(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
        opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<
            epics_pva_rs::server_native::source::SubscriptionSeed<
                epics_pva_rs::server_native::MonitorUpdate,
            >,
        >,
    > + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        let provider_seed = self.provider.clone();
        let pva_pvs_seed = self.pva_pvs.clone();
        async move {
            let opened =
                open_monitor(provider, pva_pvs, checked.clone(), ctx.clone(), opts).await?;
            match opened {
                OpenedMonitor::Native(rx) => {
                    // Native-registered PVA PVs (NDPluginPva etc.) serve the
                    // GET seed and monitor frames from one cached snapshot, so
                    // their `record._options` already match — keep the GET seed.
                    let initial = self.read_checked(checked, ctx).await;
                    Some(epics_pva_rs::server_native::source::SubscriptionSeed {
                        initial,
                        updates: epics_pva_rs::server_native::plain_monitor_updates(rx.into()),
                        on_start: None,
                    })
                }
                OpenedMonitor::Db(monitor) => {
                    // Seed a group monitor from the monitor-stamped value path
                    // (`AnyMonitor::seed`), not the GET path, so the initial
                    // DATA frame's `record._options` (atomic = true, negotiated
                    // queueSize) match the update stream that `poll()` drains
                    // from the same `group_channel`. pvxs delivers the first
                    // group post through the monitor-stamped `currentValue`
                    // (ioc/groupsource.cpp:401-405), whereas the GET path stamps
                    // the operation atomicity and queueSize = 0 (:480-485); the
                    // old GET seed made the first frame disagree with every
                    // later one. A single-record monitor returns `None` from
                    // `seed()` (its GET seed and frames already carry identical
                    // options) and falls back to the GET seed. Read the seed
                    // BEFORE the monitor moves into its forward task; the
                    // subscription is already armed (open_monitor started it),
                    // so any event between seed and stream is buffered in the
                    // monitor's fan-in channel rather than lost or doubled.
                    //
                    // The seed carries the leaves the read assigned, like any
                    // other framed read — plus `record._options.queueSize`,
                    // which `onSubscribe` assigns on the monitor's
                    // `currentValue` and `onGet` does not
                    // (`groupsource.cpp:401-405` vs `:484`).
                    let name = checked.pv_name().to_string();
                    let initial = match monitor.seed().await {
                        Some(value) => {
                            let mut marked =
                                read_marks(&provider_seed, &pva_pvs_seed, &name, &value).await;
                            if let Some(paths) = marked.as_mut() {
                                paths.push("record._options.queueSize".to_string());
                            }
                            Some(epics_pva_rs::server_native::source::SourceRead { value, marked })
                        }
                        None => self.read_checked(checked, ctx).await,
                    };
                    // Capture the enable/disable handles before the monitor
                    // moves into its forward task; `Arc` so the gate closure
                    // (invoked once per START/STOP edge) reuses them.
                    let handles = std::sync::Arc::new(monitor.activation_handles());
                    let updates = spawn_db_monitor_updates(monitor);
                    let gate =
                        epics_pva_rs::server_native::source::MonitorGate::new(move |active| {
                            let handles = handles.clone();
                            async move {
                                for h in handles.iter() {
                                    h.set_active(active).await;
                                }
                            }
                        });
                    Some(epics_pva_rs::server_native::source::SubscriptionSeed {
                        initial,
                        updates: updates.into(),
                        on_start: Some(gate),
                    })
                }
            }
        }
    }

    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let provider = self.provider.clone();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            let mut names = provider.channel_list().await;
            for key in pva_pvs.read().keys() {
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
            if pva_pvs.read().contains_key(&name) {
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
            if let Some(handle) = pva_pvs.read().get(&name_owned).cloned() {
                // Prefer the canonical descriptor supplied at registration
                // (wire-faithful for `UnionArray` and other types that
                // `PvField::descriptor` cannot losslessly reconstruct).
                if let Some(desc) = handle.descriptor().cloned() {
                    return Some(desc);
                }
                if let Some(value) = handle.current_value() {
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
            if let Some(handle) = pva_pvs.read().get(&name_owned).cloned()
                && let Some(value) = handle.current_value()
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
                .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e)))
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
            if pva_pvs.read().contains_key(&name) {
                return false;
            }
            provider.is_writable(&name).await
        }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let name_owned = name.to_string();
        let pva_pvs = self.pva_pvs.clone();
        async move {
            // Native-registered PVA PVs publish into their own subscriber
            // list; `add_subscriber` appends a tx (reaping any already-
            // dropped receivers) so the plugin's `post()` fans out into
            // the PVA server.
            if let Some(handle) = pva_pvs.read().get(&name_owned).cloned() {
                return Some(handle.add_subscriber().into());
            }
            let channel = self.channel(&name_owned).await?;
            let mut monitor = channel.create_monitor().await.ok()?;
            monitor.start().await.ok()?;
            let (tx, rx) = mpsc::channel::<PvField>(64);
            epics_base_rs::runtime::task::spawn(async move {
                // Legacy ctx-less path: plain values, marked set dropped
                // (the marked-aware cooked entry is `subscribe_checked_opts_marked`).
                loop {
                    // See `spawn_db_monitor_updates`: park on poll() but tear
                    // down on downstream drop so a quiet / all-const monitor
                    // does not leak after cancel.
                    tokio::select! {
                        _ = tx.closed() => break,
                        poll = monitor.poll() => {
                            let Some(poll) = poll else { break };
                            if tx.send(PvField::Structure(poll.value)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                monitor.stop().await;
            });
            Some(rx.into())
        }
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

/// Process-global QSRV2 enable decision, computed exactly once from the
/// environment. Pinning it here is the structural single-source-of-truth:
/// the two consumers that run at different iocInit points — the pvalink
/// install seam (`pvalink_link_set_install`, fired at `AfterCaLinkInit`)
/// and the Phase-3 protocol runner (`run_ca_pva_qsrv_ioc`) — read this one
/// cell, so they cannot disagree on whether QSRV2 is on. Mirrors pvxs,
/// where `enable2()` is evaluated once in `pvxsBaseRegistrar` and the
/// single result gates `single_enable()` / `group_enable()` /
/// `pvalink_enable()` together (iochooks.cpp:465-496).
static QSRV2_DECISION: OnceLock<Qsrv2Decision> = OnceLock::new();

/// The single owner of the QSRV2 enable decision. Reads
/// `EPICS_IOC_IGNORE_SERVERS` / `PVXS_QSRV_ENABLE` and applies the pvxs
/// `enable2()` rule on the FIRST call only; every later caller reuses the
/// cached [`Qsrv2Decision`]. Pure and silent — the startup diagnostics are
/// emitted once by [`qsrv2_enabled`] (the runner's authoritative print),
/// so callers that only need the boolean (the install seam) take no
/// diagnostic side effect.
fn qsrv2_decision() -> &'static Qsrv2Decision {
    QSRV2_DECISION.get_or_init(|| {
        let ignore = std::env::var("EPICS_IOC_IGNORE_SERVERS").ok();
        let enable = std::env::var("PVXS_QSRV_ENABLE").ok();
        resolve_qsrv2_enable(ignore.as_deref(), enable.as_deref())
    })
}

/// Apply the cached QSRV2 decision, print the same synchronous startup
/// diagnostics pvxs emits (iochooks.cpp:421-446), and return whether QSRV2
/// database/group serving is enabled. Called once, by the protocol runner,
/// so the `INFO:` / `ERROR:` line prints exactly once at the pvxs-equivalent
/// point.
fn qsrv2_enabled() -> bool {
    let decision = qsrv2_decision();
    if let Some(e) = &decision.error {
        eprintln!("{e}");
    }
    if let Some(i) = &decision.info {
        println!("{i}");
    }
    decision.enabled
}

// ---------------------------------------------------------------------------
// The QSRV mount — the one construction path for a served QSRV source
// ---------------------------------------------------------------------------

/// A built QSRV mount: the `ChannelSource` a PVA server answers QSRV
/// channels from, plus the QSRV2 enable decision that produced it.
///
/// The decision is carried out of [`build_qsrv_mount`] rather than
/// recomputed by the caller because the caller has more to gate on it than
/// serving does — the host runner also gates the QSRV iocsh command set on
/// it (pvxs registers those only inside `if(enableQ)`,
/// iochooks.cpp:492-496).
pub struct QsrvMount {
    /// The source to hand to the PVA server (`Arc<QsrvPvStore>` **is** a
    /// `DynSource`: the blanket `ChannelSourceObj` impl at
    /// `server_native/source.rs:2361` supplies object safety, so there is no
    /// adapter and no boxing here).
    pub store: Arc<QsrvPvStore>,
    /// The cached pvxs `enable2()` decision (iochooks.cpp:401-448).
    pub enabled: bool,
}

/// Build the served QSRV mount: apply the QSRV2 enable decision, construct
/// the provider, install the IOC-wide ACF on it, and finalize the group set
/// — in the order C runs the equivalent hooks.
///
/// **This is the single owner of that sequence.** Both entry points that can
/// serve QSRV go through it: the host dual-protocol runner
/// ([`run_ca_pva_qsrv_ioc`]) and the RTEMS target IOC, which has no iocsh, no
/// `IocApplication` and no CA server and so could not reach the runner. Two
/// hand-rolled copies of "decide, build, load groups, wrap" is how the two
/// would come to disagree about whether `PVXS_QSRV_ENABLE` is honoured or
/// about whether groups are finalized before the first GET — the two
/// properties that are invisible until a client asks.
///
/// The ordering constraint from C (`ioc/iochooks.cpp:343-366`:
/// `processGroups()` at `initHookAfterInitDatabase`, `addGroupSrc()` at
/// `initHookAfterIocBuilt`) is **structural here, not a rule to remember**:
/// the group set is finalized before the [`QsrvPvStore`] that exposes it
/// exists, so a caller cannot add the source to a server before the groups
/// are built. What the caller still owns is the other half — bind and start
/// accepting only after `add_source`.
///
/// `acf` is the IOC-wide access-security configuration, or `None`. Passing
/// `None` leaves the provider on `AllowAllAccess`; passing the same config
/// the CA server got is what keeps the two protocols on one policy, which
/// is the documented configuration trap on the host side.
///
/// `group_files` carries `dbLoadGroup` requests the caller obtained by some
/// route other than the iocsh command — which on the RTEMS target is the
/// only route there is, because the target has no iocsh and therefore
/// nothing ever pushes onto the base startup queue
/// [`take_group_load_requests`](epics_base_rs::server::ioc_app::take_group_load_requests)
/// drains. The target's command line *is* its st.cmd, so a `.json`
/// argument becomes a `GroupLoadRequest` here and reaches exactly the same
/// loader the host's `dbLoadGroup` feeds. The host runner passes an empty
/// slice: its files are already on that queue. Keeping this a parameter of
/// the one owner — rather than letting the target apply group files to the
/// provider itself — is what preserves the ordering guarantee below; a
/// caller that loaded groups on its own would necessarily do it after the
/// store already existed.
pub async fn build_qsrv_mount(
    db: &Arc<epics_base_rs::server::database::PvDatabase>,
    acf: Option<epics_base_rs::server::access_security::AccessSecurityConfig>,
    group_files: &[epics_base_rs::server::ioc_app::GroupLoadRequest],
) -> QsrvMount {
    // ── QSRV2 enable gate (pvxs enable2(), iochooks.cpp:401-496) ──
    // Honor PVXS_QSRV_ENABLE / EPICS_IOC_IGNORE_SERVERS=qsrv2 before standing
    // up the database/group sources. When disabled, the source is still
    // mounted and the PVA server still serves native PVA PVs, but the
    // BridgeProvider answers "absent" for every DB/group channel — matching a
    // pvxs IOC where enable2() returned false.
    let enabled = qsrv2_enabled();

    let provider = Arc::new(BridgeProvider::new_with_serving(db.clone(), enabled));

    // Install the IOC-wide ACF on the QSRV bridge so PVA single-record /
    // group operations enforce the same policy the CA server does. Without
    // this, an IOC launched with an ACF would protect CA but leave the PVA
    // QSRV path on `AllowAllAccess`.
    if let Some(acf_cfg) = acf {
        let acf = Arc::new(super::provider::AcfAccessControl::new(db.clone(), acf_cfg));
        provider.set_access_control(acf);
        tracing::info!("qsrv: ACF installed on BridgeProvider");
    }

    // ── QSRV group loading (pvxs `processGroups()` parity) ──
    // Gated on the QSRV2 enable decision, matching pvxs only adding the group
    // source when `enable2()` is true.
    if enabled {
        load_qsrv_groups(&provider, db, group_files).await;
    }

    QsrvMount {
        store: Arc::new(QsrvPvStore::new(provider)),
        enabled,
    }
}

/// Async link-set installer for
/// [`epics_base_rs::server::ioc_app::IocApplication::register_link_set_installer`].
///
/// Installs the `"pva"` link set (pvalink) on `db` and returns the
/// `pvxr` / `dbpvxr` / `pvalink_enable` / `pvalink_disable` iocsh
/// commands. Registered at IOC construction, it is fired by
/// `IocApplication::run` at the `AfterCaLinkInit` hook — BEFORE
/// `setup_cp_links` and before record processing (PINI), so a `pva://`
/// CP link is opened and its background connect kicked off while iocInit
/// is still running, matching pvxs opening pvalink channels at
/// `initHookAfterIocBuilt` (`linkGlobal_t::init`). Installing pvalink in
/// the Phase-3 protocol runner (where it lived previously) ran after
/// `setup_cp_links`, so a Passive CP holder's warm no-op'd.
///
/// PVA links do NOT participate in the iocInit external-link wait
/// (`wait_for_external_links`): that wait is CA-facility-only, exactly as
/// in C, where `dbCaRun` blocks on CA links alone and pvxs pvalink never
/// blocks iocInit. A `pva://` link that cannot connect therefore does not
/// hold up PINI; it connects asynchronously in the background.
///
/// QSRV2-gated (pvxs `iochooks.cpp:461-496`: `pvalink_enable()` runs
/// only when `enable2()` is true) and feature-gated on `pvalink`. When
/// QSRV2 is disabled or the feature is off this is an empty no-op
/// installer — a database with no PVA links stays fully serviceable,
/// and the shared PVA client is created lazily per link, so there is
/// no init failure to abort the IOC on. The gate reads the shared
/// [`qsrv2_decision`] (silent), so this seam and the runner's
/// `qsrv2_enabled` print always agree on the same one decision; the
/// authoritative startup diagnostic is emitted once by the runner.
pub async fn pvalink_link_set_install(
    db: Arc<epics_base_rs::server::database::PvDatabase>,
) -> Vec<epics_base_rs::server::iocsh::registry::CommandDef> {
    if !qsrv2_decision().enabled {
        return Vec::new();
    }
    #[cfg(feature = "pvalink")]
    {
        let resolver = crate::pvalink::install_pvalink_resolver(&db).await;
        crate::pvalink::register_pvalink_commands(resolver)
    }
    #[cfg(not(feature = "pvalink"))]
    {
        let _ = db;
        Vec::new()
    }
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
///
/// Host-only, and it needs the full `qsrv` selection.
///
/// Two independent requirements, so two clauses, and both are load-bearing:
///
/// * `not(epics_embedded_target)` — both servers this starts are themselves
///   embedded-target-gated in their own crates. `epics_ca_rs::server::CaServer`
///   and `epics_pva_rs::server::PvaServer` are each behind
///   `cfg(not(epics_embedded_target))`, because the target (RTEMS or
///   VxWorks) runs the blocking thread-per-client drivers instead of the
///   async reactor front ends. This function was the last caller that had
///   not followed.
/// * `feature = "qsrv"` — `epics-ca-rs` is a dependency of `qsrv`, not of
///   `qsrv-core`. Gating on the target alone would compile this body in a
///   host `--features qsrv-core` build, where the crate is not linked at all
///   (E0433 on `epics_ca_rs`). A feature whose validity depended on which
///   target you pointed it at would be exactly the dual meaning the
///   `qsrv-core` split exists to avoid.
///
/// The target's equivalent entry point is `epics-bridge-rs`'s `realtime-pva-ioc`.
#[cfg(all(feature = "qsrv", not(epics_embedded_target)))]
pub async fn run_ca_pva_qsrv_ioc(
    config: epics_base_rs::server::ioc_app::IocRunConfig,
) -> epics_base_rs::error::CaResult<()> {
    use epics_base_rs::error::CaError;

    let db = config.db.clone();
    let ca_port = config.port;
    // pvxs owns the PVA port, and it is NOT the CA rule: `PickOne` reads the
    // server-specific `EPICS_PVAS_SERVER_PORT` before the shared
    // `EPICS_PVA_SERVER_PORT` (config.cpp:402-408), and `0` is a legitimate
    // ephemeral-bind request rather than a value to reject. A strict `parse()`
    // here honoured neither.
    let pva_port: u16 = epics_pva_rs::config::env::pvas_server_port();

    // ── QSRV bridge ──
    // The enable gate, the provider, the ACF install and the
    // `processGroups()`-equivalent group load all live in `build_qsrv_mount`
    // — the one owner shared with the RTEMS target IOC, so the two entry
    // points cannot disagree about whether `PVXS_QSRV_ENABLE` was honoured
    // or whether groups were finalized before the first client GET. The ACF
    // is the same `config.acf` the CA server gets via `CaServer::from_parts`
    // below; handing the PVA side a different one is the documented
    // configuration trap.
    let QsrvMount {
        store,
        enabled: qsrv2_on,
        // Empty `group_files`: this runner's `dbLoadGroup` requests already sit
        // on the base startup queue, put there by the iocsh command during
        // st.cmd. The parameter exists for the target IOC, which has no iocsh.
    } = build_qsrv_mount(&db, config.acf.clone(), &[]).await;

    // Register native PVA PVs (NTNDArray from NDPvaConfigure, etc.).
    // Handles were stored in the global registry during st.cmd execution.
    let pva_pvs = take_registered_pva_pvs();
    for (pv_name, handle) in pva_pvs {
        tracing::info!(pv = %pv_name, "registering native PVA PV");
        store.register_pva_pv(&pv_name, handle);
    }

    // ── External links (`calink` + `pvalink`) ──
    // Neither external link set is installed in this Phase-3 runner. Both
    // register at the base `AfterCaLinkInit` hook via
    // `IocApplication::register_link_set_installer(...)`:
    //   * `"ca"`  — `calink_link_set_install`
    //   * `"pva"` — `pvalink_link_set_install`
    // The hook fires BEFORE `setup_cp_links` (which warms Passive `ca://`
    // CP holders) and before record processing, so both link sets' CP
    // links are opened while iocInit runs. Installing either set here in
    // Phase 3 is too late: a Passive `ca://` CP holder's
    // `resolve_external_pv` warm would no-op, and a `pva://` CP link would
    // open only after record processing began.
    //
    // Only the `"ca"` set's local-target links are then held by the
    // iocInit external-link wait (`wait_for_external_links`, CA-facility
    // only, like C `dbCaRun`); `pva://` links connect in the background
    // and never block PINI (pvxs parity — pvalink does not wait at init).
    // `config.shell_commands` already carries the `caxr` / `dbcaxr` /
    // `pvxr` / `dbpvxr` commands those installers returned at the hook.
    let mut shell_commands = config.shell_commands;

    // Register the QSRV runtime (interactive-shell) commands —
    // `processGroups`, `qsrvStats`, `pvxsl`, `pvxgl`, `resetGroups` —
    // bound to the SAME `BridgeProvider` the served store wraps, so a
    // post-iocInit `processGroups` / `pvxgl` acts on the served groups
    // rather than a throwaway provider (pvxs registers these from
    // `group_enable()` / `singlesourcehooks.cpp`). `dbLoadGroup` is the
    // base startup command (pvxs only permits it before iocInit), so it
    // is intentionally absent from this runtime set.
    //
    // Gated on the SAME `qsrv2_on` decision that gates serving and group
    // loading above: pvxs registers `single_enable()` / `group_enable()`
    // commands only inside `if(enableQ)` (iochooks.cpp:492-496), so a
    // QSRV2-disabled IOC must expose none of this QSRV control surface.
    shell_commands.extend(super::iocsh::register_qsrv_runtime_commands_if_enabled(
        qsrv2_on,
        store.provider().clone(),
    ));

    // ── CA server (background) ──
    let ca_server = epics_ca_rs::server::CaServer::from_parts(
        db.clone(),
        ca_port,
        None,
        config.acf.clone(),
        config.autosave_config.clone(),
        config.autosave_manager.clone(),
    )
    .await?;
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

/// Build the served provider's group set from both pvxs group sources,
/// then finalize it — the QSRV equivalent of pvxs `processGroups()`
/// (ioc/groupsourcehooks.cpp:192-213), run here before the PVA server
/// accepts connections so the first client GET already sees finalized
/// group PVs.
///
/// Source order matches pvxs `GroupConfigProcessor`:
///
///  1. DB `info(Q:group, ...)` records (`loadConfigFromDb`) — every record
///     carrying the tag contributes its group definition.
///  2. Queued `dbLoadGroup` files (`loadConfigFiles`) — drained from the
///     base startup queue ([`take_group_load_requests`]) in st.cmd order,
///     followed by `extra` (the caller-supplied requests described on
///     [`build_qsrv_mount`]). On the host exactly the first list is
///     populated; on the RTEMS target, which has no iocsh to run the
///     command, exactly the second is. They are the same request type
///     applied by the same loader, so a group file behaves identically
///     whichever route it arrived by.
///  3. `process_groups()` — validate/resolve trigger references
///     (`resolveTriggerReferences` / `createGroups`).
///
/// Cross-source duplicate fields collapse first-wins inside the provider's
/// field-keyed merge, matching pvxs `fieldConfigMap` accumulation. A
/// failing source logs a warning and is skipped — a malformed group must
/// not abort an otherwise-serviceable IOC.
async fn load_qsrv_groups(
    provider: &Arc<BridgeProvider>,
    db: &Arc<epics_base_rs::server::database::PvDatabase>,
    extra: &[epics_base_rs::server::ioc_app::GroupLoadRequest],
) {
    use epics_base_rs::server::ioc_app::take_group_load_requests;

    // 1. DB info(Q:group) records (pvxs loadConfigFromDb).
    for name in db.all_record_names().await {
        let json = match db.get_record(&name) {
            Some(rec) => rec.read().get_info("Q:group").map(str::to_string),
            None => None,
        };
        if let Some(json) = json {
            if let Err(e) = provider.load_info_group(&name, &json) {
                tracing::warn!(record = %name, "qsrv: info(Q:group) load failed: {e}");
            }
        }
    }

    // 2. Queued dbLoadGroup files (pvxs loadConfigFiles), in st.cmd order,
    //    then the caller's own requests. Exactly one of the two lists is
    //    non-empty in practice — the queue on a host with an iocsh, `extra`
    //    on the target without one — so the concatenation order is not a
    //    behaviour choice, it just keeps one loop over one request type.
    for req in take_group_load_requests().iter().chain(extra) {
        match super::iocsh::apply_group_file(provider, &req.filename, &req.macros) {
            Ok(total) => {
                tracing::info!(file = %req.filename, "qsrv: dbLoadGroup loaded ({total} groups total)");
            }
            Err(e) => {
                tracing::warn!(file = %req.filename, "qsrv: dbLoadGroup failed: {e}");
            }
        }
    }

    // 3. Finalize (pvxs resolveTriggerReferences / createGroups).
    let n = provider.process_groups().await;
    tracing::info!("qsrv: processGroups created {n} group(s)");
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

        let handle = PvaPvHandle::new(None);
        handle
            .post(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(7)))
            .expect("descriptor-less post accepted");
        store.register_pva_pv("NATIVE:PV", handle);
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

        let handle = PvaPvHandle::new(Some(canonical.clone()));
        handle
            .post(value)
            .expect("value matches canonical descriptor");
        store.register_pva_pv("TEST:UARR", handle);

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
        let handle = PvaPvHandle::new(None);
        handle.post(value).expect("descriptor-less post accepted");
        store.register_pva_pv("TEST:UARR_LOSSY", handle);

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

    /// pvxs `SharedPV::post` (sharedpv.cpp:417-431) validates the posted
    /// value against the descriptor the PV was opened with and throws
    /// *before* `impl->current.assign(val)`, so a descriptor-mismatched
    /// post leaves the last accepted value readable via `fetch()` and
    /// never reaches monitors. `PvaPvHandle::post` is the single
    /// validating write owner that reproduces this: a bad post returns
    /// `Err`, leaves `latest` untouched, and fans out nothing — `latest`
    /// can only ever hold a descriptor-valid value, by construction.
    #[tokio::test]
    async fn native_pva_bad_post_keeps_prior_value_and_skips_monitor() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        // Canonical descriptor: NTScalar<Double>.
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let handle = PvaPvHandle::new(Some(desc));

        // Subscribe before any post so the monitor observes the same post
        // sequence the GET path sees (the clone shares `latest`/
        // `subscribers`, so registering a clone keeps both views in sync).
        let mut rx = handle.add_subscriber();

        // A descriptor-matching post becomes current and is delivered.
        let good = {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.set("value", PvField::Scalar(ScalarValue::Double(1.5)));
            PvField::Structure(s)
        };
        handle.post(good.clone()).expect("good post accepted");
        store.register_pva_pv("TEST:NTS", handle.clone());

        assert_eq!(
            store.get_value("TEST:NTS").await,
            Some(good.clone()),
            "accepted value is served"
        );
        assert_eq!(
            rx.try_recv().ok(),
            Some(good.clone()),
            "monitor receives the good frame"
        );

        // A descriptor-mismatched post (Int leaf under a Double
        // descriptor: same root kind and struct_id, wrong scalar type) is
        // rejected before it can touch `latest` or fan out.
        let bad = {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.set("value", PvField::Scalar(ScalarValue::Int(3)));
            PvField::Structure(s)
        };
        handle
            .post(bad)
            .expect_err("descriptor-mismatched post must be rejected");

        // GET still returns the last *accepted* value — not None, not the
        // bad value (pvxs `fetch()` after a thrown post).
        assert_eq!(
            store.get_value("TEST:NTS").await,
            Some(good),
            "rejected post must leave the prior accepted value readable"
        );
        // The monitor saw no frame for the rejected post.
        assert!(
            rx.try_recv().is_err(),
            "rejected post must not reach monitor subscribers"
        );
    }

    /// Boundaries of the fan-out reap predicate in [`PvaPvHandle::post`].
    ///
    /// A bounded `try_send` fails on `Full` AND on `Closed`. Only `Closed`
    /// means the subscriber is dead. The boundaries, one case each:
    ///
    /// - queue below capacity  → delivered, subscriber retained
    /// - queue exactly full    → frame dropped, subscriber RETAINED
    /// - full, then drained    → later posts still arrive (the regression:
    ///   the old `retain(|tx| tx.try_send(v).is_ok())` evicted the live
    ///   subscriber at the moment it filled, so it never received again)
    /// - receiver dropped      → subscriber reaped
    #[tokio::test]
    async fn native_pva_full_monitor_queue_drops_the_frame_not_the_subscriber() {
        use epics_pva_rs::pvdata::{PvField, ScalarValue};

        // Descriptor-less: this test is about the fan-out, not validation.
        let handle = PvaPvHandle::new(None);
        let mut rx = handle.add_subscriber();
        let cap = 64; // `add_subscriber`'s channel capacity.
        let v = |i: i32| PvField::Scalar(ScalarValue::Int(i));

        // Boundary 1: below capacity — delivered, subscriber retained.
        handle.post(v(0)).expect("post accepted");
        assert_eq!(handle.subscribers.lock().len(), 1, "live sub retained");

        // Boundary 2: fill to exactly capacity, then post past it. The
        // overflow frames are lost; the SUBSCRIPTION must survive.
        for i in 1..cap {
            handle.post(v(i)).expect("post accepted");
        }
        for i in cap..(cap + 10) {
            handle
                .post(v(i))
                .expect("post accepted even when the queue is full");
        }
        assert_eq!(
            handle.subscribers.lock().len(),
            1,
            "a FULL queue is backpressure, not death: the subscriber must survive"
        );

        // Boundary 3: drain, then post again — a live subscriber that once
        // filled must still receive. This is what the old predicate broke.
        for i in 0..cap {
            assert_eq!(
                rx.try_recv().ok(),
                Some(v(i)),
                "the queued frames are the first {cap} posts, in order"
            );
        }
        assert!(rx.try_recv().is_err(), "nothing beyond capacity was queued");
        handle.post(v(999)).expect("post accepted");
        assert_eq!(
            rx.try_recv().ok(),
            Some(v(999)),
            "a subscriber that survived a full queue still receives after it drains"
        );

        // Boundary 4: receiver gone (Closed) — reap.
        drop(rx);
        handle.post(v(1000)).expect("post accepted");
        assert!(
            handle.subscribers.lock().is_empty(),
            "a CLOSED receiver is the only condition that reaps a subscriber"
        );
    }

    /// A descriptor-mismatched *first* post (no previously accepted value)
    /// leaves the PV with no current value — matching pvxs before
    /// `open()` / the first accepted post, where `fetch()` has nothing to
    /// return. The bad frame must not be coerced into a fabricated
    /// snapshot.
    #[tokio::test]
    async fn native_pva_bad_first_post_leaves_no_current_value() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        let provider = Arc::new(BridgeProvider::new(db));
        let store = QsrvPvStore::new(provider);

        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let handle = PvaPvHandle::new(Some(desc));

        // First post is descriptor-mismatched (Int leaf) → rejected.
        let bad = {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.set("value", PvField::Scalar(ScalarValue::Int(3)));
            PvField::Structure(s)
        };
        handle
            .post(bad)
            .expect_err("descriptor-mismatched first post must be rejected");

        store.register_pva_pv("TEST:NTS:EMPTY", handle);
        assert_eq!(
            store.get_value("TEST:NTS:EMPTY").await,
            None,
            "no accepted post yet: GET has no current value"
        );
    }

    /// pvxs installs no `onRPC` for QSRV records (singlesource.cpp:427-460);
    /// an RPC EXEC replies "RPC Not Implemented" (serverget.cpp:482-486).
    /// QsrvPvStore must inherit the default "RPC Not Implemented": RPC must
    /// NOT become a write-through, so `pvcall PV value=...` cannot mutate a
    /// record and a parameterless RPC is rejected rather than acting as a
    /// GET.
    #[tokio::test]
    async fn rpc_on_qsrv_record_is_rejected_and_leaves_value_unchanged() {
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

        // Parameterless RPC: rejected (pvxs has no RPC-as-GET for records).
        let err = store
            .rpc("RPC:AO", FieldDesc::Variant, PvField::Null)
            .await
            .expect_err("parameterless RPC on a QSRV record must be rejected");
        assert!(
            err.message == epics_pva_rs::server_native::source::RPC_NOT_IMPLEMENTED,
            "expected unsupported-RPC error, got: {err}"
        );

        // RPC carrying a write must be rejected AND must not mutate.
        let mut query = PvStructure::new("");
        query
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        let mut nturi = PvStructure::new("epics:nt/NTURI:1.0");
        nturi
            .fields
            .push(("query".into(), PvField::Structure(query)));
        let request = PvField::Structure(nturi);
        let err = store
            .rpc("RPC:AO", request.descriptor(), request)
            .await
            .expect_err("RPC write-through on a QSRV record must be rejected");
        assert!(
            err.message == epics_pva_rs::server_native::source::RPC_NOT_IMPLEMENTED,
            "expected unsupported-RPC error, got: {err}"
        );

        // The record value is untouched: RPC never wrote.
        let value = store
            .get_value("RPC:AO")
            .await
            .expect("record GET must return a value");
        let PvField::Structure(s) = value else {
            panic!("expected NTScalar structure");
        };
        assert_eq!(
            s.get_field("value"),
            Some(&PvField::Scalar(ScalarValue::Double(0.0))),
            "rejected RPC must not have written the record"
        );
    }

    /// pvxs installs no `onRPC` for QSRV group PVs
    /// (groupsource.cpp:108-130); RPC against a group is rejected, so a
    /// member cannot be written through `pvcall GRP member=...`.
    #[tokio::test]
    async fn rpc_on_qsrv_group_is_rejected_and_leaves_members_unchanged() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;

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
        provider.process_groups().await;
        let store = QsrvPvStore::new(provider);

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

        let err = store
            .rpc("RPC:GRP", request.descriptor(), request)
            .await
            .expect_err("RPC on a QSRV group must be rejected");
        assert!(
            err.message == epics_pva_rs::server_native::source::RPC_NOT_IMPLEMENTED,
            "expected unsupported-RPC error, got: {err}"
        );

        // Members untouched: the rejected RPC wrote nothing.
        let value = store
            .get_value("RPC:GRP")
            .await
            .expect("group GET must return a value");
        let PvField::Structure(s) = value else {
            panic!("expected group structure");
        };
        assert_eq!(
            s.get_field("level"),
            Some(&PvField::Scalar(ScalarValue::Double(1.0))),
            "rejected group RPC must not have written member `level`"
        );
        match s.get_field("count") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => assert_eq!(*v, 2),
            Some(PvField::Scalar(ScalarValue::Int(v))) => assert_eq!(*v as i64, 2),
            other => panic!("group member `count` changed or missing: {other:?}"),
        }
    }

    /// End-to-end wire test: a top-level `UnionArray` PV registered
    /// with a canonical descriptor is served over real PVA, and the
    /// client recovers the full variants list via `GET_FIELD`. Closes
    /// the loop on the doc claim that wire-faithful round-tripping
    /// now works — the previous unit tests only validated the
    /// `ChannelSource` contract.
    ///
    /// The only test in this module that needs a PVA *client*
    /// (`PvaServer::client_config` is behind `epics-pva-rs/client`), so it is
    /// the only one gated on the full `qsrv` selection rather than
    /// `qsrv-core`. Everything else here drives the `ChannelSource` surface
    /// directly and runs in either.
    #[cfg(feature = "qsrv")]
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
        let handle = PvaPvHandle::new(Some(canonical.clone()));
        handle
            .post(value)
            .expect("value matches canonical descriptor");
        store.register_pva_pv("TEST:WIRE:UARR", handle);

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
            log: Default::default(),
        };

        let checked = AccessGate::open()
            .check("TEST:proc", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        // Sanity: VAL starts at 0.0.
        let val0 = {
            let rec = db.get_record("TEST:proc").unwrap();
            let inst = rec.read();
            inst.snapshot_for_field("VAL").map(|s| s.value)
        };
        assert!(matches!(val0, Some(EpicsValue::Double(v)) if v == 0.0));

        store
            .put_value_checked(checked, PvField::Structure(value), ctx)
            .await
            .expect("put_value_checked must succeed");

        // VAL must reflect the put. ProcessMode::Force routes through
        // put_pv + process_record_with_links; under either ProcessMode the value
        // lands at 2.5 here — the per-mode semantic divergence is
        // exercised in dedicated tests. The point of THIS test is that
        // option routing from ctx.pv_request reached the bridge: a
        // process=true with no record._options in the value resolves
        // to Force, not silently degraded to Passive.
        let val1 = {
            let rec = db.get_record("TEST:proc").unwrap();
            let inst = rec.read();
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
            log: Default::default(),
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
            log: Default::default(),
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
            let rec = db.get_record("TEST:proc_call").unwrap();
            let inst = rec.read();
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
            let rec = db.get_record("TEST:proc_call").unwrap();
            let inst = rec.read();
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

    /// QSRV forced PUT (`record._options.process=true`) and QSRV
    /// PROCESS must run the *link-aware* processing chain, not a
    /// value-only local notification: a record whose FLNK targets a
    /// second record must process that target before the operation
    /// replies success. pvxs routes both through `dbProcess`
    /// (iocsource.cpp:397-417), which runs FLNK / OUT links; the bare
    /// `process_record` (process_local + notify) would skip them.
    ///
    /// Discriminator: B is processed only when A's FLNK chain fires,
    /// which advances B's `common.time`. With the pre-fix local-only
    /// path B's TIME would never move.
    #[tokio::test]
    async fn forced_put_and_process_run_the_flnk_link_chain() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        let db = Arc::new(PvDatabase::new());
        db.add_record("FLNK:a", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("FLNK:b", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // A.FLNK -> B (both default to SCAN=Passive, so FLNK processes B).
        db.put_pv("FLNK:a.FLNK", EpicsValue::String("FLNK:b".into()))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        let store = QsrvPvStore::new(provider);

        let b_time = |db: Arc<PvDatabase>| async move {
            let rec = db.get_record("FLNK:b").unwrap();
            let inst = rec.read();
            inst.common.time
        };

        // --- Forced PUT path ---
        let b_before = b_time(db.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mut value = PvStructure::new("epics:nt/NTScalar:1.0");
        value
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
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
            log: Default::default(),
        };
        let checked = AccessGate::open()
            .check("FLNK:a", "127.0.0.1", "anonymous", "anonymous", "")
            .await;
        store
            .put_value_checked(checked, PvField::Structure(value), ctx)
            .await
            .expect("forced PUT must succeed");
        let b_after_put = b_time(db.clone()).await;
        assert!(
            b_after_put > b_before,
            "forced PUT must run A's FLNK chain and process B \
             (B.TIME must advance): {b_before:?} -> {b_after_put:?}"
        );

        // --- PROCESS path ---
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        store.process("FLNK:a").await.expect("PROCESS must run");
        let b_after_process = b_time(db.clone()).await;
        assert!(
            b_after_process > b_after_put,
            "PROCESS must run A's FLNK chain and process B \
             (B.TIME must advance): {b_after_put:?} -> {b_after_process:?}"
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

    /// PROCESS on a name that is BOTH a record and a (shadowed) group must
    /// process the record, not reject as a group PV. pvxs serves a
    /// record-shadowed group name only as the record (`defineGroups`,
    /// groupconfigprocessor.cpp:170-181). The prior PROCESS gate read the
    /// raw group registry, so a shadowed name was rejected with
    /// "PROCESS not supported for group PV" even though the record is
    /// processable.
    #[tokio::test]
    async fn process_shadowed_group_processes_the_record() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::server_native::ChannelSource;

        let db = Arc::new(PvDatabase::new());
        db.add_record("SP:rec", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        // `SP:grp`'s `+channel` must resolve, or the group is refused at
        // creation and the shadow rule under test is never exercised.
        db.add_record("OTHER", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        // "SP:rec" shadows the record; "SP:grp" has no backing record.
        provider
            .load_group_config(
                r#"{
                    "SP:rec": { "value": { "+channel": "SP:rec.VAL", "+type": "plain" } },
                    "SP:grp": { "value": { "+channel": "OTHER.VAL",  "+type": "plain" } }
                }"#,
            )
            .expect("load group");
        let store = QsrvPvStore::new(provider);

        // The shadowed name processes through the record chain.
        store
            .process("SP:rec")
            .await
            .expect("PROCESS on a record-shadowed group name must process the record");

        // The non-colliding group is still rejected as a group PV.
        let err = store
            .process("SP:grp")
            .await
            .expect_err("PROCESS on a real group PV must error");
        assert!(
            err.message.contains("group PV"),
            "error must classify SP:grp as a group PV; got: {err}"
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
            let rec = db.get_record(rec_name).unwrap();
            let inst = rec.read();
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
            log: Default::default(),
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
            let rec = db.get_record("MRR10:a").unwrap();
            let inst = rec.read();
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

    /// QSRV group PUT with `record._options.process=true` must run a
    /// FORCED record-processing cycle for an ordinary member even when
    /// the member field would not be passively processed — pvxs threads
    /// the full `TriState forceProcessing` into
    /// `doPostProcessing(forceProcessing==True)` (groupsource.cpp:
    /// 563-571, iocsource.cpp:397-420). Before the fix the group apply
    /// collapsed the tri-state to `use_process = process != Inhibit` and
    /// routed Force through `put_record_field_from_ca`, which processes
    /// only a `pp(TRUE)` field on a `SCAN=Passive` record.
    ///
    /// Discriminator: the member record is set to a NON-Passive SCAN, so
    /// the passive put path never processes it (`should_process` gates on
    /// `scan == Passive`). A Passive-mode group PUT must leave `common.time`
    /// untouched; a `process=true` group PUT must force processing and
    /// advance it. Verified for both non-atomic and atomic groups.
    #[tokio::test]
    async fn group_put_forces_processing_of_non_passive_member_on_process_true() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::record::ScanType;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        // One PUT against `group_pv` carrying the given process option
        // (None = omit the option entirely → ProcessMode::Passive).
        async fn do_put(
            store: &QsrvPvStore,
            group_pv: &str,
            member: &str,
            v: f64,
            process: Option<&str>,
        ) {
            let mut value = PvStructure::new("structure");
            value
                .fields
                .push((member.into(), PvField::Scalar(ScalarValue::Double(v))));

            let mut record = PvStructure::new("");
            if let Some(p) = process {
                let mut opts = PvStructure::new("");
                opts.fields.push((
                    "process".into(),
                    PvField::Scalar(ScalarValue::String(p.into())),
                ));
                record
                    .fields
                    .push(("_options".into(), PvField::Structure(opts)));
            }
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
                log: Default::default(),
            };
            let checked = AccessGate::open()
                .check(group_pv, "127.0.0.1", "anonymous", "anonymous", "")
                .await;
            store
                .put_value_checked(checked, PvField::Structure(value), ctx)
                .await
                .expect("group PUT must succeed");
        }

        const GROUP_JSON: &str = r#"{
            "B119:na": {
                "+atomic": false,
                "v": { "+channel": "B119:rna.VAL", "+type": "plain", "+putorder": 0 }
            },
            "B119:at": {
                "+atomic": true,
                "v": { "+channel": "B119:rat.VAL", "+type": "plain", "+putorder": 0 }
            }
        }"#;

        let db = Arc::new(PvDatabase::new());
        for rec in ["B119:rna", "B119:rat"] {
            db.add_record(rec, Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            // Non-Passive scan: a passive put will NOT process this record.
            db.put_pv(
                &format!("{rec}.SCAN"),
                EpicsValue::Enum(ScanType::Sec1.to_u16()),
            )
            .await
            .unwrap();
        }
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load group");
        let store = QsrvPvStore::new(provider);

        let rec_time = |db: Arc<PvDatabase>, rec: &'static str| async move {
            let r = db.get_record(rec).unwrap();
            r.read().common.time
        };

        for (group_pv, rec) in [("B119:na", "B119:rna"), ("B119:at", "B119:rat")] {
            // Passive (process unset): a non-Passive record must NOT be
            // processed — its TIME must stay put.
            let t0 = rec_time(db.clone(), rec).await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            do_put(&store, group_pv, "v", 5.0, None).await;
            let t_passive = rec_time(db.clone(), rec).await;
            assert_eq!(
                t_passive, t0,
                "{group_pv}: passive group PUT must NOT process \
                 non-Passive member {rec} (TIME {t0:?} -> {t_passive:?})"
            );

            // Force (process=true): must process regardless of SCAN.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            do_put(&store, group_pv, "v", 7.0, Some("true")).await;
            let t_force = rec_time(db.clone(), rec).await;
            assert!(
                t_force > t_passive,
                "{group_pv}: process=true group PUT must force-process \
                 member {rec} (TIME {t_passive:?} -> {t_force:?})"
            );
        }
    }

    /// Partial group PUT must write/process only the marked member.
    /// The generic `put_delta_checked` default reads the full group and
    /// overlays marked leaves, so every putorder member ends up present
    /// in the merged structure and gets written/processed. The override
    /// prunes the delta to the marked members (pvxs
    /// groupsource.cpp:547-567 writes only marked members). Discriminator:
    /// a member record's `common.time` advances only when processed.
    #[tokio::test]
    async fn bridge_rs_120_partial_group_put_writes_only_marked_member() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_pva_rs::proto::BitSet;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        const GROUP_JSON: &str = r#"{
            "BR120:grp": {
                "+atomic": false,
                "a": { "+channel": "BR120:a.VAL", "+type": "plain", "+putorder": 0 },
                "b": { "+channel": "BR120:b.VAL", "+type": "plain", "+putorder": 1 }
            }
        }"#;

        let db = Arc::new(PvDatabase::new());
        db.add_record("BR120:a", Box::new(LonginRecord::new(0)))
            .await
            .unwrap();
        db.add_record("BR120:b", Box::new(LonginRecord::new(0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load group");
        let store = QsrvPvStore::new(provider);

        let member_time = |db: Arc<PvDatabase>, rec: &'static str| async move {
            let rec = db.get_record(rec).unwrap();
            let inst = rec.read();
            inst.common.time
        };
        let a_before = member_time(db.clone(), "BR120:a").await;
        let b_before = member_time(db.clone(), "BR120:b").await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Group value descriptor: depth-first bits root=0, a=1, b=2.
        let desc = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Long)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Long)),
            ],
        };
        // Mark ONLY member a; b carries its unmarked type default.
        let mut changed = BitSet::new();
        changed.set(1);
        let mut delta = PvStructure::new("structure");
        delta
            .fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Long(11))));
        delta
            .fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Long(0))));

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "anonymous".into(),
            method: "anonymous".into(),
            host: "127.0.0.1".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
            log: Default::default(),
        };
        let checked = AccessGate::open()
            .check("BR120:grp", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        store
            .put_delta_checked(checked, desc, changed, PvField::Structure(delta), ctx)
            .await
            .expect("partial group PUT must succeed");

        let a_val = {
            let rec = db.get_record("BR120:a").unwrap();
            let inst = rec.read();
            inst.snapshot_for_field("VAL").map(|s| s.value)
        };
        assert!(
            matches!(a_val, Some(epics_base_rs::types::EpicsValue::Long(11))),
            "marked member a must be written to 11, got {a_val:?}"
        );
        let a_after = member_time(db.clone(), "BR120:a").await;
        assert_ne!(
            a_after, a_before,
            "marked member a must be processed (time advances)"
        );

        // Pre-fix the merge made b present, so every member was written
        // and processed; post-fix b is unmarked and must be untouched.
        let b_val = {
            let rec = db.get_record("BR120:b").unwrap();
            let inst = rec.read();
            inst.snapshot_for_field("VAL").map(|s| s.value)
        };
        assert!(
            matches!(b_val, Some(epics_base_rs::types::EpicsValue::Long(0))),
            "unmarked member b must stay 0, got {b_val:?}"
        );
        let b_after = member_time(db.clone(), "BR120:b").await;
        assert_eq!(
            b_after, b_before,
            "unmarked member b must NOT be processed (time unchanged)"
        );
    }

    /// An empty changed BitSet (nothing marked) must write/process no
    /// member — "No fields changed" follows the BitSet. Pre-fix the
    /// read-merge-write default made every member present in the merged
    /// value and processed them all.
    #[tokio::test]
    async fn bridge_rs_120_empty_bitset_group_put_writes_nothing() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_pva_rs::proto::BitSet;
        use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        const GROUP_JSON: &str = r#"{
            "BR120E:grp": {
                "+atomic": false,
                "a": { "+channel": "BR120E:a.VAL", "+type": "plain", "+putorder": 0 },
                "b": { "+channel": "BR120E:b.VAL", "+type": "plain", "+putorder": 1 }
            }
        }"#;

        let db = Arc::new(PvDatabase::new());
        db.add_record("BR120E:a", Box::new(LonginRecord::new(0)))
            .await
            .unwrap();
        db.add_record("BR120E:b", Box::new(LonginRecord::new(0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load group");
        let store = QsrvPvStore::new(provider);

        let member_time = |db: Arc<PvDatabase>, rec: &'static str| async move {
            let rec = db.get_record(rec).unwrap();
            let inst = rec.read();
            inst.common.time
        };
        let a_before = member_time(db.clone(), "BR120E:a").await;
        let b_before = member_time(db.clone(), "BR120E:b").await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let desc = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Long)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Long)),
            ],
        };
        let changed = BitSet::new(); // nothing marked
        let mut delta = PvStructure::new("structure");
        delta
            .fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Long(0))));
        delta
            .fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Long(0))));

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "anonymous".into(),
            method: "anonymous".into(),
            host: "127.0.0.1".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
            log: Default::default(),
        };
        let checked = AccessGate::open()
            .check("BR120E:grp", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        store
            .put_delta_checked(checked, desc, changed, PvField::Structure(delta), ctx)
            .await
            .expect("empty-marked group PUT is a silent no-op");

        assert_eq!(
            member_time(db.clone(), "BR120E:a").await,
            a_before,
            "member a must not be processed when nothing is marked"
        );
        assert_eq!(
            member_time(db.clone(), "BR120E:b").await,
            b_before,
            "member b must not be processed when nothing is marked"
        );
    }

    /// End-to-end: a pvxs-compatible st.cmd `dbLoadGroup(...)` run BEFORE
    /// iocInit, plus a record carrying `info(Q:group, ...)`,
    /// both end up served by the SAME provider the `QsrvPvStore` wraps —
    /// the bug was that group-loading was unwired into the IOC startup and
    /// the runner served a throwaway provider. Exercises the real base
    /// `dbLoadGroup` startup command (queue) and the runner's single-owner
    /// `load_qsrv_groups` build (info records + queued files + finalize).
    #[tokio::test]
    async fn dbloadgroup_before_iocinit_is_served_by_the_same_store() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::ioc_app::{
            db_load_group_startup_command, take_group_load_requests,
        };
        use epics_base_rs::server::iocsh::IocShell;
        use epics_base_rs::server::records::ai::AiRecord;

        // Isolate from any leftover process-global queue state.
        let _ = take_group_load_requests();

        let db = Arc::new(PvDatabase::new());
        db.add_record("GRP:fileval", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("GRP:infoval", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();

        // A record carrying info(Q:group) — pvxs `loadConfigFromDb` source.
        {
            let rec = db.get_record("GRP:infoval").unwrap();
            rec.write().set_info(
                "Q:group",
                r#"{ "INFO:grp": { "+id": "epics:nt/NTScalar:1.0",
                     "value": { "+channel": "VAL", "+type": "plain" } } }"#,
            );
        }

        // st.cmd `dbLoadGroup("file.json")` BEFORE iocInit: run the REAL
        // base startup command through an iocsh shell on a std::thread (the
        // off-runtime execution model `IocApplication::run` itself uses),
        // queueing the request into the process-global dbLoadGroup queue.
        let json = r#"{ "FILE:grp": { "+id": "epics:nt/NTScalar:1.0",
            "value": { "+channel": "GRP:fileval.VAL", "+type": "plain" } } }"#;
        let path = std::env::temp_dir().join("qsrv_e2e_dbloadgroup.json");
        std::fs::write(&path, json).unwrap();
        {
            let db_t = db.clone();
            let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
            let line = format!("dbLoadGroup(\"{}\")", path.display());
            std::thread::spawn(move || {
                let shell = IocShell::new(db_t, bridge);
                shell.register(db_load_group_startup_command());
                shell.execute_line(&line).expect("dbLoadGroup must queue");
            })
            .join()
            .unwrap();
        }

        // Runner single-owner build into the SERVED provider, before serving.
        let provider = Arc::new(BridgeProvider::new_with_serving(db.clone(), true));
        // `&[]`: this case drives the base startup queue (the real
        // `dbLoadGroup` command ran above), which is the host route. The
        // caller-supplied list is the target's route and is exercised by
        // `rtems-pva-ioc`'s own tests.
        load_qsrv_groups(&provider, &db, &[]).await;
        let store = Arc::new(QsrvPvStore::new(provider));

        // Both pvxs group sources are served by the same store/provider.
        let groups = store.provider().groups();
        assert!(
            groups.contains_key("FILE:grp"),
            "dbLoadGroup file group must be served by the same store"
        );
        assert!(
            groups.contains_key("INFO:grp"),
            "info(Q:group) record group must be served by the same store"
        );

        let _ = std::fs::remove_file(&path);
    }
}
