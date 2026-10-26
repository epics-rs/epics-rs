//! Adapter that exposes a [`BridgeProvider`] (qsrv) through the native
//! [`epics_pva_rs::server_native::ChannelSource`] trait so that the native
//! PVA server can serve EPICS records (single-record and group composite
//! PVs) over pvAccess.
//!
//! All values flow through [`epics_pva_rs::pvdata::PvField`] end-to-end —
//! only native types appear in this module.

// RTEMS-EXEC-MODEL-ALLOW(13): checked - these run and pass in the exec-backend
// suite.

use epics_base_rs::types::RecordName;
use epics_pva_rs::server_native::MonitorStream;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure};
use epics_pva_rs::server_native::source::OpError;

use super::provider::{BridgeProvider, Channel, ClientCreds, PvaMonitor};

/// Convert a native-PVA [`epics_pva_rs::server_native::source::ChannelContext`] to a [`ClientCreds`] for
/// `BridgeProvider::create_channel_with_creds`.
///
/// Carries method/authority/roles through so `AcfAccessControl` can
/// evaluate METHOD/AUTHORITY rules and role-based UAG entries — fixing the
/// defect where these were hardcoded to `"anonymous"` / `""`.
fn ctx_to_creds(ctx: &epics_pva_rs::server_native::source::ChannelContext) -> ClientCreds {
    ClientCreds {
        user: ctx.creds.account.clone(),
        host: ctx.creds.host.clone(),
        method: ctx.creds.method.clone(),
        authority: ctx.creds.authority.clone(),
        // forward the native PVA peer's parsed role/group
        // claims so `AcfAccessControl` can evaluate role-based UAG
        // entries (`R member group:ops`). Previously hardcoded empty,
        // so role-scoped ACF rules denied real over-the-wire clients.
        roles: ctx.creds.roles.clone(),
    }
}

/// pvxs's `qsrvGroup` source, backed by a qsrv [`BridgeProvider`].
///
/// Serves group composite PVs and nothing else: single records are
/// `qsrvSingle`'s ([`epics_pva_rs::server::PvDatabaseSource`]), mounted
/// ahead of this by [`QsrvMount::install_channel_sources`].
pub struct QsrvPvStore {
    /// The executor the per-subscription forward tasks are spawned through.
    /// Handed in at construction because the `ChannelSource` methods below
    /// are plain `fn`s returning a future: their prelude can run before any
    /// executor is entered, so the store cannot take the capability itself.
    reactor: epics_base_rs::runtime::task::Reactor,
    provider: Arc<BridgeProvider>,
    /// Per-peer cache of resolved channels, keyed `(peer, pv name)`.
    /// A pvput client mints a fresh op per put, and every op used to
    /// re-run the full name→record/group resolution plus a fresh
    /// `AccessContext`. An entry is served only while BOTH guards hold:
    ///
    /// - the op's `ctx.creds` is the SAME `Arc` the entry was built
    ///   from (`Arc::ptr_eq`) — a re-auth mints a new credentials Arc,
    ///   so a stale identity can never leak into another peer's (or the
    ///   re-authed peer's own) channel, which is the defect a name-only
    ///   channel cache had here before; holding the Arc in the entry
    ///   also makes the pointer comparison ABA-safe, and
    /// - the provider's [`BridgeProvider::group_generation`] is
    ///   unchanged, so `dbLoadGroup`-style registry rewrites force
    ///   re-resolution.
    ///
    /// ACF hot-reload needs no guard: the cached `AccessContext` holds
    /// the provider's live access proxy, so `set_access_control` swaps
    /// are observed by cached channels on their next check. Entries are
    /// evicted on `notify_channel_close`, which the wire layer fires
    /// for explicit DESTROY_CHANNEL and on connection teardown alike.
    channel_cache: ChannelCache,
}

/// See [`QsrvPvStore::channel_cache`].
type ChannelCache =
    Arc<parking_lot::Mutex<HashMap<SocketAddr, HashMap<RecordName, CachedChannel>>>>;

struct CachedChannel {
    creds: Arc<epics_pva_rs::server_native::config::ClientCredentials>,
    generation: u64,
    channel: Arc<super::group::GroupChannel>,
}

/// Resolve `name` through the per-peer cache (see
/// [`QsrvPvStore::channel_cache`] for the freshness guards). A free fn
/// over the captured `Arc`s so the `impl Future + Send` trait methods
/// and the free monitor path can both call it.
async fn cached_channel(
    provider: &Arc<BridgeProvider>,
    cache: &ChannelCache,
    name: &[u8],
    ctx: &epics_pva_rs::server_native::source::ChannelContext,
) -> crate::BridgeResult<Arc<super::group::GroupChannel>> {
    let generation = provider.group_generation();
    if let Some(hit) = cache
        .lock()
        .get(&ctx.peer)
        .and_then(|m| m.get(name))
        .filter(|e| Arc::ptr_eq(&e.creds, &ctx.creds) && e.generation == generation)
        .map(|e| e.channel.clone())
    {
        return Ok(hit);
    }
    let channel = Arc::new(
        provider
            .create_group_channel_with_creds(name, ctx_to_creds(ctx))
            .await?,
    );
    cache.lock().entry(ctx.peer).or_default().insert(
        RecordName::from(name),
        CachedChannel {
            creds: ctx.creds.clone(),
            generation,
            channel: channel.clone(),
        },
    );
    Ok(channel)
}

impl QsrvPvStore {
    pub fn new(
        reactor: epics_base_rs::runtime::task::Reactor,
        provider: Arc<BridgeProvider>,
    ) -> Self {
        Self {
            reactor,
            provider,
            channel_cache: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    pub fn provider(&self) -> &Arc<BridgeProvider> {
        &self.provider
    }

    /// Legacy ctx-less channel path — used only by the default
    /// trait methods (`get_value` / `put_value` / `is_writable` /
    /// `subscribe`) that lack a `ChannelContext`. Anonymous
    /// identity, no per-op caching. The `_checked` overrides
    /// below carry the real `user/host` through to ACF.
    async fn channel(&self, name: &[u8]) -> Option<super::group::GroupChannel> {
        self.channel_for(name, "", "").await
    }

    /// create a channel for the supplied identity, uncached. A
    /// name-only channel cache here once reused one peer's
    /// `AccessContext` for every subsequent peer and silently
    /// bypassed any ACF policy the IOC runner installed; the
    /// ctx-bearing `_checked` paths now cache through
    /// [`QsrvPvStore::channel_cache`], whose peer + creds-Arc
    /// keying is what makes caching safe. This ctx-less path has
    /// no peer to key by, so it stays uncached — metadata is
    /// still cached inside the `BridgeProvider` (record_cache)
    /// so the per-call cost stays low.
    async fn channel_for(
        &self,
        name: &[u8],
        user: &str,
        host: &str,
    ) -> Option<super::group::GroupChannel> {
        let creds = super::provider::ClientCreds {
            user: user.to_string().into(),
            host: host.to_string(),
            ..Default::default()
        };
        self.provider
            .create_group_channel_with_creds(name, creds)
            .await
            .ok()
    }
}

/// The leaves a QSRV read ASSIGNS into `value` — what the server frames as
/// the GET reply's / monitor seed's changed-bitset (pvxs `to_wire_valid(R,
/// value, &pvMask)`, `serverget.cpp:104`). `None` = "everything the request
/// selected", for a name that is no servable group.
///
/// pvxs reads into a `cloneEmpty()`: `singleGet` (`singlesource.cpp:283`) and
/// `getGroupField` (`groupsource.cpp:454-460`) both run `IOCSource::
/// initialize` + `IOCSource::get(…, Everything, …)` per mapping, so only the
/// leaves those two assign carry `valid`. [`super::pvif::read_leaf_paths`] is
/// the single owner of that set; here it is composed per group member, plus
/// `record._options.atomic`, which `onGet` assigns before the field loop
/// (`groupsource.cpp:484`).
///
/// Each mapping also carries whether its channel addresses the record's VAL
/// field, because `IOCSource::initialize` assigns
/// `display.form.index` for VAL only (`iocsource.cpp:54`) — the mark set must
/// not claim it for a `REC.RVAL` channel or a group member bound to a non-VAL
/// field.
///
/// `narrow_enum_value_leaves` then resolves the semantic `value` leaf against
/// the concrete value, so an NTEnum marks `value.index` (assigned by
/// `getScalarValue`) and leaves `value.choices` to the property set.
async fn read_marks(
    provider: &Arc<BridgeProvider>,
    name: &[u8],
    value: &PvField,
) -> Option<Vec<String>> {
    let def = provider.servable_group(name)?;
    let mut paths = vec!["record._options.atomic".to_string()];
    for m in def.channels.iter() {
        paths.extend(super::pvif::read_leaf_paths(
            &m.def.field_name,
            m.def.mapping,
            m.is_value_field(),
            super::provider::member_property_support(provider.database(), m).await,
        ));
    }
    let PvField::Structure(root) = value else {
        return Some(paths);
    };
    Some(super::pvif::narrow_enum_value_leaves(paths, root))
}

/// resolve a started monitor for a read-authorized PV.
/// Factored out of `subscribe_checked` so `subscribe_checked_opts_marked`
/// (which carries the `+trigger` marked set to the wire) reuses the
/// exact same channel / DBE / queue-depth resolution.
async fn open_monitor(
    provider: Arc<BridgeProvider>,
    checked: epics_pva_rs::server_native::source::AccessChecked,
    ctx: epics_pva_rs::server_native::source::ChannelContext,
    opts: epics_pva_rs::server_native::MonitorOptions,
) -> Option<super::group::GroupMonitor> {
    if !checked.allows_read() {
        return None;
    }
    let name = checked.pv_name().clone();
    let group = provider
        .create_group_channel_with_creds(name.as_bytes(), ctx_to_creds(&ctx))
        .await
        .ok()?;
    // R10-33: the negotiated monitor queue limit is the SERVER's, not a
    // second reading of the pvRequest. pvxs `GroupSource::onSubscribe` asks
    // the subscription control what depth it actually got
    // (`subscriptionControl->stats(stats)` → `stats.limitQueue`, which is
    // `MonitorOp::limit`, servermon.cpp:313) and stamps THAT into
    // `record._options.queueSize` (groupsource.cpp:401-404); it never reads
    // the client's `queueSize` option itself. `opts.queue_size` is that
    // limit, resolved once by the server's INIT negotiation.
    let mut monitor = group
        .create_monitor()
        .await
        .ok()?
        .with_queue_size(opts.queue_size);
    if let Err(e) = monitor.start().await {
        // servermon.cpp:598, logging what `onSubscribe` threw.
        if matches!(e, crate::error::BridgeError::SubscriptionFailed) {
            epics_pva_rs::pvxs_log::log(
                epics_pva_rs::pvxs_log::TCP_SETUP,
                epics_pva_rs::pvxs_log::Level::Err,
                format_args!(
                    "Client {} Monitor \"{name}\" onSubscribe() error: {e}",
                    ctx.peer
                ),
            );
        }
        return None;
    }
    Some(monitor)
}

/// Spawn the forward task that drains a started DB/group monitor's
/// `poll()` into a cooked [`epics_pva_rs::server_native::source::MonitorUpdate`] stream. Shared by
/// `subscribe_checked_opts_marked` and the gated `subscribe_seeded` so the
/// poll→update conversion lives once. The task ends (and `stop()`s the
/// monitor, dropping its `DbSubscription`s) when the downstream receiver is
/// dropped — i.e. when the PVA op tears down.
fn spawn_db_monitor_updates(
    reactor: &epics_base_rs::runtime::task::Reactor,
    mut monitor: super::group::GroupMonitor,
) -> mpsc::Receiver<epics_pva_rs::server_native::MonitorUpdate> {
    let (tx, rx) = mpsc::channel::<epics_pva_rs::server_native::MonitorUpdate>(64);
    reactor.spawn(async move {
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
                        value: PvField::Structure(poll.value).into(),
                        marked: poll.marked.map(Into::into),
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

    /// Evict this peer's cached resolution of `name` (see the private
    /// `QsrvPvStore::channel_cache` field docs). The wire layer fires
    /// this once per opened channel, for explicit DESTROY_CHANNEL and
    /// connection teardown alike, so entries never outlive their
    /// channel.
    fn notify_channel_close(
        &self,
        name: &[u8],
        ctx: &epics_pva_rs::server_native::source::ChannelContext,
    ) {
        let mut cache = self.channel_cache.lock();
        if let Some(by_name) = cache.get_mut(&ctx.peer) {
            by_name.remove(name);
            if by_name.is_empty() {
                cache.remove(&ctx.peer);
            }
        }
    }

    fn get_value_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let provider = self.provider.clone();
        let cache = self.channel_cache.clone();
        async move {
            if !checked.allows_read() {
                return None;
            }
            let name = checked.pv_name().clone();
            let channel = cached_channel(&provider, &cache, &name, &ctx).await.ok()?;
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
                        ctx.creds.account,
                        ctx.creds.method,
                        ctx.creds.host
                    );
                    None
                }
            }
        }
    }

    /// The framed read: the value from `get_value_checked` plus the
    /// leaves QSRV actually assigned into it (`read_marks`). Without this
    /// the GET reply, the PUT readback and the monitor seed all framed
    /// EVERY leaf the request selected — including the seven the port's NT
    /// carries but pvxs's `getProperties` never assigns.
    fn read_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<epics_pva_rs::server_native::source::SourceRead>> + Send
    {
        let provider = self.provider.clone();
        async move {
            let name = checked.pv_name().clone();
            let value = self.get_value_checked(checked, ctx).await?;
            let marked = read_marks(&provider, &name, &value).await;
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
        let cache = self.channel_cache.clone();
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "PUT denied by access security: '{}' from {}@{}",
                    checked.pv_name(),
                    ctx.creds.account,
                    ctx.creds.host
                )));
            }
            let name = checked.pv_name().clone();
            let pv = match value {
                PvField::Structure(s) => s,
                other => {
                    return Err(OpError::failed(format!(
                        "qsrv PUT expects a structure value, got {other}"
                    )));
                }
            };
            // prefer the INIT pvRequest for `record._options`,
            // matching pvxs (`iocsource.cpp:430`). The data-phase value
            // is just the delta; per-operation options live in the
            // INIT pvRequest and reach us via `ChannelContext`. Fall
            // back to value-embedded options for callers that did not
            // come through the wire (in-process tests, gateway).
            //
            // the group arm previously called `other.put(&pv)`,
            // which re-parses options from the data-phase value — so a
            // native PVA group PUT whose `record._options.atomic`
            // lives only in the INIT pvRequest was silently ignored.
            // Route the group through `put_with_options` with the same
            // INIT-pvRequest-derived options, matching pvxs
            // `groupsource.cpp:202-204`.
            let init_req = match ctx.pv_request {
                Some(PvField::Structure(ref req)) => Some(req),
                _ => None,
            };
            let opts = match init_req {
                Some(req) => crate::qsrv::channel::PutOptions::from_pv_request(req, &ctx.log),
                None => crate::qsrv::channel::PutOptions::from_pv_request(&pv, &ctx.log),
            };
            let channel = cached_channel(&provider, &cache, &name, &ctx)
                .await
                .map_err(|e| OpError::failed(e.to_string()))?;
            // `atomic` lives in the INIT pvRequest on the wire path; fall
            // back to the value for in-process callers, then to the group
            // default inside `put_with_options` when neither set it.
            let atomic_override = match init_req {
                Some(req) => crate::qsrv::channel::atomic_from_pv_request(req),
                None => crate::qsrv::channel::atomic_from_pv_request(&pv),
            };
            // pvxs `onPutGroup`'s `CurrentOp` spans the whole group put
            // (`groupsource.cpp:591`).
            let put = channel.put_with_options(&pv, opts, atomic_override, &ctx.log);
            epics_pva_rs::server::current_op::attached(&ctx, put)
                .await
                .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e)))
        }
    }

    /// BitSet-delta PUT (PVA PUT data phase).
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
    /// access-checks, and writes only present/marked members).
    fn put_delta_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: epics_pva_rs::proto::BitSet,
        delta: &PvField,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let provider = self.provider.clone();
        let cache = self.channel_cache.clone();
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "PUT denied by access security: '{}' from {}@{}",
                    checked.pv_name(),
                    ctx.creds.account,
                    ctx.creds.host
                )));
            }
            let name = checked.pv_name().clone();
            let init_req = match ctx.pv_request {
                Some(PvField::Structure(ref req)) => Some(req),
                _ => None,
            };
            let channel = cached_channel(&provider, &cache, &name, &ctx)
                .await
                .map_err(|e| OpError::failed(e.to_string()))?;
            // Prune to the client's marked members (presence ==
            // marked). Nothing marked → empty apply, which the
            // group's own "No fields changed" logic treats as a
            // silent no-op (pvxs `value.isMarked` false).
            let pv = match epics_pva_rs::pvdata::encode::prune_to_marked(&desc, &changed, 0, delta)
            {
                Some(PvField::Structure(s)) => s,
                Some(other) => {
                    return Err(OpError::failed(format!(
                        "qsrv group PUT expects a structure value, got {other}"
                    )));
                }
                None => PvStructure::new(""),
            };
            let opts = match init_req {
                Some(req) => crate::qsrv::channel::PutOptions::from_pv_request(req, &ctx.log),
                None => crate::qsrv::channel::PutOptions::from_pv_request(&pv, &ctx.log),
            };
            let atomic_override = match init_req {
                Some(req) => crate::qsrv::channel::atomic_from_pv_request(req),
                None => crate::qsrv::channel::atomic_from_pv_request(&pv),
            };
            let put = channel.put_with_options(&pv, opts, atomic_override, &ctx.log);
            epics_pva_rs::server::current_op::attached(&ctx, put)
                .await
                .map_err(|e| OpError::failed(crate::qsrv::put_status::wire_message(&e)))
        }
    }

    fn subscribe_checked(
        &self,
        checked: epics_pva_rs::server_native::source::AccessChecked,
        ctx: epics_pva_rs::server_native::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let provider = self.provider.clone();
        let reactor = self.reactor.clone();
        async move {
            // Legacy cooked path: plain `PvField`s, no marked set. The
            // PVA layer's `subscribe_checked_opts_marked` (below) is what
            // carries the `+trigger` marked set to the wire.
            // No `MonitorOptions` on this legacy entry — nothing was
            // negotiated, so the source sees the per-op defaults (pvxs
            // `MonitorOp::limit = 4u`).
            let mut monitor = open_monitor(
                provider,
                checked,
                ctx,
                epics_pva_rs::server_native::MonitorOptions::default(),
            )
            .await?;
            let (tx, rx) = mpsc::channel::<PvField>(64);
            reactor.spawn(async move {
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

    /// the cooked MONITOR entry the native PVA server
    /// actually uses. QSRV overrides it to carry each event's resolved
    /// `+trigger` target set (`MonitorPoll::marked`) into the
    /// `MonitorUpdate` stream, so the server emits a wire changed-bitset
    /// matching the configured trigger graph instead of re-deriving a
    /// full mask.
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
        let reactor = self.reactor.clone();
        async move {
            let monitor = open_monitor(provider, checked, ctx, opts).await?;
            Some(spawn_db_monitor_updates(&reactor, monitor).into())
        }
    }

    /// MONITOR seed entry the native PVA server actually uses. QSRV
    /// overrides it (rather than relying on the default
    /// `subscribe_checked_opts_marked` + `get_value` wrapper) so a
    /// DB/group monitor can return a per-op `MonitorGate` on the seed:
    /// the wire layer drives it on this op's MONITOR START/STOP edge, and
    /// the gate `db_event_disable`/`db_event_enable`s the backing
    /// `DbSubscription`s — pvxs `onStart(false)` parity
    /// (`singlesource.cpp:151`, `groupsource.cpp`).
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
        let provider_seed = self.provider.clone();
        async move {
            let mut monitor = open_monitor(provider, checked.clone(), ctx.clone(), opts).await?;
            // START is pvxs `onStartSubscription`: an `enable()` per
            // member subscription, whose posts the drain holds until
            // each has delivered once (`groupsource.cpp:256-297`), so
            // the first update marks only what those events assign.
            // A group with no member subscription has nothing to wait
            // for and posts at once; that post is read here, through
            // the monitor-stamped `GroupMonitor::seed` so its
            // `record._options` match the update stream (`:401-405`).
            let handles = std::sync::Arc::new(monitor.activation_handles());
            let initial = if handles.is_empty() {
                let name = checked.pv_name().clone();
                match monitor.seed().await.map(PvField::Structure) {
                    Some(value) => {
                        let mut marked = read_marks(&provider_seed, &name, &value).await;
                        if let Some(paths) = marked.as_mut() {
                            paths.push("record._options.queueSize".to_string());
                        }
                        Some(epics_pva_rs::server_native::source::SourceRead { value, marked })
                    }
                    None => self.read_checked(checked, ctx).await,
                }
            } else {
                None
            };
            // The source's drain posts into the monitor's ring, which
            // the op reads directly: no task per monitor. The gate
            // owns the monitor, so the op's teardown (which drops
            // the gate) cancels its subscriptions — pvxs's
            // subscription context owns both its `dbChannel`s and
            // its `onStart`.
            let updates = monitor.take_updates()?;
            let db = provider_seed.database().clone();
            let gate = epics_pva_rs::server_native::source::MonitorGate::new(move |active| {
                let _owner = &monitor;
                let handles = handles.clone();
                let db = db.clone();
                async move {
                    for h in handles.iter() {
                        if active {
                            h.enable(&db).await;
                        } else {
                            h.set_active(false).await;
                        }
                    }
                }
            });
            Some(epics_pva_rs::server_native::source::SubscriptionSeed {
                initial,
                updates: epics_pva_rs::server_native::source::MonitorStream::Ring(updates),
                on_start: Some(gate),
            })
        }
    }

    fn list_pvs(&self) -> Vec<RecordName> {
        let mut names: Vec<RecordName> = self
            .provider
            .group_list()
            .into_iter()
            .map(RecordName::from)
            .collect();
        names.sort();
        names
    }

    /// pvxs `GroupSource::show` (`ioc/groupsource.cpp:152-157`): the group
    /// names only (`:80-87`).
    fn show(&self) -> epics_pva_rs::server_native::SourceShow {
        epics_pva_rs::server_native::SourceShow {
            kind: "IOC",
            names: self
                .provider
                .group_list()
                .into_iter()
                .map(RecordName::from)
                .collect(),
            nested: false,
        }
    }

    /// pvxs `onCreate`: the channel can be built. `AI.VAL$` passes the
    /// search below and is refused here, as `dbChannelCreate` refuses `$` on
    /// a field that is neither a string nor a link.
    fn has_pv(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
        let provider = self.provider.clone();
        let name = RecordName::from(name);
        async move { provider.group_claimed(name.as_bytes()) }
    }

    /// pvxs `onSearch`: `dbChannelTest`, which resolves the record and field
    /// and ignores what follows them (`singlesource.cpp:466-473`).
    fn searchable(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
        let provider = self.provider.clone();
        let name = RecordName::from(name);
        async move { provider.group_claimed(name.as_bytes()) }
    }

    fn get_introspection(
        &self,
        name: &[u8],
    ) -> impl std::future::Future<Output = Option<epics_pva_rs::pvdata::FieldDesc>> + Send {
        let name_owned = RecordName::from(name);
        async move {
            let channel = self.channel(&name_owned).await?;
            channel.get_field().await.ok()
        }
    }

    fn get_value(&self, name: &[u8]) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let name_owned = RecordName::from(name);
        async move {
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
        name: &[u8],
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let name_owned = RecordName::from(name);
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

    fn is_writable(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
        let provider = self.provider.clone();
        let name = RecordName::from(name);
        async move {
            // previously returned `true` for any existing PV via
            // channel_find, lying for read-only records (DISP=1) and
            // delaying the PUT refusal until the actual write attempt.
            // Now consult provider.is_writable (DISP-aware).
            provider.is_servable_group(name.as_bytes())
                && provider.is_writable(name.as_bytes()).await
        }
    }

    fn subscribe(
        &self,
        name: &[u8],
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let name_owned = RecordName::from(name);
        let reactor = self.reactor.clone();
        async move {
            let group = self.channel(&name_owned).await?;
            let mut monitor = group.create_monitor().await.ok()?;
            monitor.start().await.ok()?;
            let (tx, rx) = mpsc::channel::<PvField>(64);
            reactor.spawn(async move {
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

/// The pvxs `enable2()` decision: whether QSRV2's iocsh commands register, plus
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
/// and the QSRV mount build (`build_qsrv_mount`) — read this one
/// cell, so they cannot disagree on whether QSRV2 is on. Mirrors pvxs,
/// where `enable2()` is evaluated once in `pvxsBaseRegistrar` and the
/// single result gates `single_enable()` / `group_enable()` /
/// `pvalink_enable()` together (iochooks.cpp:465-496).
static QSRV2_DECISION: OnceLock<Qsrv2Decision> = OnceLock::new();

/// The single owner of the QSRV2 enable decision. Reads
/// `EPICS_IOC_IGNORE_SERVERS` / `PVXS_QSRV_ENABLE`, applies the pvxs
/// `enable2()` rule and prints its diagnostics on the FIRST call only, as
/// `enable2()` does from `pvxsBaseRegistrar` before the st.cmd runs
/// (iochooks.cpp:459-462); every later caller reuses the cached
/// [`Qsrv2Decision`].
fn qsrv2_decision() -> &'static Qsrv2Decision {
    QSRV2_DECISION.get_or_init(|| {
        let ignore = epics_base_rs::runtime::env::get_lossy("EPICS_IOC_IGNORE_SERVERS");
        let enable = epics_base_rs::runtime::env::get_lossy("PVXS_QSRV_ENABLE");
        let decision = resolve_qsrv2_enable(ignore.as_deref(), enable.as_deref());
        if let Some(e) = &decision.error {
            epics_base_rs::stderr_println!("{e}");
        }
        if let Some(i) = &decision.info {
            println!("{i}");
        }
        decision
    })
}

// ---------------------------------------------------------------------------
// The QSRV mount — the one construction path for a served QSRV source
// ---------------------------------------------------------------------------

/// A built QSRV mount: the `ChannelSource` a PVA server answers QSRV
/// channels from.
pub struct QsrvMount {
    /// The source to hand to the PVA server (`Arc<QsrvPvStore>` **is** a
    /// `DynSource`: the blanket `ChannelSourceObj` impl at
    /// `server_native/source.rs:2361` supplies object safety, so there is no
    /// adapter and no boxing here).
    pub store: Arc<QsrvPvStore>,
    /// The IOC's live ACF cell, so `qsrvSingle` gates on the same policy
    /// the bridge's `AcfAccessControl` adopts for groups.
    pub acf: epics_base_rs::server::access_security::AcfCell,
}

/// Build the served QSRV mount: apply the QSRV2 enable decision, construct
/// the provider, install the IOC-wide ACF on it, and finalize the group set
/// — in the order C runs the equivalent hooks.
///
/// **This is the single owner of that sequence.** Both entry points that can
/// serve QSRV go through it: the host dual-protocol runner
/// (`run_ca_pva_qsrv_ioc_app`) and the RTEMS target IOC, which has no iocsh, no
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
/// `acf` is the IOC's live access-security policy cell — the SAME cell the
/// CA/PVA servers and the iocsh `asInit` command use (`IocRunConfig.acf`),
/// which is what keeps every protocol on one policy and lets a runtime
/// `asInit`/ACF reload re-gate QSRV operations. A cell holding `None`
/// behaves as `AllowAllAccess` until a policy is stored.
///
/// `group_files` carries `dbLoadGroup` requests the caller obtained by some
/// route other than the iocsh command — which on the RTEMS target is the
/// only route there is, because the target has no iocsh and therefore
/// nothing ever pushes onto the base startup queue
/// [`epics_base_rs::server::ioc_app::take_group_load_requests`]
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
    acf: epics_base_rs::server::access_security::AcfCell,
    group_files: &[epics_base_rs::server::ioc_app::GroupLoadRequest],
) -> QsrvMount {
    let provider = Arc::new(BridgeProvider::new(db.clone()));

    // Install the IOC-wide ACF cell on the QSRV bridge so PVA
    // single-record / group operations enforce the same policy the CA
    // server does. Installed unconditionally — a cell holding `None`
    // grants like `AllowAllAccess`, and installing it even then is what
    // lets a post-boot `asInit` (which stores into this same cell) turn
    // access security ON for QSRV without a restart.
    provider.set_access_control(Arc::new(super::provider::AcfAccessControl::adopting(
        db.clone(),
        acf.clone(),
    )));
    if acf.load().is_some() {
        tracing::info!("qsrv: ACF installed on BridgeProvider");
    }

    // pvxs `processGroups()` runs whatever `enable2()` decided
    // (iochooks.cpp:354-355); `group_files` are a command line's
    // `dbLoadGroup` lines, which QSRV2 off leaves unregistered.
    let group_files = if qsrv2_decision().enabled {
        group_files
    } else {
        for _ in group_files {
            epics_base_rs::stderr_println!("Command 'dbLoadGroup' not registered.");
        }
        &[]
    };
    load_qsrv_groups(&provider, db, group_files).await;

    QsrvMount {
        store: Arc::new(QsrvPvStore::new(
            epics_base_rs::runtime::task::Reactor::current()
                .expect("build_qsrv_mount is awaited on the IOC's reactor"),
            provider,
        )),
        acf,
    }
}

impl QsrvMount {
    /// pvxs's `addSingleSrc()` and `addGroupSrc()`, which run at
    /// `initHookAfterIocBuilt` whatever `enable2()` decided
    /// (`ioc/iochooks.cpp:367-372`): `qsrvSingle` at order 0
    /// (`ioc/singlesourcehooks.cpp:159`) and `qsrvGroup` at 1
    /// (`ioc/groupsourcehooks.cpp:219`), "lower order first". A single record
    /// is in the database under its own name and answers at order 0; a group
    /// PV is not, so it falls through to the QSRV store at order 1.
    ///
    /// Errors carry the [`CompositeSource`](epics_pva_rs::server_native::composite::CompositeSource)
    /// rejection verbatim (a duplicate `(name, order)`); the caller decides
    /// whether that is fatal.
    pub fn install_channel_sources(
        &self,
        db: &Arc<epics_base_rs::server::database::PvDatabase>,
        composite: &epics_pva_rs::server_native::composite::CompositeSource,
    ) -> Result<(), String> {
        composite.add_source(
            "qsrvSingle",
            Arc::new(epics_pva_rs::server::PvDatabaseSource::new_with_acf(
                db.clone(),
                self.acf.clone(),
            )),
            0,
        )?;
        composite.add_source("qsrvGroup", self.store.clone(), 1)
    }
}

/// Async link-set installer for
/// [`epics_base_rs::server::ioc_app::IocApplication::register_link_type`].
///
/// Installs the `"pva"` link set (pvalink) on `db` and returns its iocsh
/// commands. `run_qsrv_ioc_app` registers it for every QSRV route; only a
/// custom runner registers it itself. It is fired by
/// `IocApplication::run` at the `AfterCaLinkInit` hook — BEFORE
/// `setup_cp_links` and before record processing (PINI), so a pva
/// CP link is opened and its background connect kicked off while iocInit
/// is still running, matching pvxs opening pvalink channels at
/// `initHookAfterIocBuilt` (`linkGlobal_t::init`). Installing pvalink in
/// the Phase-3 protocol runner (where it lived previously) ran after
/// `setup_cp_links`, so a Passive CP holder's warm no-op'd.
///
/// PVA links do NOT participate in `dbCaRun`'s wait, which blocks on CA
/// links alone; pvxs pvalink never blocks iocInit. A pva link that cannot connect therefore does not
/// hold up PINI; it connects asynchronously in the background.
///
/// pvxs allocates and starts its link machinery whatever `enable2()`
/// decided (`iochooks.cpp:338-341,367-369`); the decision gates only
/// `pvalink_enable()`'s registrations (`iochooks.cpp:503-507`). With the
/// `pvalink` feature off this is an empty no-op installer.
pub async fn pvalink_link_set_install(
    db: Arc<epics_base_rs::server::database::PvDatabase>,
) -> Vec<epics_base_rs::server::iocsh::registry::CommandDef> {
    #[cfg(feature = "pvalink")]
    {
        let resolver = crate::pvalink::install_pvalink_resolver(&db).await;
        let mut commands = crate::pvalink::register_pvalink_commands(resolver.clone());
        if qsrv2_decision().enabled {
            commands.extend(crate::pvalink::pvalink_enable_commands(resolver));
        }
        commands
    }
    #[cfg(not(feature = "pvalink"))]
    {
        let _ = db;
        Vec::new()
    }
}

/// Runs a combined CA + PVA IOC with QSRV bridge.
///
/// Runs `app` through [`epics_base_rs::server::ioc_app::IocApplication::run`]: starts a CA
/// server in the background, creates a `QsrvPvStore` wrapping the database,
/// then runs the PVA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// run_ca_pva_qsrv_ioc_app(IocApplication::new().startup_script("st.cmd")).await
/// ```
///
/// Needs the full `qsrv` selection: `epics-ca-rs` is a dependency of `qsrv`,
/// not of `qsrv-core`. Both backends: the CA leg is `CaServer` on tokio and
/// `BlockingCaServing` on the exec backend, as `run_ca_ioc` forks.
///
/// The target's equivalent entry point is `epics-bridge-rs`'s `realtime-pva-ioc`.
/// Stand `app` up on CA + pvAccess + QSRV, with both protocol registrars
/// already run and the QSRV mount built at `initHookAfterInitDatabase`.
///
/// C and pvxs each register their shell commands from a registrar the `.dbd`
/// names, so `casr` and `pvxsr` answer on the first `st.cmd` line. Rust has
/// no link-time registrar, so this port must call them, and a call that every
/// application head has to remember is a rule that holds until one forgets —
/// measured on `scope-ioc`, whose script got `Command 'pvxsr' not registered`
/// and `Command 'casr' not registered` from a head that had nothing wrong
/// with it. Pairing the two registrars with the runner is what removes the
/// choice.
///
/// pvxs runs `processGroups` at `initHookAfterInitDatabase`
/// (iochooks.cpp:355), before `scanInit` and `initialProcess`; a protocol
/// runner runs after `iocRun`, so the mount is built at that hook and handed
/// over, and there is no runner entry point that builds it late.
#[cfg(feature = "qsrv")]
pub async fn run_ca_pva_qsrv_ioc_app(
    app: epics_base_rs::server::ioc_app::IocApplication,
) -> epics_base_rs::error::CaResult<()> {
    let app = epics_ca_rs::server::iocsh::register_rsrv_commands(app);
    let app = epics_pva_rs::server::iocsh::pvxs_base_registrar_with_enable2(app, || {
        qsrv2_decision();
    });
    // No `group_files`: this IOC's `dbLoadGroup` requests sit on the base
    // startup queue, put there by the iocsh command during st.cmd.
    run_qsrv_ioc_app(app, Vec::new(), serve_ca_pva_qsrv).await
}

/// Run `app` with [`build_qsrv_mount`] at `initHookAfterInitDatabase`, ahead
/// of `scanInit` and `initialProcess` as pvxs `processGroups` is
/// (iochooks.cpp:355), and hand the mount to `serve` at `iocRun`.
///
/// Every `IocApplication` route that serves QSRV goes through here, so none
/// can order the mount against scan start by hand.
#[cfg(feature = "qsrv")]
pub async fn run_qsrv_ioc_app<F, Fut>(
    app: epics_base_rs::server::ioc_app::IocApplication,
    group_files: Vec<epics_base_rs::server::ioc_app::GroupLoadRequest>,
    serve: F,
) -> epics_base_rs::error::CaResult<()>
where
    F: FnOnce(epics_base_rs::server::ioc_app::IocRunConfig, QsrvMount) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = epics_base_rs::error::CaResult<()>> + Send + 'static,
{
    // pvxsBaseRegistrar's point: the decision and its INFO line come before
    // the st.cmd, not at iocInit, and `group_enable()` registers
    // `dbLoadGroup` there (iochooks.cpp:503-507).
    let app = if qsrv2_decision().enabled {
        app.register_startup_command(
            epics_base_rs::server::ioc_app::db_load_group_startup_command(),
        )
    } else {
        app
    };
    // pvxs's `link("pva", "lsetPVX")` (`ioc/pvxs7x.dbd:2`) is in the `.dbd`
    // whether or not QSRV2 then enables pvalink, so `{pva:…}` loads either way.
    // Both link sets are installed here, the one place for every QSRV route:
    // at `AfterCaLinkInit`, before link init refuses a scheme with no set.
    // `ca` because softIocPVX links dbCa as every softIoc does.
    let app = app
        .register_link_type("ca", {
            let reactor = epics_base_rs::runtime::task::Reactor::current()
                .expect("run_qsrv_ioc_app is awaited on the IOC's reactor");
            move |db| epics_ca_rs::calink::calink_link_set_install(db, reactor)
        })
        .register_link_type("pva", pvalink_link_set_install);
    let slot = Arc::new(parking_lot::Mutex::new(None::<QsrvMount>));
    let built = slot.clone();
    let app = app.register_after_init_database(move |db, acf| async move {
        *built.lock() = Some(build_qsrv_mount(&db, acf, &group_files).await);
    });
    app.run(move |config| async move {
        let mount = slot
            .lock()
            .take()
            .expect("iocBuild announces initHookAfterInitDatabase before iocRun");
        serve(config, mount).await
    })
    .await
}

#[cfg(feature = "qsrv")]
async fn serve_ca_pva_qsrv(
    mut config: epics_base_rs::server::ioc_app::IocRunConfig,
    mount: QsrvMount,
) -> epics_base_rs::error::CaResult<()> {
    use epics_base_rs::error::CaError;

    let db = config.db.clone();
    let ca_port = config.port;

    // ── QSRV bridge ──
    // Built by `build_qsrv_mount` at `initHookAfterInitDatabase` — the one
    // owner shared with the RTEMS target IOC — over the same ACF cell the CA
    // server adopts via `RsrvServing::start` below.
    let composite = epics_pva_rs::server_native::composite::CompositeSource::new();
    mount
        .install_channel_sources(&db, &composite)
        .map_err(CaError::InvalidValue)?;
    let QsrvMount { store, .. } = mount;

    // ── External links (`calink` + `pvalink`) ──
    // Neither external link set is installed in this Phase-3 runner. Both
    // register at the base `AfterCaLinkInit` hook via
    // `IocApplication::register_link_type(...)`:
    //   * `"ca"`  — `calink_link_set_install`, by `run_qsrv_ioc_app`
    //   * `"pva"` — `pvalink_link_set_install`, by `run_qsrv_ioc_app`
    // The hook fires BEFORE `setup_cp_links` (which warms Passive CA
    // CP holders) and before record processing, so both link sets' CP
    // links are opened while iocInit runs. Installing either set here in
    // Phase 3 is too late: a Passive CA CP holder's
    // `resolve_external_pv` warm would no-op, and a pva CP link would
    // open only after record processing began.
    //
    // Only the `"ca"` set's local-target links are then held by `dbCaRun`'s
    // wait; pva links connect in the background and never block
    // iocInit (pvxs parity — pvalink does not wait at init).
    // The `dbcaxr` / `pvxr` / `dbpvxr` commands those installers
    // return are registered on the process's one command table at the hook
    // itself, so `config.shell_commands` no longer carries them and this list
    // holds only what THIS runner adds.
    let mut shell_commands = config.shell_commands;

    // The QSRV runtime (interactive-shell) commands, bound to the SAME
    // `BridgeProvider` the served store wraps, so a post-iocInit
    // `processGroups` / `pvxgl` acts on the served groups rather than a
    // throwaway provider. `dbLoadGroup` is a startup command (pvxs only
    // permits it before iocInit), registered by `run_qsrv_ioc_app`.
    shell_commands.extend(super::iocsh::register_qsrv_runtime_commands_if_enabled(
        qsrv2_decision().enabled,
        store.provider().clone(),
    ));

    // ── CA server (background) ──
    let ca_serving = epics_ca_rs::server::RsrvServing::start(
        db.clone(),
        ca_port,
        config.tcp_port,
        config.acf.clone(),
        // Autosave is the PVA server's below: C has one save task per IOC.
        None,
        None,
        &mut config.servers,
    )
    .await?;

    // ── PVA server (foreground with iocsh) ──
    let pva_server = epics_pva_rs::server::PvaServer::from_parts(
        db,
        None,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    )
    .bound_at_registrar(&mut config.servers);

    let result = pva_server
        .run_with_source_and_shell(composite, move |shell| {
            for cmd in shell_commands {
                shell.register(cmd);
            }
        })
        .await
        .map_err(|e| CaError::InvalidValue(e.to_string()));
    ca_serving.shutdown();
    result
}

/// Build the served provider's group set from both pvxs group sources,
/// then finalize it — the QSRV equivalent of pvxs `processGroups()`
/// (ioc/groupsourcehooks.cpp:192-214), run here before the PVA server
/// accepts connections so the first client GET already sees finalized
/// group PVs.
///
/// Source order matches pvxs `GroupConfigProcessor`:
///
///  1. DB `info(Q:group, ...)` records (`loadConfigFromDb`) — every record
///     carrying the tag contributes its group definition.
///  2. Queued `dbLoadGroup` files (`loadConfigFiles`) — drained from the
///     base startup queue ([`epics_base_rs::server::ioc_app::take_group_load_requests`]) in st.cmd order,
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
/// field-keyed merge, matching pvxs `fieldConfigMap` accumulation. A bad
/// info tag warns and is skipped, an unreadable file prints pvxs's
/// `Error opening` line and is skipped; a group file that
/// does not parse aborts, as pvxs's throw out of its noexcept hook does.
async fn load_qsrv_groups(
    provider: &Arc<BridgeProvider>,
    db: &Arc<epics_base_rs::server::database::PvDatabase>,
    extra: &[epics_base_rs::server::ioc_app::GroupLoadRequest],
) {
    use epics_base_rs::server::ioc_app::take_group_load_requests;

    // 1. DB info(Q:group) records (pvxs loadConfigFromDb).
    for name in db.all_record_names() {
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
                tracing::debug!(file = %req.filename, "qsrv: dbLoadGroup loaded ({total} groups total)");
            }
            Err(super::iocsh::GroupFileError::Open(line)) => {
                epics_base_rs::stderr_println!("{line}")
            }
            Err(super::iocsh::GroupFileError::Parse(what)) => {
                // libstdc++'s verbose terminate handler framing pvxs's throw.
                epics_base_rs::stderr_println!(
                    "terminate called after throwing an instance of 'std::runtime_error'\n  what():  {what}"
                );
                std::process::abort();
            }
        }
    }

    // 3. Finalize (pvxs resolveTriggerReferences / createGroups).
    provider.process_groups().await;
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

    async fn mount() -> (Arc<epics_base_rs::server::database::PvDatabase>, QsrvMount) {
        use epics_base_rs::server::database::PvDatabase;
        let db = Arc::new(PvDatabase::new());
        db.add_pv("TEST:GATE", epics_base_rs::types::EpicsValue::Double(1.0))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        let mount = QsrvMount {
            store: Arc::new(QsrvPvStore::new(crate::test_reactor(), provider)),
            acf: epics_base_rs::server::access_security::new_acf_cell(None),
        };
        (db, mount)
    }

    /// What a QSRV IOC serves `provider`'s database as: `qsrvSingle` ahead of
    /// `qsrvGroup`.
    fn qsrv_composite(
        provider: Arc<BridgeProvider>,
    ) -> Arc<epics_pva_rs::server_native::composite::CompositeSource> {
        let db = provider.database().clone();
        let composite = epics_pva_rs::server_native::composite::CompositeSource::new();
        QsrvMount {
            store: Arc::new(QsrvPvStore::new(crate::test_reactor(), provider)),
            acf: epics_base_rs::server::access_security::new_acf_cell(None),
        }
        .install_channel_sources(&db, &composite)
        .unwrap();
        composite
    }

    /// The tokio runners' route: a record name is claimed at order 0 by
    /// `qsrvSingle`, which the composite consults first, and a group name
    /// only by `qsrvGroup` at order 1 (pvxs `singlesourcehooks.cpp:159`,
    /// `groupsourcehooks.cpp:219`).
    #[tokio::test]
    async fn channel_sources_route_a_record_to_qsrv_single() {
        use epics_pva_rs::server_native::ChannelSource;
        let (db, mount) = mount().await;
        let composite = epics_pva_rs::server_native::composite::CompositeSource::new();
        mount
            .install_channel_sources(&db, &composite)
            .expect("a fresh composite accepts both sources");
        assert_eq!(
            composite.list_source(),
            vec![("qsrvSingle".to_string(), 0), ("qsrvGroup".to_string(), 1)]
        );
        let single = composite.get_source("qsrvSingle", 0).unwrap();
        assert!(single.has_pv(b"TEST:GATE").await);
        assert!(composite.has_pv(b"TEST:GATE").await);
    }

    /// The store is pvxs's `qsrvGroup` and nothing more: a record is not a
    /// channel of it, so no channel answers GET/PUT/info yet cannot be
    /// monitored. softIocPVX serves the record from `qsrvSingle`.
    #[tokio::test]
    async fn a_direct_store_has_no_single_record_channel() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_pva_rs::server_native::ChannelSource;
        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "TEST:X",
            Box::new(epics_base_rs::server::records::ai::AiRecord::new(1.0)),
        )
        .await
        .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        provider
            .load_group_config(r#"{"G": {"x": {"+channel": "TEST:X"}}}"#)
            .unwrap();
        assert_eq!(provider.process_groups().await, 1);
        let store = QsrvPvStore::new(crate::test_reactor(), provider.clone());
        assert!(!store.has_pv(b"TEST:X").await);
        assert!(!store.searchable(b"TEST:X").await);
        assert!(store.get_value(b"TEST:X").await.is_none());
        assert!(store.get_introspection(b"TEST:X").await.is_none());
        assert!(store.subscribe(b"TEST:X").await.is_none());
        assert_eq!(store.list_pvs(), vec!["G".to_string()]);
        assert!(store.has_pv(b"G").await);

        let composite = qsrv_composite(provider);
        let single = composite.get_source("qsrvSingle", 0).unwrap();
        assert!(single.has_pv(b"TEST:X").await);
        assert!(composite.get_value(b"TEST:X").await.is_some());
        assert!(composite.subscribe(b"TEST:X").await.is_some());
    }

    /// pvxs answers the search for `AI.VAL$` (`dbChannelTest` resolves
    /// `AI.VAL`) and refuses the create; softIocPVX R7.0.10 prints
    /// `refuses channel to 'AI.VAL$' : Refused to create Channel`.
    #[tokio::test]
    async fn a_dollar_on_a_double_is_searched_and_refused_at_create() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::stringin::StringinRecord;
        use epics_pva_rs::server_native::ChannelSource;
        let db = Arc::new(PvDatabase::new());
        db.add_record("AI", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("SI", Box::new(StringinRecord::new("x")))
            .await
            .unwrap();
        let store = qsrv_composite(Arc::new(BridgeProvider::new(db)));
        assert!(store.searchable(b"AI.VAL$").await);
        assert!(!store.has_pv(b"AI.VAL$").await);
        assert!(store.searchable(b"SI.VAL$").await);
        assert!(store.has_pv(b"SI.VAL$").await);
        assert!(!store.searchable(b"AI.NOSUCH$").await);
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
        let store = qsrv_composite(provider);

        // Parameterless RPC: rejected (pvxs has no RPC-as-GET for records).
        let err = store
            .rpc(b"RPC:AO", FieldDesc::Variant, PvField::Null)
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
            .rpc(b"RPC:AO", request.descriptor(), request)
            .await
            .expect_err("RPC write-through on a QSRV record must be rejected");
        assert!(
            err.message == epics_pva_rs::server_native::source::RPC_NOT_IMPLEMENTED,
            "expected unsupported-RPC error, got: {err}"
        );

        // The record value is untouched: RPC never wrote.
        let value = store
            .get_value(b"RPC:AO")
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
        let store = QsrvPvStore::new(crate::test_reactor(), provider);

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
            .rpc(b"RPC:GRP", request.descriptor(), request)
            .await
            .expect_err("RPC on a QSRV group must be rejected");
        assert!(
            err.message == epics_pva_rs::server_native::source::RPC_NOT_IMPLEMENTED,
            "expected unsupported-RPC error, got: {err}"
        );

        // Members untouched: the rejected RPC wrote nothing.
        let value = store
            .get_value(b"RPC:GRP")
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

    /// softIocPVX 1.5.3 on `{"G2":{"a":{"+channel":"NOPE.VAL"}}}` prints
    /// `G2: Error Group not created`, still answers G2, and fails each op:
    ///
    /// ```text
    /// pvxinfo G2      Error: Implicit Cancel
    /// pvxget G2       Op Create implied error
    /// pvxput G2 a=1   RemoteError : Op Create implied error
    /// pvxmonitor G2   RemoteError : Monitor Create implied error
    /// ```
    #[cfg(all(feature = "qsrv", tokio_backend))]
    #[tokio::test]
    async fn an_uncreated_group_answers_and_fails_each_op_with_pvxs_text() {
        use std::time::Duration;

        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

        let db = Arc::new(PvDatabase::new());
        db.add_record("A", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db));
        provider
            .load_group_config(
                r#"{"G1":{"a":{"+channel":"A.VAL"}},"G2":{"a":{"+channel":"NOPE.VAL"}}}"#,
            )
            .unwrap();
        assert_eq!(provider.process_groups().await, 1);
        let store = Arc::new(QsrvPvStore::new(crate::test_reactor(), provider));
        let server =
            PvaServer::start(store, PvaServerConfig::isolated()).expect("test server must start");
        let client = server.client_config();
        let within = Duration::from_secs(5);

        let err = tokio::time::timeout(within, client.pvinfo("G2"))
            .await
            .expect("pvinfo G2 timed out")
            .expect_err("pvinfo G2");
        assert!(err.to_string().contains("Implicit Cancel"), "{err}");
        let err = tokio::time::timeout(within, client.pvget("G2"))
            .await
            .expect("pvget G2 timed out")
            .expect_err("pvget G2");
        assert!(err.to_string().contains("Op Create implied error"), "{err}");
        let err = tokio::time::timeout(within, client.pvput("G2", "1"))
            .await
            .expect("pvput G2 timed out")
            .expect_err("pvput G2");
        assert!(err.to_string().contains("Op Create implied error"), "{err}");
        let err = tokio::time::timeout(within, client.pvmonitor("G2", |_| {}))
            .await
            .expect("pvmonitor G2 timed out")
            .expect_err("pvmonitor G2");
        assert!(
            err.to_string().contains("Monitor Create implied error"),
            "{err}"
        );
        tokio::time::timeout(within, client.pvget("G1"))
            .await
            .expect("pvget G1 timed out")
            .expect("G1 was created");
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
        let store = qsrv_composite(provider);

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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
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

    /// A full subscriber cap on a record refuses its `qsrvSingle` monitor
    /// and every group monitor that has it as a member, with pvxs's line:
    /// subscriptionctx.h:48 throws out of `onSubscribe` and servermon.cpp:598
    /// logs it at ERR. The group fails whole, as groupsource.cpp's throw does,
    /// so its other member keeps no subscription.
    #[epics_base_rs::epics_test]
    async fn a_full_subscriber_cap_refuses_single_and_group_monitors_with_pvxss_line() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::server_native::ChannelSource;
        use epics_pva_rs::server_native::source::{AccessGate, ChannelContext};

        let db = Arc::new(PvDatabase::new());
        for name in ["CAP:a", "CAP:b"] {
            db.add_record(name, Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
        }
        let full = db.get_record("CAP:b").unwrap();
        let held: Vec<_> = (1..)
            .map_while(|sid| {
                full.write()
                    .add_subscriber("VAL", sid, epics_base_rs::types::DbFieldType::Double, 1)
                    .ok()
            })
            .collect();
        assert!(!held.is_empty());
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider
            .load_group_config(
                r#"{"CAP:GRP": {
                    "a": {"+channel": "CAP:a.VAL", "+type": "plain"},
                    "b": {"+channel": "CAP:b.VAL", "+type": "plain"}
                }}"#,
            )
            .expect("load group");
        provider.process_groups().await;
        let composite = epics_pva_rs::server_native::CompositeSource::new();
        QsrvMount {
            store: Arc::new(QsrvPvStore::new(crate::test_reactor(), provider)),
            acf: epics_base_rs::server::access_security::new_acf_cell(None),
        }
        .install_channel_sources(&db, &composite)
        .expect("qsrv sources mount");

        // The group's no-+trigger WARN may still be queued here; a listener added
        // before the worker drains it hears it, as in C errlog.
        epics_base_rs::runtime::log::errlog_flush();
        let heard = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = heard.clone();
        let listener = epics_base_rs::runtime::log::errlog_add_listener(move |m| {
            if let Some((_time, line)) = m.split_once(' ') {
                sink.lock().unwrap().push_str(line);
            }
        });
        let peer: std::net::SocketAddr = "127.0.0.1:5075".parse().unwrap();
        for name in ["CAP:b", "CAP:GRP"] {
            let checked = AccessGate::open()
                .check(name, "127.0.0.1", "anonymous", "anonymous", "")
                .await;
            let ctx = ChannelContext {
                peer,
                creds: std::sync::Arc::new(
                    epics_pva_rs::server_native::config::ClientCredentials {
                        account: "anonymous".into(),
                        method: "anonymous".into(),
                        host: "127.0.0.1".into(),
                        authority: String::new(),
                        roles: Vec::new(),
                    },
                ),
                pv_request: None,
                log: Default::default(),
            };
            let opened = composite
                .subscribe_checked_opts_marked(checked, ctx, Default::default())
                .await;
            assert!(opened.is_none(), "{name}: the cap refuses the monitor");
        }
        epics_base_rs::runtime::log::errlog_flush();
        epics_base_rs::runtime::log::errlog_remove_listener(listener);
        assert_eq!(
            *heard.lock().unwrap(),
            format!(
                "ERR pvxs.tcp.setup Client {peer} Monitor \"CAP:b\" onSubscribe() error: Failed to create db subscription\n\
                 ERR pvxs.tcp.setup Client {peer} Monitor \"CAP:GRP\" onSubscribe() error: Failed to create db subscription\n"
            )
        );
        // A new subscriber reaps closed rows first, so only it remains when
        // the group left nothing live behind.
        let other = db.get_record("CAP:a").unwrap();
        let _probe = other
            .write()
            .add_subscriber("VAL", 1, epics_base_rs::types::DbFieldType::Double, 1)
            .expect("under the cap");
        assert_eq!(
            other.read().subscriber_count("VAL"),
            1,
            "the failed group keeps no subscription on its other member"
        );
    }

    /// Regression: `get_value_checked` must forward the INIT
    /// pvRequest carried on `ChannelContext::pv_request` into
    /// `channel.get(...)`. Before the fix it always passed an empty
    /// `PvStructure::new("")`, so the request's `field` projection (and
    /// group `record._options`) were silently dropped: a GET asking
    /// only for member `a` still received every member.
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
        db.add_record("TEST:mrr13b", Box::new(AiRecord::new(2.5)))
            .await
            .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider
            .load_group_config(
                r#"{"G13": {"a": {"+channel": "TEST:mrr13"}, "b": {"+channel": "TEST:mrr13b"}}}"#,
            )
            .unwrap();
        assert_eq!(provider.process_groups().await, 1);
        let store = QsrvPvStore::new(crate::test_reactor(), provider);

        // INIT pvRequest selecting ONLY member `a`:
        //   field { a }
        let value_sel = PvStructure::new("");
        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("a".into(), PvField::Structure(value_sel)));
        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let checked = AccessGate::open()
            .check("G13", "127.0.0.1", "anonymous", "anonymous", "")
            .await;
        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
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
        // The field projection must have been honored: only `a`
        // survives. Before the fix the empty request returned every member.
        assert!(
            s.get_field("a").is_some(),
            "projected `a` member must be present"
        );
        assert_eq!(
            s.fields.len(),
            1,
            "pvRequest `field {{ a }}` must filter the GET to one member, got: {:?}",
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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "alice".into(),
                method: "ca".into(),
                host: "ws01".into(),
                authority: String::new(),
                roles: vec!["operators".into(), "experts".into()],
            }),
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

    /// QSRV forced PUT (`record._options.process=true`) must run the
    /// *link-aware* processing chain, not a value-only local
    /// notification: a record whose FLNK targets a second record must
    /// process that target before the operation replies success. pvxs
    /// routes it through `dbProcess` (iocsource.cpp:397-421), which runs
    /// FLNK / OUT links; the bare
    /// `process_record` (process_local + notify) would skip them.
    ///
    /// Discriminator: B is processed only when A's FLNK chain fires,
    /// which advances B's `common.time`. With the pre-fix local-only
    /// path B's TIME would never move.
    #[tokio::test]
    async fn forced_put_runs_the_flnk_link_chain() {
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
        db.put_record_field_from_ca_no_notify(
            "FLNK:a",
            "FLNK",
            EpicsValue::String("FLNK:b".into()),
        )
        .await
        .unwrap();
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        let store = qsrv_composite(provider);

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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
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
    }

    /// a native PVA group PUT must honor INIT pvRequest
    /// options. pvxs reads `record._options` from the INIT
    /// `channelConnectOperation->pvRequest()`
    /// (`groupsource.cpp:202-204`, `:207` `setForceProcessingFlag`),
    /// not from the data-phase
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
        let store = QsrvPvStore::new(crate::test_reactor(), provider);

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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
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
    /// 564-571, iocsource.cpp:397-421). Before the fix the group apply
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
                creds: std::sync::Arc::new(
                    epics_pva_rs::server_native::config::ClientCredentials {
                        account: "anonymous".into(),
                        method: "anonymous".into(),
                        host: "127.0.0.1".into(),
                        authority: String::new(),
                        roles: Vec::new(),
                    },
                ),
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
                EpicsValue::Enum(ScanType::SEC1.to_u16()),
            )
            .await
            .unwrap();
        }
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load group");
        let store = QsrvPvStore::new(crate::test_reactor(), provider);

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
        let store = QsrvPvStore::new(crate::test_reactor(), provider);

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
            ]
            .into(),
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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        };
        let checked = AccessGate::open()
            .check("BR120:grp", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        store
            .put_delta_checked(
                checked,
                std::sync::Arc::new(desc),
                changed,
                &PvField::Structure(delta),
                ctx,
            )
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
        let store = QsrvPvStore::new(crate::test_reactor(), provider);

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
            ]
            .into(),
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
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        };
        let checked = AccessGate::open()
            .check("BR120E:grp", "127.0.0.1", "anonymous", "anonymous", "")
            .await;

        store
            .put_delta_checked(
                checked,
                std::sync::Arc::new(desc),
                changed,
                &PvField::Structure(delta),
                ctx,
            )
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
        let tmpdir = tempfile::tempdir().expect("fixture root");
        let path = tmpdir.path().join("qsrv_e2e_dbloadgroup.json");
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
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        // `&[]`: this case drives the base startup queue (the real
        // `dbLoadGroup` command ran above), which is the host route. The
        // caller-supplied list is the target's route and is exercised by
        // `realtime-pva-ioc`'s own tests.
        load_qsrv_groups(&provider, &db, &[]).await;
        let store = Arc::new(QsrvPvStore::new(crate::test_reactor(), provider));

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
