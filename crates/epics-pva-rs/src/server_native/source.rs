//! [`ChannelSource`] — the trait every native PVA server is generic over.
//!
//! Replaces the spvirit `PvStore` trait. Uses our own [`crate::pvdata`]
//! types, so no `spvirit_*` types appear in the public surface.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::pvdata::{FieldDesc, PvField};
pub use epics_base_rs::server::access_security::{AccessChecked, AccessGate};

/// Per-operation context surfaced to [`ChannelSource`] implementors
/// that need the downstream peer's identity (audit, ACL, gateway
/// credential pass-through). Fields mirror what `CONNECTION_VALIDATION`
/// established at handshake time, plus the peer's TCP socket address.
///
/// PG-G10: gateways use this to pick the correct upstream client
/// when the gateway maintains a per-credential connection pool to
/// the upstream IOC. Default trait methods that don't take a context
/// remain available so existing implementations are unaffected.
#[derive(Clone, Debug)]
pub struct ChannelContext {
    /// Downstream client TCP socket address.
    pub peer: SocketAddr,
    /// Account name. For `ca`/`anonymous` this comes from
    /// CONNECTION_VALIDATION; for `x509` it is the verified peer
    /// leaf-certificate subject CommonName.
    pub account: String,
    /// Auth method (`"anonymous"`, `"ca"`, `"x509"`).
    pub method: String,
    /// Reverse-resolved host name. Empty when DNS lookup failed.
    pub host: String,
    /// Certificate authority for the `x509` method: the root CA's
    /// subject CommonName. Empty for non-TLS methods. ACF
    /// `AUTHORITY(...)` rule scopes match against this.
    pub authority: String,
    /// Group / role claims advertised by the downstream peer's auth
    /// method. parsed off the `ca` auth payload's
    /// `groups`/`roles` array into `ClientCredentials::roles`, then
    /// forwarded here so role-based ACF rules (`R member group:ops`,
    /// `role/...` credential strings) can be enforced for native PVA
    /// clients. Empty for methods that carry no role list.
    pub roles: Vec<String>,
    /// Decoded INIT pvRequest value for the current operation, when
    /// the wire layer captured one. PVA PUT INIT carries
    /// `record._options.process`/`block`; the data-phase payload is
    /// just the delta, so sources that interpret per-operation
    /// options must consult this rather than the value.
    ///
    /// `None` for op kinds where no pvRequest was captured (RPC INIT
    /// today, GET/MONITOR where the request was consumed for masking)
    /// or when the wire decoder could not parse it. Sources that
    /// don't need per-op options can ignore the field.
    pub pv_request: Option<PvField>,
}

/// Event-affecting options decoded from a downstream MONITOR INIT
/// pvRequest, surfaced to [`ChannelSource`] implementors that need to
/// reason about whether they can honor them.
///
/// a PVA-to-PVA gateway fans one upstream monitor out to N
/// downstream subscribers. The upstream monitor is opened with the
/// gateway's *default* pvRequest, so a downstream option that changes
/// *upstream event production* is not transparent through the fanout:
/// the events the gateway has on the shared stream are not the events
/// a direct upstream monitor would produce for that option. The only
/// such option is a server-side `record._options._filter` chain (a
/// stateful `dbnd` deadband / `arr` slice run per subscription) —
/// [`Self::server_filter`]. A source that cannot honor it must be able
/// to see it and reject the subscription rather than silently serving
/// fanout events that differ from a direct upstream monitor.
///
/// The pvxs pipeline flow-control options are deliberately NOT in this
/// "affects upstream" set, because each is pure downstream credit/ACK/
/// buffer flow control between the client and *its* server, terminated
/// locally by the gateway and transparent through the fanout:
/// `pipeline` (`servermon.cpp:523`, sets `op->pipeline`), `queueSize`
/// (`:533`, sets `op->limit`), and `ackAny` (`:546-582`, parsed only
/// inside `if(op->pipeline)`, sets the ACK-refill threshold `op->ackAt`
/// that feeds the window watermarks at `:332`). None of them change
/// which events the source produces, so none belongs here — flagging
/// them would make every pipelined PVA client unable to monitor
/// through the gateway. `ackAny` therefore has no field: it is
/// transparent like `pipeline`/`queueSize`, not an upstream-event
/// option the gateway must detect.
///
/// Field projection (the pvRequest field mask) is intentionally NOT
/// represented here: it is pure downstream-local masking the server
/// applies after fanout, and is transparent through a gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorOptions {
    /// `record._options.pipeline` — the client requested the
    /// flow-controlled credit/ACK monitor sub-protocol. This is flow
    /// control between the client and *its* server, not a change to
    /// which events are produced: a fanout gateway terminates the
    /// downstream pipeline on its own downstream connection and
    /// propagates backpressure upstream via the per-PV `Pauser`
    /// (PG-G9). It therefore does NOT make a monitor non-transparent.
    pub pipeline: bool,
    /// `record._options.queueSize` when the client set it explicitly
    /// (pvxs default 4). `None` means the client did not request a
    /// specific queue depth. Like `pipeline`, this is a downstream
    /// buffer-depth request the gateway honors on its per-downstream
    /// outbox; it does not change upstream event production.
    pub queue_size: Option<u32>,
    /// True when the downstream pvRequest carried a server-side
    /// `record._options._filter` chain. A stateful filter (e.g.
    /// deadband) changes which events are *produced*; running it at a
    /// fanout gateway on a shared upstream stream is not equivalent to
    /// the upstream server running it per subscription.
    pub server_filter: bool,
}

impl MonitorOptions {
    /// True when an option here changes *upstream event production*
    /// and therefore cannot be honored transparently by a fanout
    /// gateway that shares one upstream monitor across downstreams.
    ///
    /// Only a server-side `_filter` chain qualifies. `pipeline`,
    /// `queueSize`, and `ackAny` are downstream client↔gateway flow
    /// control the gateway terminates locally (see the struct docs), so
    /// they are transparent and must NOT trigger a fanout-gateway
    /// rejection — rejecting them would make every default-configured
    /// PVA client (which enables pipeline by default) unable to monitor
    /// through the gateway.
    pub fn affects_upstream_events(&self) -> bool {
        self.server_filter
    }
}

/// which pipeline-window watermark transition a downstream
/// monitor op just made. A gateway fans ONE upstream monitor out to N
/// downstream subscribers of the same PV+credential and must
/// reference-count their pause votes — pausing the shared upstream only
/// when *every* live downstream op wants pause, resuming as soon as any
/// has room — so a single op's transition is not enough; the op
/// identity and its disposition are both required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatermarkKind {
    /// The op's window drained to `<= low`: it wants the upstream paused.
    Pause,
    /// The op's window refilled above `high`: it no longer needs a pause.
    Resume,
    /// The op's subscriber task ended (DESTROY / disconnect / completion):
    /// withdraw its vote entirely so a torn-down op cannot strand the
    /// shared upstream paused for its co-subscribers. Terminal — not
    /// ordered by `seq`.
    Withdraw,
}

/// a downstream monitor op's pipeline-window watermark
/// transition, carrying the op identity + ordering token a gateway needs
/// to compose pause votes across co-subscribers of one shared upstream
/// entry. See [`ChannelSource::notify_watermark`].
#[derive(Debug, Clone, Copy)]
pub struct WatermarkEvent {
    /// Process-unique downstream monitor op id (one per subscriber task).
    /// The aggregation key: distinct ops voting on one shared upstream.
    pub op_id: u64,
    /// Strictly-monotonic per-op ordering token minted in the SAME atomic
    /// transition that decided this crossing (see `tcp.rs`
    /// `cross_watermark`). Lets the consumer discard a [`WatermarkKind`]
    /// re-ordered behind a newer one *for the same op* (the LOW fires
    /// from the subscriber emission task, the HIGH from the ACK-dispatch
    /// task, so they can arrive out of order). `0` for
    /// [`WatermarkKind::Withdraw`], which is terminal and not seq-gated.
    pub seq: u64,
    pub kind: WatermarkKind,
}

/// A backend that can answer pvAccess GET / PUT / MONITOR requests for a
/// set of named PVs.
// no on_channel_close hook — pvxs serverchan.cpp:57-59 fires onClose("") per channel;
// this trait has no equivalent. Doc-only; fix requires a semver-minor breaking API change.
pub trait ChannelSource: Send + Sync + 'static {
    /// Per-source access policy. Returns the [`AccessGate`] used by
    /// the wire layer to mint [`AccessChecked`] tokens for the typed
    /// op methods (`*_checked` family). Default impl returns a
    /// process-wide singleton `Open` gate — sources that need ACF
    /// enforcement override to install a `Required` gate wrapping
    /// their `AcfCell`.
    ///
    /// Round 40 (type-state ACF gate): the typed op methods take
    /// `AccessChecked` instead of `name: &str + ctx`. Because
    /// `AccessChecked` is unforgeable outside this gate's `check`,
    /// every wire op MUST flow through it — closing the missed-path
    /// pattern that surfaced across rounds 32-39.
    fn access(&self) -> &AccessGate {
        static OPEN_GATE: std::sync::OnceLock<AccessGate> = std::sync::OnceLock::new();
        OPEN_GATE.get_or_init(AccessGate::open)
    }

    /// Enumerate every PV name this source can serve.
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send;

    /// True iff `name` resolves to a known PV.
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// True iff `name` should be answered to a UDP SEARCH broadcast.
    ///
    /// Distinct from [`Self::has_pv`]: a name may be reachable via a
    /// direct TCP connect (`has_pv` true) yet deliberately NOT be
    /// advertised on UDP discovery (`searchable` false). pvxs's
    /// built-in `ServerSource` does exactly this — `onSearch` is empty
    /// so the `server` PV resolves only by direct connect, never by
    /// broadcast SEARCH (`serversource.cpp`). The default impl
    /// delegates to `has_pv`, so ordinary sources are unaffected; the
    /// built-in [`crate::server_native::ServerInfoSource`] overrides
    /// this to return `false`.
    fn searchable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        self.has_pv(name)
    }

    /// Fetch the type descriptor for a PV (used by GET-INIT and GET_FIELD).
    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send;

    /// Credential-aware [`Self::has_pv`], called at CREATE_CHANNEL time
    /// with the downstream connection's [`ChannelContext`].
    ///
    /// a gateway must resolve a credentialed downstream
    /// channel's existence against that peer's own upstream identity, not
    /// the shared gateway identity — otherwise the upstream cache/monitor
    /// is opened under the wrong audit identity as a side effect of
    /// channel setup. pvxs constructs `ServerChannelControl` with
    /// `conn->cred` (`serverchan.cpp:62`) for exactly this reason. The
    /// default delegates to the credential-free [`Self::has_pv`], so
    /// non-gateway sources (which ignore credentials) are unaffected.
    fn has_pv_checked(
        &self,
        name: &str,
        _ctx: ChannelContext,
    ) -> impl std::future::Future<Output = bool> + Send {
        self.has_pv(name)
    }

    /// Credential-aware [`Self::get_introspection`], called at
    /// CREATE_CHANNEL and GET_FIELD time with the downstream connection's
    /// [`ChannelContext`].
    ///
    /// descriptor discovery for a credentialed downstream
    /// peer must open/refresh upstream state under that peer's identity,
    /// not the shared gateway identity (pvxs builds the GET_FIELD
    /// `ConnectOp` with `conn->cred`, `serverintrospect.cpp:66`). The
    /// default delegates to the credential-free
    /// [`Self::get_introspection`].
    fn get_introspection_checked(
        &self,
        name: &str,
        _ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        self.get_introspection(name)
    }

    /// Fetch the current value of a PV.
    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send;

    /// Re-check READ access for
    /// `(pv_name, ctx)` through the SOURCE-SPECIFIC ACL gate that
    /// served the original subscription. Returns `Some(token)` on
    /// allow, `None` on deny.
    ///
    /// Invariant (closed by this method): every monitor event
    /// after an ACL version mismatch MUST re-check READ through
    /// the same gate that originally produced its `AccessChecked`.
    /// For terminal sources (`PvDatabaseSource`,
    /// `GatewayChannelSource`) `self.access()` IS that gate, so the
    /// default impl below is correct. For `CompositeSource` the
    /// top-level `access()` is an `open_with_aggregator` —
    /// permissive on every call — and the override MUST resolve
    /// the matched inner source and route the check through THAT
    /// source's gate. Without the override, a monitor whose
    /// subscribe-time inner-gate said allow would re-check against
    /// the composite's Open gate on reload and keep streaming
    /// after the inner flipped to deny.
    fn revalidate_read(
        &self,
        pv_name: &str,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<AccessChecked>> + Send {
        let gate = self.access();
        let host = ctx.host.clone();
        let account = ctx.account.clone();
        let method = ctx.method.clone();
        let authority = ctx.authority.clone();
        // forward the peer's role claims so `role/<name>` UAG
        // members can match.
        let roles = ctx.roles.clone();
        let name = pv_name.to_string();
        async move {
            let checked = gate
                .check_with_roles(&name, &host, &account, &roles, &method, &authority)
                .await;
            if checked.allows_read() {
                Some(checked)
            } else {
                None
            }
        }
    }

    /// Type-state-enforced GET. The wire layer mints `checked` via
    /// `self.access().check(...)` once per op; the source then
    /// inspects `checked.allows_read()` and dispatches. Round 43
    /// deleted the `get_value_ctx` legacy path — every credential-
    /// aware GET now flows through this method and the AccessGate.
    /// The ctx is still passed so gateway-style sources can route
    /// to per-credential upstream connection pools.
    fn get_value_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let _ = ctx;
        async move {
            if !checked.allows_read() {
                return None;
            }
            self.get_value(checked.pv_name()).await
        }
    }

    /// Apply a PUT.
    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// Type-state-enforced PUT. Refuses non-`ReadWrite` tokens; on
    /// `ReadWrite` it delegates to the legacy ctx-less `put_value`.
    /// Sources that need credential-aware PUT routing (e.g. gateway
    /// per-credential upstream client pool) override this directly
    /// and consume `ctx` themselves.
    fn put_value_checked(
        &self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(format!(
                    "PUT denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                ));
            }
            self.put_value(checked.pv_name(), value).await
        }
    }

    /// Type-state-enforced **BitSet-delta PUT**.
    ///
    /// PVA PUT/PUT_GET data frames carry only the changed fields plus
    /// a changed-BitSet. Applying the delta is a read-merge-write:
    /// read the PV's current complete value, overlay the marked
    /// fields, store the result. The default impl below does that as
    /// `get_value` + `fill_unmarked_from_prior` + `put_value_checked`,
    /// which is correct for a single client but has a TOCTOU
    /// lost-update window under concurrent partial PUTs to the same
    /// PV (two writers read the same prior; the second write drops
    /// the first's disjoint fields).
    ///
    /// The default impl forwards to [`Self::put_value_checked`] (not
    /// the ctx-less `put_value`) so credential-aware sources — the
    /// pva-gateway routes PUTs through a per-`(account, method)`
    /// upstream client — keep their identity propagation. The
    /// `put_value_checked` call performs the `allows_write()` gate.
    ///
    /// Sources whose backing store can merge under a single lock
    /// override this to close the TOCTOU window —
    /// [`crate::server_native::shared_pv::SharedSource`] forwards to
    /// [`crate::server_native::shared_pv::SharedPV::put_delta`], which
    /// reads + merges + stores under one mutex acquisition.
    ///
    /// `desc` is the PV introspection (per-field bit numbering);
    /// `changed` is the wire changed-BitSet; `delta` is the decoded
    /// sparse value (unmarked leaves hold type defaults).
    fn put_delta_checked(
        &self,
        checked: AccessChecked,
        desc: FieldDesc,
        changed: crate::proto::BitSet,
        delta: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move {
            // Default (non-atomic) merge for sources without a
            // contained merge primitive. Single-client correct.
            //
            // the prior-value read MUST run under the same
            // authenticated identity as the write. Reading the prior
            // through the ctx-less `get_value` would let an
            // access-controlled or credential-routed source resolve
            // the prior under an anonymous/default context — a denied
            // or differently-resolved read then collapses to
            // `None => delta`, treating the sparse data-phase value as
            // a full value and replacing unmarked leaves with type
            // defaults. Route the prior read through
            // `get_value_checked` with a clone of the same `checked`
            // token and `ctx`, so credential-aware sources merge under
            // their own identity. `put_value_checked` below still
            // enforces the WRITE gate.
            let merged = match self.get_value_checked(checked.clone(), ctx.clone()).await {
                Some(prior) => crate::pvdata::encode::fill_unmarked_from_prior(
                    &desc, &changed, 0, delta, &prior,
                ),
                None => delta,
            };
            self.put_value_checked(checked, merged, ctx).await
        }
    }

    /// True iff PUT is allowed against this PV (for ACL gating).
    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// Subscribe to value-change notifications. Returns `None` if unknown.
    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send;

    /// Type-state-enforced MONITOR. Refuses `NoAccess` tokens; on
    /// any READ-class level delegates to the legacy ctx-less
    /// `subscribe`. Sources that need credential-aware MONITOR
    /// routing override this directly.
    fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let _ = ctx;
        async move {
            if !checked.allows_read() {
                return None;
            }
            self.subscribe(checked.pv_name()).await
        }
    }

    /// Optional **raw-frame subscribe**. When the source can
    /// hand the server pre-encoded MONITOR DATA payloads (e.g. the
    /// pva_gateway upstream-monitor task already received them on the
    /// wire and never decoded them), the server skips its own
    /// `encode_pv_field` step and writes the cached bytes straight
    /// onto the downstream socket — pvxs / pva2pva style raw frame
    /// forwarding. Default returns `None`, which keeps the regular
    /// `subscribe` decoded-PvField path active.
    ///
    /// Each [`RawMonitorEvent`] holds the **changed bitset + value
    /// bytes + overrun bitset** verbatim from upstream. The dispatch
    /// layer prepends the per-subscription PVA header (with the
    /// downstream IOID + subcmd 0) and emits.
    fn subscribe_raw(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let _ = name;
        async { None }
    }

    /// Type-state-enforced raw MONITOR fast path. NoAccess → None;
    /// otherwise delegates to ctx-less `subscribe_raw`.
    fn subscribe_raw_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let _ = ctx;
        async move {
            if !checked.allows_read() {
                return None;
            }
            self.subscribe_raw(checked.pv_name()).await
        }
    }

    /// MONITOR with the downstream's event-affecting pvRequest
    /// options, decoded-`PvField` form. The default impl ignores `opts`
    /// and delegates to [`Self::subscribe_checked`] — correct for any
    /// source that owns the record directly, since it applies pipeline /
    /// filter semantics itself on the same stream.
    ///
    /// A fanout source (the PVA gateway) overrides this to reject a
    /// subscription whose options cannot be honored transparently
    /// across a shared upstream monitor, instead of silently serving
    /// fanout events that diverge from a direct upstream monitor.
    ///
    /// The server's monitor dispatch uses the cooked
    /// [`Self::subscribe_checked_opts_marked`] variant (which carries the
    /// trigger `marked` bitset); this `PvField`-returning method is the
    /// stable entry point retained for API compatibility and for callers
    /// that do not need the marked metadata.
    fn subscribe_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let _ = opts;
        self.subscribe_checked(checked, ctx)
    }

    /// MONITOR with event-affecting pvRequest options,
    /// **cooked** form — the stream carries [`MonitorUpdate`] so a
    /// `+trigger` graph can mark which members changed. The default impl
    /// delegates to [`Self::subscribe_checked_opts`] and wraps each value
    /// with `marked: None`, so the server derives the changed-bitset as
    /// before (full mask / value-diff). The server's monitor dispatch
    /// calls this method; a fanout source (the PVA gateway) overrides it
    /// to apply trigger selection over a shared upstream monitor.
    fn subscribe_checked_opts_marked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<MonitorUpdate>>> + Send {
        async move {
            self.subscribe_checked_opts(checked, ctx, opts)
                .await
                .map(plain_monitor_updates)
        }
    }

    /// Raw-path counterpart of [`Self::subscribe_checked_opts`].
    /// Default impl ignores `opts` and delegates to
    /// [`Self::subscribe_raw_checked`].
    fn subscribe_raw_checked_opts(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send {
        let _ = opts;
        self.subscribe_raw_checked(checked, ctx)
    }

    /// Dispatch an RPC. The default impl returns "RPC not supported";
    /// implementors can override to provide actual RPC behaviour.
    ///
    /// Returns the response (FieldDesc, PvField) on success.
    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let _ = (name, request_desc, request_value);
        async move { Err("RPC not supported by this source".to_string()) }
    }

    /// Type-state-enforced RPC. pvxs treats RPC as READ-class for
    /// ACF; refuse `NoAccess` tokens with an error and otherwise
    /// delegate to the legacy ctx-less `rpc`.
    fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        async move {
            if !checked.allows_read() {
                return Err(format!(
                    "RPC denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                ));
            }
            self.rpc(checked.pv_name(), request_desc, request_value)
                .await
        }
    }

    /// Trigger record/PV **processing** without transferring a value
    /// (PVA wire command `PROCESS`, cmd 16). Unlike PUT-with-
    /// `record[process=true]`, this carries no value payload — it is
    /// the wire equivalent of an EPICS `dbProcess` / `caput .PROC`.
    ///
    /// Default impl returns `Ok(())` — sources whose PVs have no
    /// processing semantics (constant / mailbox PVs) treat PROCESS as
    /// a no-op success, matching how a passive record handles a `.PROC`
    /// write. Sources backed by a processable record (IOC database,
    /// `SharedPV` with an `on_process` hook) override to actually run
    /// the processing chain. An `Err` surfaces to the client as a
    /// PROCESS error status.
    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let _ = name;
        async move { Ok(()) }
    }

    /// Type-state-enforced PROCESS. pvxs treats `process()` as a
    /// WRITE-class operation for ACF (it mutates record state), so a
    /// non-`ReadWrite` token is refused with an error; on `ReadWrite`
    /// it delegates to the ctx-less [`Self::process`]. Sources that
    /// need credential-aware routing override this directly.
    fn process_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(format!(
                    "PROCESS denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                ));
            }
            self.process(checked.pv_name()).await
        }
    }

    /// does this source emit *partial* monitor updates for
    /// `name` — i.e. each event changes only a subset of the
    /// structure's leaves, not the whole value?
    ///
    /// pvxs posts a monitor `Value` whose own marked-changed bitset
    /// reflects exactly the leaves touched since the last `unmark()`
    /// (`servermon.cpp:174` `to_wire_valid(R, ent, &self->pvMask)`
    /// intersects that with the request mask). A QSRV *group* monitor
    /// with the default `+trigger` (self-trigger) re-reads only the
    /// triggered member on each event, so only that member's leaves
    /// change — the wire changed-bitset must be narrowed accordingly
    /// rather than always marking the whole request mask.
    ///
    /// When this returns `true` the server's decoded monitor loop
    /// derives the per-event changed-bitset by structurally diffing
    /// consecutive snapshots (intersected with the request mask),
    /// matching pvxs's marked-leaf semantics. Default `false` keeps
    /// the static-mask behaviour for single-record sources whose
    /// every event already carries a full value.
    fn monitor_emits_partial(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// Notify the source that the per-connection monitor outbox for
    /// `name` just crossed UP through its high watermark. Producers
    /// can throttle their post() rate in response. Default impl is
    /// a no-op; [`crate::server_native::shared_pv::SharedSource`]
    /// overrides to fire the per-PV `on_high_mark` callback.
    /// Mirrors pvxs `MonitorControlOp::onHighMark`.
    ///
    /// a downstream monitor op crossed a pipeline-window
    /// watermark (`ev.kind`: [`WatermarkKind::Pause`] on the LOW edge,
    /// [`WatermarkKind::Resume`] on the HIGH edge) or its subscriber task
    /// ended ([`WatermarkKind::Withdraw`]). Default no-op;
    /// [`crate::server_native::shared_pv::SharedSource`] overrides to fire
    /// its per-PV `on_high_mark`/`on_low_mark` callbacks. Mirrors pvxs
    /// `MonitorControlOp::onHighMark`/`onLowMark`, plus a teardown signal
    /// the gateway needs.
    ///
    /// `ctx` is the firing downstream subscription's credential context: a
    /// gateway routes per-credential upstreams into separate caches, so it
    /// must scope the resulting upstream resume/pause to the layer this
    /// subscription's upstream lives in rather than every layer.
    /// `ev.op_id` + `ev.seq` let a fanout gateway reference-count pause
    /// votes across the N downstream ops sharing one upstream entry (pause
    /// only when every live op wants pause; a `Withdraw` removes a
    /// torn-down op's vote) and order each op's transitions correctly even
    /// though its LOW and HIGH fire from different tasks.
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        let _ = (name, ctx, ev);
    }

    /// a downstream MONITOR op crossed the Executing<->Idle
    /// boundary. `start == true` when the op begins or resumes producing
    /// (MONITOR START / RESUME); `start == false` when it stops (MONITOR
    /// PAUSE / CANCEL_REQUEST / DESTROY / disconnect). Mirrors pvxs
    /// `MonitorControlOp::onStart(std::function<void(bool)>)`
    /// (`source.h:130`, `servermon.cpp:677-683`).
    ///
    /// A source uses this to gate work that only matters while a client
    /// is actually consuming: a gateway suspends its single upstream
    /// subscription when every downstream op pauses and resumes it on the
    /// first restart; a hardware/poller source stops sampling. The wire
    /// layer fires it exactly once per edge through one
    /// `MonitorStartControl` per op (see `tcp.rs`), so implementors never
    /// see a duplicate `true`/`false` or a stop without a prior start.
    /// `ctx` is credential-scoped (no `pv_request`) like
    /// [`Self::notify_watermark`] so a fanout gateway can scope the
    /// suspend/resume to the firing credential's upstream cache layer.
    /// Default impl ignores it.
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool) {
        let _ = (name, ctx, start);
    }

    /// the per-PV pipeline-window watermark levels `(low,
    /// high)` for `name`, in window-credit units. The monitor loop fires
    /// [`Self::notify_watermark`] with [`WatermarkKind::Resume`] when an
    /// ACK refills the window above `high` and [`WatermarkKind::Pause`]
    /// when a DATA emission drains it to `<= low` — pvxs `servermon.cpp`
    /// flow-control semantics, not server-queue occupancy. Default
    /// `None` (no per-PV levels);
    /// [`crate::server_native::shared_pv::SharedSource`] overrides to
    /// return its `SharedPV` levels.
    ///
    /// Async because a [`crate::server_native::CompositeSource`] must
    /// resolve the source that actually serves `name` (via `has_pv`, the
    /// same single-owner resolution every other op uses) before reading
    /// its levels — a catch-all source that returns levels for every
    /// name (the PVA gateway) must not preempt the name-scoped owner.
    fn monitor_watermarks(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<(usize, usize)>> + Send {
        let _ = name;
        async { None }
    }
}

/// One MONITOR DATA event in **raw wire form** — the bytes the
/// upstream server emitted, ready to be re-emitted downstream after
/// the per-subscription PVA header has been prepended. Used by
/// [`ChannelSource::subscribe_raw`] to skip the server-side
/// `encode_pv_field` round-trip.
///
/// `body_bytes` is the **`changed bitset | value bytes | overrun
/// bitset`** triplet exactly as it sat on the upstream wire (after
/// the upstream server's IOID + subcmd, which we discard and
/// replace with the downstream IOID + subcmd 0).
///
/// `byte_order` records what byte order the producer encoded with,
/// so the dispatch layer can refuse the fast path when the
/// downstream connection negotiated the opposite endian (rare, but
/// matters for cross-host gateways). On mismatch, dispatch falls
/// back to the decoded `subscribe` path.
/// a cooked MONITOR update — the new value plus an
/// optional explicit set of changed field paths.
///
/// `marked == None` means the server derives the wire changed-bitset
/// itself: the full request mask for an ordinary source, or a
/// value-diff for a partial-emitting source ([`ChannelSource::monitor_emits_partial`]).
///
/// `marked == Some(paths)` carries an *explicit* marked-leaf set: the
/// dot-separated field paths the source declares changed for this
/// event, marked whether or not their value differs from the previous
/// snapshot. A QSRV group monitor uses this to honor `+trigger` target
/// graphs (pvxs `groupsource.cpp:288` marks each trigger target
/// assigned-not-changed); the encoder turns the paths into a wire
/// changed-bitset via [`crate::pvdata::encode::marked_changed_bitset`].
#[derive(Debug, Clone)]
pub struct MonitorUpdate {
    /// The full snapshot value for this event.
    pub value: PvField,
    /// Explicit changed field paths, or `None` to let the server
    /// derive the changed-bitset.
    pub marked: Option<Vec<String>>,
}

impl From<PvField> for MonitorUpdate {
    /// A plain value with no explicit marked set — the server derives
    /// the changed-bitset (full mask or value-diff) as before.
    fn from(value: PvField) -> Self {
        Self {
            value,
            marked: None,
        }
    }
}

/// Adapt a plain `PvField` monitor stream into a [`MonitorUpdate`]
/// stream that carries no explicit marked set (`marked: None`). Used by
/// every cooked source without a `+trigger` graph so the
/// [`ChannelSource::subscribe_checked_opts_marked`] item type is uniform
/// while the source keeps producing bare `PvField`s on its own channel.
pub fn plain_monitor_updates(mut rx: mpsc::Receiver<PvField>) -> mpsc::Receiver<MonitorUpdate> {
    let (tx, out) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            if tx.send(MonitorUpdate::from(v)).await.is_err() {
                break;
            }
        }
    });
    out
}

#[derive(Debug, Clone)]
pub struct RawMonitorEvent {
    /// Body of the MONITOR DATA frame: `changed | value | overrun`.
    /// Refcounted via `bytes::Bytes` so fan-out across N
    /// downstream subscribers is N atomic increments, no copies.
    pub body_bytes: bytes::Bytes,
    /// Byte order the producer encoded with. `Little` is the
    /// default for both pva-rs and pvxs; only relevant when the
    /// server's downstream connection negotiated `Big`.
    pub byte_order: crate::proto::ByteOrder,
    /// when `true`, this event signals an upstream
    /// descriptor change. `body_bytes` is meaningless (and may be
    /// empty); the downstream wire layer must NOT forward it under
    /// the original MONITOR INIT descriptor. The downstream
    /// dispatch path emits `MONITOR FINISH` instead so the client
    /// can reopen with the new descriptor. pvxs treats reconnect /
    /// type-change as a subscription boundary (pvalink_channel.cpp:
    /// 342-351 `onTypeChange()`); the gateway mirrors that here.
    pub type_changed: bool,
}

/// Type-erased handle so the server runtime can hold heterogeneous sources
/// without monomorphising every async path. Most callers pass an
/// `Arc<MySource>` directly; this is mainly for the runtime internals.
pub type DynSource = Arc<dyn ChannelSourceObj>;

/// Object-safe variant of [`ChannelSource`]. Auto-implemented via blanket
/// for any `T: ChannelSource`.
pub trait ChannelSourceObj: Send + Sync {
    fn list_pvs<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>>;
    fn has_pv<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    /// Dyn forwarder for [`ChannelSource::searchable`].
    fn searchable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn get_introspection<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FieldDesc>> + Send + 'a>>;
    /// dyn forwarder for credential-aware existence.
    fn has_pv_checked<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    /// dyn forwarder for credential-aware introspection.
    fn get_introspection_checked<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FieldDesc>> + Send + 'a>>;
    fn get_value<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    /// Round 41: type-state-gated GET. Dyn forwarder.
    fn get_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    /// Round 41: per-source access gate. Dyn forwarder.
    fn access_gate(&self) -> &AccessGate;
    /// Round 50 follow-up: monitor reload revalidation owner.
    fn revalidate_read<'a>(
        &'a self,
        pv_name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<AccessChecked>> + Send + 'a>>;
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    /// Round 42: dyn forwarder for type-state PUT.
    fn put_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    /// Dyn forwarder for type-state atomic BitSet-delta PUT.
    fn put_delta_checked<'a>(
        &'a self,
        checked: AccessChecked,
        desc: FieldDesc,
        changed: crate::proto::BitSet,
        delta: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    fn is_writable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    >;
    /// Round 42: dyn forwarder for type-state MONITOR.
    fn subscribe_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    >;
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    >;
    /// Round 42: dyn forwarder for type-state raw MONITOR.
    fn subscribe_raw_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    >;
    /// dyn forwarder for MONITOR with event-affecting options
    /// (decoded `PvField` form; the stable entry point retained for API
    /// compatibility).
    fn subscribe_checked_opts<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    >;
    /// dyn forwarder for the cooked (`MonitorUpdate`) MONITOR
    /// with event-affecting options. The server's monitor dispatch uses this.
    fn subscribe_checked_opts_marked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<MonitorUpdate>>> + Send + 'a>,
    >;
    /// dyn forwarder for raw MONITOR with event-affecting options.
    fn subscribe_raw_checked_opts<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    >;
    fn rpc<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    >;
    /// Round 42: dyn forwarder for type-state RPC.
    fn rpc_checked<'a>(
        &'a self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    >;
    fn process<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    /// Dyn forwarder for type-state PROCESS.
    fn process_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
    fn monitor_emits_partial(&self, name: &str) -> bool;
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent);
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool);
    fn monitor_watermarks<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(usize, usize)>> + Send + 'a>>;
}

impl<T: ChannelSource + 'static> ChannelSourceObj for T {
    fn list_pvs<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::list_pvs(self))
    }
    fn has_pv<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::has_pv(self, name))
    }
    fn searchable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::searchable(self, name))
    }
    fn get_introspection<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FieldDesc>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_introspection(self, name))
    }
    fn has_pv_checked<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::has_pv_checked(self, name, ctx))
    }
    fn get_introspection_checked<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FieldDesc>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_introspection_checked(
            self, name, ctx,
        ))
    }
    fn get_value<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_value(self, name))
    }
    fn get_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::get_value_checked(
            self, checked, ctx,
        ))
    }
    fn access_gate(&self) -> &AccessGate {
        <Self as ChannelSource>::access(self)
    }
    fn revalidate_read<'a>(
        &'a self,
        pv_name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<AccessChecked>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::revalidate_read(self, pv_name, ctx))
    }
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value(self, name, value))
    }
    fn put_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value_checked(
            self, checked, value, ctx,
        ))
    }
    fn put_delta_checked<'a>(
        &'a self,
        checked: AccessChecked,
        desc: FieldDesc,
        changed: crate::proto::BitSet,
        delta: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_delta_checked(
            self, checked, desc, changed, delta, ctx,
        ))
    }
    fn is_writable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::is_writable(self, name))
    }
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe(self, name))
    }
    fn subscribe_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_checked(
            self, checked, ctx,
        ))
    }
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw(self, name))
    }
    fn subscribe_raw_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw_checked(
            self, checked, ctx,
        ))
    }
    fn subscribe_checked_opts<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_checked_opts(
            self, checked, ctx, opts,
        ))
    }
    fn subscribe_checked_opts_marked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<MonitorUpdate>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_checked_opts_marked(
            self, checked, ctx, opts,
        ))
    }
    fn subscribe_raw_checked_opts<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<mpsc::Receiver<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw_checked_opts(
            self, checked, ctx, opts,
        ))
    }
    fn rpc<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::rpc(
            self,
            name,
            request_desc,
            request_value,
        ))
    }
    fn rpc_checked<'a>(
        &'a self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::rpc_checked(
            self,
            checked,
            request_desc,
            request_value,
            ctx,
        ))
    }
    fn process<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::process(self, name))
    }
    fn process_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::process_checked(self, checked, ctx))
    }
    fn monitor_emits_partial(&self, name: &str) -> bool {
        <Self as ChannelSource>::monitor_emits_partial(self, name)
    }
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        <Self as ChannelSource>::notify_watermark(self, name, ctx, ev);
    }
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool) {
        <Self as ChannelSource>::notify_monitor_start(self, name, ctx, start);
    }
    fn monitor_watermarks<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(usize, usize)>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::monitor_watermarks(self, name))
    }
}
