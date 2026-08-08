//! [`ChannelSource`] — the trait every native PVA server is generic over.
//!
//! Uses our own [`crate::pvdata`] types, so only native types appear in the
//! public surface.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::proto::{MessageType, Status};
use crate::pvdata::{FieldDesc, PvField, RpcReply};
pub use epics_base_rs::server::access_security::{AccessChecked, AccessGate};

/// One pvxs `RemoteLogger::logRemote()` diagnostic recorded by a source
/// while serving an operation. The wire layer turns each into an
/// IOID-tagged `CMD_MESSAGE` frame (`serverconn.cpp:146-160`:
/// `ioid:u32 + messageType:u8 + message:string`), so `level` is the
/// PVA `messageType` byte (`level2mtype`, `pvaproto.h:715`): pvxs
/// `Level::Warn` → [`MessageType::Warning`] (1), `Level::Crit` →
/// [`MessageType::Fatal`] (3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLogMessage {
    pub level: MessageType,
    pub message: String,
}

/// pvxs `server::RemoteLogger` (`src/pvxs/srvcommon.h:97`) — the
/// source→client diagnostic channel every pvxs operation handle
/// (`ConnectOp`, `ExecOp`, `MonitorSetupOp`) implements.
///
/// A source records a diagnostic while it serves an operation; the wire
/// layer drains this sink once the source call returns and emits one
/// IOID-tagged `CMD_MESSAGE` Warning/Fatal frame per message *before*
/// that operation's reply (see `tcp.rs` `flush_remote_log`). This is the
/// only path by which a source can talk to the client outside the
/// operation's own status/value — pvxs's IOC source layer uses it for
/// "present but unusable option" diagnostics that do NOT change the
/// negotiated outcome (`ioc/groupsource.cpp:560`,
/// `ioc/singlesource.cpp:129`, `ioc/iocsource.cpp:447`).
///
/// Cheap to clone (one `Arc`); every clone of a [`ChannelContext`] shares
/// the same sink, so a source may hand the context down through its own
/// layers and still have its diagnostics reach the connection that owns
/// the IOID. Messages recorded on a context that belongs to no operation
/// (channel lifecycle / watermark edges, where pvAccess has no IOID to
/// tag and pvxs correspondingly exposes no `RemoteLogger`) are never
/// drained and are dropped with the context.
#[derive(Debug, Clone, Default)]
pub struct RemoteLog {
    queued: Arc<Mutex<Vec<RemoteLogMessage>>>,
}

impl RemoteLog {
    /// pvxs `logRemote(Level::Warn, msg)` — a `messageType=1` frame.
    pub fn warn(&self, message: impl Into<String>) {
        self.push(MessageType::Warning, message.into());
    }

    /// pvxs `logRemote(Level::Crit, msg)` — a `messageType=3` frame.
    pub fn crit(&self, message: impl Into<String>) {
        self.push(MessageType::Fatal, message.into());
    }

    fn push(&self, level: MessageType, message: String) {
        self.queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(RemoteLogMessage { level, message });
    }

    /// Drain every recorded diagnostic, in the order the source recorded
    /// them. The wire layer is the single caller; a source never drains
    /// its own sink.
    pub fn take(&self) -> Vec<RemoteLogMessage> {
        std::mem::take(&mut *self.queued.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Per-operation context surfaced to [`ChannelSource`] implementors
/// that need the downstream peer's identity (audit, ACL, gateway
/// credential pass-through). Fields mirror what `CONNECTION_VALIDATION`
/// established at handshake time, plus the peer's TCP socket address.
///
/// gateways use this to pick the correct upstream client
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
    /// Host identity of the peer as the ACF `HAG(...)` gate matches it —
    /// [`Self::peer`]'s address in numeric form, port stripped and
    /// IPv4-mapped IPv6 collapsed to IPv4 (QSRV `ioc/credentials.cpp:27-29`).
    ///
    /// NOT reverse-resolved, and never taken from the wire: a client's
    /// advertised `host` field is ignored by the CONNECTION_VALIDATION
    /// parser, because this is the string host-scoped ACF rules are matched
    /// against. See `ClientCredentials::host`.
    pub host: String,
    /// Certificate authority for the `x509` method: the root CA's
    /// subject CommonName. Empty for non-TLS methods. ACF
    /// `AUTHORITY(...)` rule scopes match against this.
    pub authority: String,
    /// Group / role memberships of the peer's `account`, re-derived
    /// SERVER-SIDE from the local passwd/group DB into
    /// `ClientCredentials::roles` (NEVER from the wire — pvxs
    /// `ClientCredentials::roles()` / `osdGetRoles`) and forwarded here so
    /// role-based ACF rules (`R member group:ops`, `role/...` credential
    /// strings) can be enforced for native PVA clients. A client cannot
    /// self-assign these to satisfy a group-gated rule.
    pub roles: Vec<String>,
    /// Decoded INIT pvRequest value for the current operation, when
    /// the wire layer captured one. PVA PUT INIT carries
    /// `record._options.process`/`block`; the data-phase payload is
    /// just the delta, so sources that interpret per-operation
    /// options must consult this rather than the value. For RPC this is
    /// the create-time pvRequest, kept distinct from the EXEC argument
    /// (`request_desc`/`request_value` of [`ChannelSource::rpc_checked`])
    /// so a source — or a gateway forwarding
    /// `createChannelRPC(..., pvRequest)` — can inspect it. For PROCESS it
    /// is the PROCESS INIT pvRequest (`record._options`).
    ///
    /// `None` for op kinds / paths where no pvRequest was captured (GET,
    /// where the request was consumed for masking) or when the wire
    /// decoder could not parse it. Sources that don't need per-op options
    /// can ignore the field.
    pub pv_request: Option<PvField>,
    /// pvxs `RemoteLogger` for this operation ([`RemoteLog`]). A source
    /// records "present but unusable option" diagnostics here; the wire
    /// layer drains them and emits IOID-tagged `CMD_MESSAGE` frames
    /// before the operation's reply. Contexts for edges with no IOID
    /// (channel open/close, watermark) carry a sink that is never
    /// drained — pvxs exposes no `RemoteLogger` there either.
    pub log: RemoteLog,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorOptions {
    /// `record._options.pipeline` — the client requested the
    /// flow-controlled credit/ACK monitor sub-protocol. This is flow
    /// control between the client and *its* server, not a change to
    /// which events are produced: a fanout gateway terminates the
    /// downstream pipeline on its own downstream connection and
    /// propagates backpressure upstream via the per-PV `Pauser`
    ///. It therefore does NOT make a monitor non-transparent.
    pub pipeline: bool,
    /// The NEGOTIATED per-operation monitor queue limit — pvxs
    /// `MonitorOp::limit` (`servermon.cpp:66`, initialised to `4u`),
    /// overridden by a valid `record._options.queueSize >= 2`
    /// (`:533-543`) whether or not pipeline flow control is on.
    ///
    /// ALWAYS resolved: this is the one representation of the depth, so
    /// no consumer re-derives it and none of them can disagree. It is
    /// what pvxs squashes against (`queue.size() < limit`, `:273`), what
    /// its `ackAt` arithmetic is a fraction of (`:564,578,581`), and what
    /// it reports as `stats().limitQueue` (`:313`). The port used to
    /// carry `Option<u32>` — `None` meaning "consult the server-wide
    /// default" — alongside a SECOND, separately-defaulted copy inside
    /// the pipeline options; the two defaults were different numbers
    /// (server 64 vs pipeline 4), so a plain monitor squashed at a depth
    /// the ACK arithmetic never saw (R11-31).
    ///
    /// Like `pipeline`, this is downstream buffer depth: a gateway honors
    /// it on its per-downstream outbox and it does not change upstream
    /// event production.
    pub queue_size: u32,
    /// True when the downstream pvRequest carried a server-side
    /// `record._options._filter` chain. A stateful filter (e.g.
    /// deadband) changes which events are *produced*; running it at a
    /// fanout gateway on a shared upstream stream is not equivalent to
    /// the upstream server running it per subscription.
    pub server_filter: bool,
}

/// pvxs `MonitorOp::limit = 4u` (`servermon.cpp:66`) — the depth a
/// server-side monitor queue starts with, before the client's
/// `record._options.queueSize` gets a say. It is a PER-OPERATION
/// initializer, not a server-wide capacity: pvxs has no server knob for
/// it at all, and [`crate::server_native::PvaServerConfig::monitor_queue_depth`]
/// is exactly this initializer made configurable.
pub const DEFAULT_MONITOR_QUEUE_LIMIT: u32 = 4;

impl Default for MonitorOptions {
    /// A plain monitor with no negotiated options: no pipeline, no
    /// server-side filter, and the pvxs per-op default depth. The depth
    /// is never 0 — [`MonitorOptions::queue_size`] is a resolved limit by
    /// construction, so a `default()` used for a non-MONITOR operation
    /// still names a legal one.
    fn default() -> Self {
        Self {
            pipeline: false,
            queue_size: DEFAULT_MONITOR_QUEUE_LIMIT,
            server_filter: false,
        }
    }
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
/// Why a [`ChannelSource`] mutating op (`put_value*`, `process*`,
/// `rpc*`) failed, carried as a typed value so audit / forwarding
/// layers classify the outcome from a discriminant rather than
/// substring-matching the human message. pvxs buckets the audit log
/// into "Denied" (access-control refusal) vs "Failed" (everything
/// else); [`OpErrorKind`] is that bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    /// Classification bucket — drives the audit Denied/Failed split.
    pub kind: OpErrorKind,
    /// Human-readable message; the text serialised into the PVA `Status`
    /// sent to the client when this error carries no `status` of its own.
    pub message: String,
    /// The `Status` a proxying source received from ITS upstream, to be sent
    /// downstream unchanged. `None` for an error this server originated.
    ///
    /// Set only through [`OpError::remote`]; read only through
    /// [`OpError::wire_status`], which is the single owner of "what Status
    /// does a failed op put on the wire".
    pub status: Option<Status>,
}

/// Outcome bucket for an [`OpError`]. Distinct from a free-text
/// message so that a layer never has to guess "was this a denial?"
/// from the words in the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpErrorKind {
    /// Access-control / policy refusal — ACF gate, gateway ACL, or
    /// read-only mode. Maps to pvxs's audit "Denied" bucket.
    Denied,
    /// Any other operation failure: PV not found, malformed value,
    /// upstream timeout, backend error. Maps to pvxs's "Failed".
    Failed,
}

impl OpError {
    /// An access-control / policy refusal (`Denied` bucket).
    pub fn denied(message: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::Denied,
            message: message.into(),
            status: None,
        }
    }

    /// A non-policy operation failure (`Failed` bucket).
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::Failed,
            message: message.into(),
            status: None,
        }
    }

    /// An upstream `Status`, to be forwarded downstream **verbatim** — the
    /// error a proxying source (the PVA gateway) returns when its upstream
    /// refused the operation.
    ///
    /// pva2pva does not re-author an upstream Status because it cannot: it
    /// hands the downstream requester straight to the upstream channel
    /// (`p2pApp/channel.cpp:117-127`), so the upstream's own reply reaches the
    /// downstream client. Here the two legs are separate ops, so the Status is
    /// carried across explicitly instead — kind, message, and stack intact.
    /// The audit bucket stays `Failed`: a refusal by SOMEONE ELSE'S access
    /// control is, to this server, an upstream failure, and its own ACL
    /// denials are what the `Denied` bucket counts.
    pub fn remote(status: Status) -> Self {
        Self {
            kind: OpErrorKind::Failed,
            message: status.message().unwrap_or_default().to_string(),
            status: Some(status),
        }
    }

    /// The `Status` this error puts on the wire — the upstream's own when this
    /// is a forwarded [`Self::remote`], else an `Error` status carrying the
    /// message. Single owner: every server reply path for a failed op goes
    /// through here, so no path can flatten a forwarded Status into text.
    pub fn wire_status(&self) -> Status {
        self.status
            .clone()
            .unwrap_or_else(|| Status::error(self.message.clone()))
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpError {}

/// A bare string error is an operation **Failure**, not a policy
/// denial — denials must be built explicitly via [`OpError::denied`].
/// This lets existing `?` / `Err(string)` failure paths convert
/// unchanged while the audit layer still distinguishes refusals.
impl From<String> for OpError {
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

impl From<&str> for OpError {
    fn from(message: &str) -> Self {
        Self::failed(message)
    }
}

// NOTE: no `impl From<OpError> for String`. It existed so that
// `Status::error(op_err)` compiled — which is exactly the flattening R18-27
// is about: it discarded a forwarded upstream `Status` (kind, stack) and
// re-authored it as a local ERROR carrying the rendered text. The wire path
// is [`OpError::wire_status`] and nothing else; removing the conversion makes
// the lossy path fail to compile rather than fail on the wire.

/// Server-wide channel-invalidation fan-out. An operator-driven cache
/// removal (PVA gateway `<prefix>:drop` / `:flush`) publishes the set of
/// removed PV names through [`Self::publish`]; every per-connection task
/// owns a receiver from [`Self::subscribe`] and force-disconnects the
/// downstream channels it serves under those names with a server-initiated
/// `DESTROY_CHANNEL`. That is the downstream effect of pva2pva dropping a
/// `ChannelCacheEntry`: `channel->destroy()` → `channelStateChange(DESTROYED)`
/// fanout to every interested `GWChannel`
/// (`p2pApp/chancache.cpp:34-99`, `server.cpp:130-135`).
///
/// **Lossless by construction.** pva2pva's fanout iterates a live
/// listener vector under lock — there is no queue between removal and
/// `DESTROYED`, so no force-disconnect is ever dropped. This type matches
/// that property two ways, replacing the earlier bounded
/// `tokio::broadcast<String>` whose 1024-deep ring could silently drop
/// names on a large `:flush`:
///
/// 1. **Per-connection unbounded queues.** Each connection holds its own
///    [`mpsc::UnboundedReceiver`] — there is no shared ring buffer to
///    overflow, so a slow connection can never make another connection
///    (or itself) miss an invalidation.
/// 2. **One command, one batch.** A removal publishes the whole removed
///    set as a single [`Arc<[String]>`], so a `:flush` of the full
///    50,000-entry cache is one message per connection, not one per name.
///
/// Memory is bounded by connection lifetime: a wedged connection that
/// never drains is itself torn down by the op timeout / TCP keepalive,
/// dropping its receiver; dead senders are pruned on the next
/// [`Self::publish`] / [`Self::subscribe`]. Cheap to clone (one `Arc`).
#[derive(Clone, Default)]
pub struct ChannelInvalidator {
    subscribers: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<Arc<[String]>>>>>,
}

impl ChannelInvalidator {
    /// A fresh invalidator with no subscribers. The server creates one per
    /// `PvaServer` and hands a clone to the source
    /// (which `publish`es) and to every per-connection task (which
    /// `subscribe`s).
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a per-connection receiver. Closed subscribers (their
    /// receiver dropped) are pruned here so a high connection-churn
    /// workload cannot grow the registry between publishes.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Arc<[String]>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut subs = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| !s.is_closed());
        subs.push(tx);
        rx
    }

    /// Publish one removal batch to every live connection. Unbounded
    /// queues never drop, so no force-disconnect is lost; senders whose
    /// receiver has gone are pruned in passing. An empty batch is a no-op.
    pub fn publish(&self, names: Arc<[String]>) {
        if names.is_empty() {
            return;
        }
        let mut subs = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| s.send(names.clone()).is_ok());
    }
}

/// `serverget.cpp:486` — the text pvxs sends when an RPC EXEC lands on a
/// channel whose source installed no `onRPC` handler.
pub const RPC_NOT_IMPLEMENTED: &str = "RPC Not Implemented";

/// A `record._options` field whose storage no conversion arm accepts (an
/// ARRAY-typed `DBE`, say) fails the operation that named it — and only that
/// operation.
///
/// pvxs sources read their `record._options` inside `onSubscribe`, through the
/// THROWING `Value::as<T>()`. `SingleSource` dispatches on the field's KIND and
/// then converts (`ioc/singlesource.cpp:117-140`): `Kind::String` runs
/// `fld.as<std::string>()`, `Kind::Integer`/`Kind::Real` run
/// `fld.as<uint8_t>()`. Kind is the type-code class, so an ARRAY of those kinds
/// (`Int32A` is `Kind::Integer`) reaches the conversion — and `Value::copyOut`
/// has no scalar arm for array storage, so it raises `NoConvert`
/// (`data.cpp:466-499`).
///
/// See [`ChannelSource::check_monitor_request`] for what the port does with
/// that (an op-level error `Status`, circuit intact) and why it does NOT do
/// what QSRV does (CBUG-C2: `bev.reset()`, every channel on the circuit gone).
impl From<crate::pvdata::NoConvert> for OpError {
    fn from(e: crate::pvdata::NoConvert) -> Self {
        OpError::failed(e.message().to_string())
    }
}

pub trait ChannelSource: Send + Sync + 'static {
    /// Per-source access policy. Returns the [`AccessGate`] used by
    /// the wire layer to mint [`AccessChecked`] tokens for the typed
    /// op methods (`*_checked` family). Default impl returns a
    /// process-wide singleton `Open` gate — sources that need ACF
    /// enforcement override to install a `Required` gate wrapping
    /// their `AcfCell`.
    ///
    /// Type-state ACF gate: the typed op methods take
    /// `AccessChecked` instead of `name: &str + ctx`. Because
    /// `AccessChecked` is unforgeable outside this gate's `check`,
    /// every wire op MUST flow through it — closing the missed-path
    /// pattern that surfaced across rounds 32-39.
    fn access(&self) -> &AccessGate {
        static OPEN_GATE: std::sync::OnceLock<AccessGate> = std::sync::OnceLock::new();
        OPEN_GATE.get_or_init(AccessGate::open)
    }

    /// Monotonic counter of source-registry topology changes, mirroring
    /// pvxs `Server::pvt->beaconChange` (server.cpp:90-115). pvxs bumps
    /// this on every `addSource`/`removeSource`/`addPV`/`removePV` and
    /// writes it into every BEACON frame (server.cpp:751-767), so a
    /// client can detect a server-side registry change even when the
    /// enumerated PV-name set is unchanged (a source replaced by another
    /// serving the same names, or a priority change).
    ///
    /// Default `0` for leaf sources whose registry never changes; the
    /// registry owner ([`crate::server_native::CompositeSource`])
    /// overrides this to return its live counter. The beacon task folds
    /// this into the beacon `change_count`, keeping the PV-set hash only
    /// as a fallback for sources that mutate their own list without
    /// going through a registry mutation API.
    fn beacon_change(&self) -> u64 {
        0
    }

    /// Enumerate every PV name this source can serve.
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send;

    /// True iff `name` resolves to a known PV.
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// True iff `name` should be answered to a UDP SEARCH broadcast.
    ///
    /// Independent of [`Self::has_pv`] in BOTH directions — pvxs models
    /// `onSearch` and `onCreate` as separate callbacks, and this is the
    /// `onSearch` half. It is asked on its own; the caller never ANDs
    /// `has_pv` into it.
    ///
    /// - *Narrower* than `has_pv`: a name may be reachable via a direct
    ///   TCP connect (`has_pv` true) yet deliberately NOT be advertised on
    ///   UDP discovery (`searchable` false). pvxs's built-in `ServerSource`
    ///   does exactly this — `onSearch` is empty so the `server` PV resolves
    ///   only by direct connect, never by broadcast SEARCH
    ///   (`serversource.cpp`); the built-in
    ///   [`crate::server_native::ServerInfoSource`] overrides this to `false`.
    /// - *Wider* than `has_pv`: a source may advertise a name it will then
    ///   REFUSE at CREATE_CHANNEL. pvxs's `SingleSource` claims every name
    ///   `dbChannelTest` resolves (`singlesource.cpp:467-472`) and only
    ///   discovers at `onCreate` that the field has no NT, answering
    ///   `Refused to create Channel`. Withholding the search reply instead
    ///   would turn that prompt refusal into a client-side timeout — a
    ///   different observable, not the same one. See
    ///   [`PvDatabaseSource`](crate::server::PvDatabaseSource).
    ///
    /// The default impl delegates to `has_pv`, so a source that does not
    /// distinguish the two questions is unaffected.
    fn searchable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        self.has_pv(name)
    }

    /// True iff `name` should be answered to a SEARCH from `requester`.
    ///
    /// pvxs exposes the requester endpoint to a source's `onSearch` as
    /// `Search::source()` — filled from `msg.replyDest` for UDP
    /// (server.cpp:674-704) and from the established TCP peer for
    /// circuit search (serverchan.cpp:197-222) — so a source can scope
    /// advertisement by requester (claim a PV only for a local subnet,
    /// hide private aliases from some peers, pick a redirect policy that
    /// depends on the client endpoint).
    ///
    /// Default ignores the endpoint and defers to [`Self::searchable`],
    /// so simple sources keep answering every requester the same way.
    /// A source that wants endpoint-scoped advertisement overrides this.
    fn searchable_from(
        &self,
        name: &str,
        requester: SocketAddr,
    ) -> impl std::future::Future<Output = bool> + Send {
        let _ = requester;
        self.searchable(name)
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

    /// Resolve the source that OWNS `name` for this peer, to be bound
    /// into the channel at CREATE_CHANNEL so every later operation
    /// dispatches to the same source that accepted the channel — never
    /// re-resolving the registry per operation.
    ///
    /// pvxs iterates server sources at CREATE_CHANNEL and stops at the
    /// first that accepts, then installs THAT source's `onOp`/`onRPC`/
    /// `onSubscribe` callbacks into the `ServerChan`
    /// (`serverchan.cpp:295-322`, `serverchan.cpp:70-112`); a later
    /// `Server::removeSource` does not rewrite callbacks already
    /// installed on existing channels (`server.cpp:100-112`). A
    /// terminal/leaf source IS its own owner, so the default returns
    /// `None` and the caller binds the top-level source itself.
    /// [`CompositeSource`](crate::server_native::CompositeSource)
    /// overrides this to return the matched inner source (descending
    /// through nested composites to the leaf), so a live channel never
    /// silently changes owner when the registry is mutated.
    fn resolve_owner(
        &self,
        _name: &str,
        _ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<DynSource>> + Send {
        async { None }
    }

    /// Source-supplied contextual info to attach to a channel at
    /// CREATE_CHANNEL, surfaced verbatim in the server report's
    /// per-channel `info` field.
    ///
    /// pvxs lets a Source stash an opaque `ReportInfo`
    /// (`netcommon.h:70`) on the channel control it is handed during
    /// `onCreate`, via `ServerChannelControl::updateInfo`
    /// (`source.h:192`); `Server::report()` then copies that pointer
    /// into `Report::Channel::info` (`netcommon.h:75`). The Rust server
    /// simplifies the opaque base class to an `Option<String>` and
    /// queries it ONCE at channel admission from the bound owner — the
    /// source resolved by [`Self::resolve_owner`] — rather than handing
    /// the source a mutable control handle. Default returns `None` (a
    /// source that attaches no per-channel info, like pvxs leaving
    /// `chan->reportInfo` null).
    fn channel_report_info(
        &self,
        _name: &str,
        _ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        async { None }
    }

    /// Fetch the current value of a PV.
    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send;

    /// **The read handoff the server FRAMES.** Every value the server
    /// serializes with a changed-bitset — the GET reply, the PUT_GET
    /// readback, the connect-time monitor seed — comes back through here as
    /// a [`SourceRead`]: the value plus, optionally, the leaves the source
    /// ACTUALLY assigned into it.
    ///
    /// pvxs frames a read as `to_wire_valid(R, value, &pvMask)`
    /// (`serverget.cpp:104`), and that value is a `cloneEmpty()` the source
    /// filled in: `IOCSource::initialize` + `IOCSource::get`
    /// (`singlesource.cpp:283`, `groupsource.cpp:484`) assign a SUBSET of
    /// the structure, so only those leaves carry `valid` and only they reach
    /// the wire. `getProperties` (`iocsource.cpp:252-310`) never assigns
    /// `control.minStep`, `valueAlarm.active`, the four `valueAlarm.*Severity`
    /// leaves or `valueAlarm.hysteresis` — pinned by pvxs's own
    /// `testqsingle.cpp:129-149` delta, where those seven are absent while
    /// `display.form.index` / `.choices` (from `initialize`) are present.
    ///
    /// The port's `PvField` has no "unassigned" state — every NT leaf is
    /// populated — so a source that assigns a subset says so with `marked`.
    /// `marked: None` means "everything the request selected", which is what
    /// a source posting a wholly-assigned value means (a `SharedPV`'s
    /// `open()`-ed Value, a gateway's upstream snapshot).
    ///
    /// The default routes through [`Self::get_value_checked`], so every
    /// ACL / credential-routing override a source already has still applies;
    /// a source overrides THIS method only to declare its marks.
    fn read_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<SourceRead>> + Send {
        async move {
            self.get_value_checked(checked, ctx)
                .await
                .map(SourceRead::from)
        }
    }

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
    /// inspects `checked.allows_read()` and dispatches. The legacy
    /// `get_value_ctx` path was deleted — every credential-
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
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send;

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
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "PUT denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
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
    /// sparse value, borrowed because the wire layer reuses it as the
    /// channel's decode scratch across EXECs. Only `changed`-marked
    /// fields carry client data; unmarked slots hold type defaults or
    /// stale values from an earlier EXEC and MUST NOT be read.
    fn put_delta_checked(
        &self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
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
                    &desc,
                    &changed,
                    0,
                    delta.clone(),
                    &prior,
                ),
                None => delta.clone(),
            };
            self.put_value_checked(checked, merged, ctx).await
        }
    }

    /// Type-state-enforced atomic **PUT_GET**: apply the BitSet-delta PUT,
    /// then return the post-put readback — the two legs of a PVA PUT_GET
    /// (cmd 12) as one source operation.
    ///
    /// The default impl composes the existing primitives: the WRITE-gated
    /// [`Self::put_delta_checked`] followed by the READ-gated
    /// [`Self::get_value_checked`], reusing the same authenticated
    /// `checked` token and `ctx` for both legs (the readback runs under the
    /// identical identity as the write, exactly as `put_delta_checked`'s
    /// own prior-value read does — a `ReadWrite` token allows both). Every
    /// source that owns its record directly is therefore unchanged by
    /// construction: its PUT_GET stays put-then-get over the same backing
    /// store.
    ///
    /// Sources that front a *remote* server override this to issue a single
    /// upstream PUT_GET, so the put-then-get stays atomic upstream instead
    /// of collapsing to a local put plus a separately-read (possibly
    /// cached) get. The pva-gateway forwards `ctx.pv_request` + value as one
    /// upstream `ChannelPutGet` (pva2pva `p2pApp/channel.cpp:129-137`) and
    /// returns its readback.
    fn put_get_checked(
        &self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<Option<SourceRead>, OpError>> + Send {
        async move {
            self.put_delta_checked(checked.clone(), desc, changed, delta, ctx.clone())
                .await?;
            Ok(self.read_checked(checked, ctx).await)
        }
    }

    // ── ChannelArray (PVA cmd 14) operation surface ────────────────────
    //
    // ChannelArray is the windowed array operation: an INIT binds an
    // array-typed field of the PV, then `get`/`put` transfer a
    // `[offset, count, stride]` slice and `getLength`/`setLength` query or
    // resize it (pvAccessCPP `responseHandlers.cpp:2115-2208`,
    // `clientContextImpl.cpp:1567-1666`). pvxs has no server-side
    // ChannelArray handler at all — its connection dispatch drops CMD_ARRAY
    // into the `default:` "ignore unexpected command" arm
    // (`conn.cpp:248-253`), so a client hangs. These methods give every
    // source a *defined* answer instead: the default rejects with a
    // protocol `Status` error (no silent drop), and the wire layer always
    // replies. A source that genuinely serves windowed arrays (the PVA
    // gateway forwarding to an upstream pvAccessCPP IOC; pva2pva
    // `GWChannel::createChannelArray`, `p2pApp/channel.cpp:226-232`)
    // overrides them.
    //
    // The INIT pvRequest (which selects the array field) is threaded to
    // every sub-op through [`ChannelContext::pv_request`] — the wire layer
    // stashes it at INIT and re-supplies it on each get/put/length call —
    // so the trait stays stateless per call, matching how PROCESS / PUT_GET
    // forward their create-time `record._options`.

    /// ChannelArray INIT — bind the array-typed field this operation
    /// windows over and return its [`FieldDesc`] introspection (serialised
    /// into the INIT reply, pvAccessCPP `responseHandlers.cpp:2381-2385`
    /// `cachedSerialize(_pvArray->getArray())`). `ctx.pv_request` carries
    /// the decoded INIT pvRequest selecting the field. The default refuses:
    /// the source serves no windowed array, so the wire layer answers the
    /// client with the returned error as an INIT `Status::error` rather than
    /// dropping the frame.
    fn channel_array_init(
        &self,
        name: &str,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<FieldDesc, OpError>> + Send {
        let _ = (name, ctx);
        async {
            Err(OpError::failed(
                "channel array not supported by this source",
            ))
        }
    }

    /// ChannelArray `getArray` — return the `[offset, count, stride]` slice
    /// of the bound array as a [`PvField`] (pvAccessCPP
    /// `responseHandlers.cpp:2172-2178`). READ-gated: a token without read
    /// access is refused. `count == 0` means "to the end of the array"
    /// (pvAccessCPP `getArray` API contract). Default refuses after the
    /// read gate.
    fn channel_array_get(
        &self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<PvField, OpError>> + Send {
        async move {
            if !checked.allows_read() {
                return Err(OpError::denied(format!(
                    "ARRAY get denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
            }
            let _ = (offset, count, stride);
            Err(OpError::failed(
                "channel array get not supported by this source",
            ))
        }
    }

    /// ChannelArray `putArray` — splice `value` into the bound array at
    /// `offset` with `stride` (pvAccessCPP `responseHandlers.cpp:2190-2206`).
    /// WRITE-gated. Default refuses after the write gate.
    fn channel_array_put(
        &self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "ARRAY put denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
            }
            let _ = (offset, stride, value);
            Err(OpError::failed(
                "channel array put not supported by this source",
            ))
        }
    }

    /// ChannelArray `setLength` — resize the bound array (pvAccessCPP
    /// `responseHandlers.cpp:2180-2184`). WRITE-gated. Default refuses after
    /// the write gate.
    fn channel_array_set_length(
        &self,
        checked: AccessChecked,
        length: u32,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "ARRAY setLength denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
            }
            let _ = length;
            Err(OpError::failed(
                "channel array setLength not supported by this source",
            ))
        }
    }

    /// ChannelArray `getLength` — return the current element count of the
    /// bound array (pvAccessCPP `responseHandlers.cpp:2186-2188`,
    /// `:2376-2380` `writeSize(_length)`). READ-gated. Default refuses after
    /// the read gate.
    fn channel_array_get_length(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<u32, OpError>> + Send {
        async move {
            if !checked.allows_read() {
                return Err(OpError::denied(format!(
                    "ARRAY getLength denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
            }
            Err(OpError::failed(
                "channel array getLength not supported by this source",
            ))
        }
    }

    /// True iff PUT is allowed against this PV (for ACL gating).
    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send;

    /// pvxs's `onSubscribe` pvRequest read, run at MONITOR INIT — the part of
    /// it that can FAIL.
    ///
    /// A pvxs source parses its `record._options` at the top of `onSubscribe`,
    /// before `connect()` sends the INIT reply, using the throwing
    /// `Value::as<T>()`. An option whose storage no `copyOut` arm converts
    /// raises `NoConvert`. The port splits the two halves of `onSubscribe`:
    /// this check runs at INIT, while the subscription itself is opened at
    /// START.
    ///
    /// DEVIATION from C++, deliberate — CBUG-C2. In pvxs's QSRV sources
    /// (`ioc/singlesource.cpp:147`, `ioc/groupsource.cpp:399`) that `NoConvert`
    /// is thrown out of a bare `connect()`, is caught by nobody on the way out
    /// of `servermon.cpp:592`, and reaches the command-dispatch `catch` in
    /// `conn.cpp:277-282`, which calls `bev.reset()`: ONE client's malformed
    /// `record._options` tears down the whole TCP circuit, killing every other
    /// channel multiplexed on it. A per-operation failure must stay per-
    /// operation, so this hook's `Err` is an [`OpError`] — the INIT reply
    /// carries an error `Status`, the op is not registered, and the circuit and
    /// every other channel on it survive. There is deliberately no way for a
    /// source to reset the circuit from here: the outcome is not representable.
    /// (pvxs's own library source agrees with us — `SharedPV::Impl::connectSub`,
    /// `sharedpv.cpp:94-101`, catches around `connect()` and calls
    /// `conn->error(msg)`, "not re-throwing for consistency". Only QSRV's
    /// sources leave the throw bare.)
    ///
    /// The default is `Ok(())`: a source that reads no `record._options` — the
    /// pvxs server API's `SharedPV`, and `GroupSource`, neither of which looks
    /// at `DBE` — has nothing to fail on. Only a source that reads an option
    /// through the throwing conversion overrides this, and it must return `Ok`
    /// for the names it does NOT read that option for.
    fn check_monitor_request(
        &self,
        checked: &AccessChecked,
        ctx: &ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let _ = (checked, ctx);
        async { Ok(()) }
    }

    /// Subscribe to value-change notifications. Returns `None` if unknown.
    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send;

    /// Type-state-enforced MONITOR. Refuses `NoAccess` tokens; on
    /// any READ-class level delegates to the legacy ctx-less
    /// `subscribe`. Sources that need credential-aware MONITOR
    /// routing override this directly.
    fn subscribe_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
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
    ) -> impl std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send {
        let _ = name;
        async { None }
    }

    /// Type-state-enforced raw MONITOR fast path. NoAccess → None;
    /// otherwise delegates to ctx-less `subscribe_raw`.
    fn subscribe_raw_checked(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send {
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
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
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
    ) -> impl std::future::Future<Output = Option<MonitorStream<MonitorUpdate>>> + Send {
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
    ) -> impl std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send {
        let _ = opts;
        self.subscribe_raw_checked(checked, ctx)
    }

    /// **Single MONITOR seed owner.** The server's monitor START
    /// dispatch calls this (not [`Self::subscribe_checked_opts_marked`]
    /// directly) so the connect-time seed and the post-seed update
    /// stream come back together as one [`SubscriptionSeed`]; the server
    /// emits `initial` then drains `updates` and never issues its own
    /// `get_value` seed. This removes the prior double-seed where the
    /// server's initial `get_value_checked` and a self-seeding source's
    /// stream both delivered the connect-time value (pvxs posts the
    /// current value exactly once at attach, `sharedpv.cpp:69-92`;
    /// pva2pva copies one `lastelem` per `start()`, `moncache.cpp:270-320`).
    ///
    /// The default seeds from the ACF-aware [`Self::read_checked`]
    /// (server-equivalent) and treats the source's
    /// [`Self::subscribe_checked_opts_marked`] stream as updates-only —
    /// correct for every source whose subscription does not itself
    /// replay the current value. A source that must capture the seed
    /// atomically with subscriber registration (the PVA gateway's shared
    /// upstream-monitor `snapshot`) overrides this to return both from
    /// one critical section.
    fn subscribe_seeded(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> impl std::future::Future<Output = Option<SubscriptionSeed<MonitorUpdate>>> + Send {
        async move {
            let updates = self
                .subscribe_checked_opts_marked(checked.clone(), ctx.clone(), opts)
                .await?;
            let initial = self.read_checked(checked, ctx).await;
            Some(SubscriptionSeed {
                initial,
                updates,
                on_start: None,
            })
        }
    }

    /// Raw fast-path counterpart of [`Self::subscribe_seeded`]. Returns
    /// a decoded [`SourceRead`] seed (the server encodes the START frame
    /// through the regular path even on the raw path) plus the raw
    /// update stream. Default seeds via [`Self::read_checked`] and
    /// delegates the stream to [`Self::subscribe_raw_checked_opts`];
    /// returns `None` when the source exposes no raw path.
    fn subscribe_raw_seeded(
        &self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> impl std::future::Future<Output = Option<SubscriptionSeed<RawMonitorEvent>>> + Send {
        async move {
            let updates = self
                .subscribe_raw_checked_opts(checked.clone(), ctx.clone(), opts)
                .await?;
            let initial = self.read_checked(checked, ctx).await;
            Some(SubscriptionSeed {
                initial,
                updates,
                on_start: None,
            })
        }
    }

    /// Dispatch an RPC. The default impl is the "source installed no RPC
    /// handler" case, which pvxs answers with the fixed text
    /// `"RPC Not Implemented"` (`!chan->onRPC`, serverget.cpp:482-486);
    /// implementors override to provide actual RPC behaviour.
    ///
    /// Returns an [`RpcReply`] on success — pvxs's two `ExecOp::reply()`
    /// overloads (`pvxs/srvcommon.h:108`): `RpcReply::Value(desc, value)` for
    /// `reply(Value)`, and `RpcReply::Empty` for the no-argument `reply()`,
    /// which puts a bare `0xFF` NULL type code on the wire with no value body
    /// (`serverget.cpp:105-109`, `dataencode.cpp:29-33`). A
    /// `(FieldDesc, PvField)` pair converts into the former via `.into()`.
    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
        let _ = (name, request_desc, request_value);
        async move { Err(OpError::failed(RPC_NOT_IMPLEMENTED)) }
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
    ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
        async move {
            if !checked.allows_read() {
                return Err(OpError::denied(format!(
                    "RPC denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
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
    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
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
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "PROCESS denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                )));
            }
            self.process(checked.pv_name()).await
        }
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

    /// A peer channel to `name` has been admitted — CREATE_CHANNEL
    /// resolved the PV and the channel was inserted into the connection's
    /// channel table. Fired exactly once per opened channel by the
    /// channel-lifecycle owner, matching pvxs `SharedPV::attach()` which
    /// inserts each `ChannelControl` into `impl->channels` and runs
    /// `onFirstConnect` on the empty→non-empty transition
    /// (`sharedpv.cpp:299-313`). This is a *channel* edge, distinct from
    /// the *monitor-operation* edge [`Self::notify_monitor_start`]: it
    /// fires for a channel that only ever carries GET/PUT/RPC/GET_FIELD
    /// traffic and never opens a monitor, which is exactly the
    /// lazy-resource pattern pvxs tests (`testget.cpp:204-234`). A source
    /// uses it to acquire per-channel leases or open lazily on first
    /// attach. `ctx` is credential-scoped (no `pv_request`). Paired with
    /// [`Self::notify_channel_close`]. Default impl ignores it.
    fn notify_channel_open(&self, name: &str, ctx: &ChannelContext) {
        let _ = (name, ctx);
    }

    /// A peer channel to `name` has closed — the client sent
    /// `DESTROY_CHANNEL` or the TCP connection dropped. Fired exactly
    /// once per opened channel by the channel-lifecycle owner AFTER the
    /// channel's operations have been torn down, matching pvxs
    /// `ServerChan::cleanup()` which cleans every channel op and then
    /// invokes the moved `onClose` callback once
    /// (`serverchan.cpp:43-60`, `:115-127`). Unlike
    /// [`Self::notify_monitor_start`] — a *monitor-operation* edge — this
    /// is a *channel* edge: it fires even for a channel that only ever
    /// carried GET/PUT/RPC traffic and never had a monitor. A source uses
    /// it to release per-channel leases, upstream identities, diagnostics,
    /// or credential-scoped caches. `ctx` is credential-scoped (no
    /// `pv_request`) like [`Self::notify_monitor_start`]. Default impl
    /// ignores it.
    fn notify_channel_close(&self, name: &str, ctx: &ChannelContext) {
        let _ = (name, ctx);
    }

    /// Register the server's [`ChannelInvalidator`] with this source. The
    /// server creates one per `PvaServer` and hands
    /// it here before accepting connections. A source that can invalidate a
    /// channel out-of-band keeps the handle and `publish`es the PV name of
    /// every channel that must be force-disconnected; each per-connection
    /// task holds a receiver and, for a name it currently serves, tears that
    /// channel down with a server-initiated `DESTROY_CHANNEL`. That is the
    /// downstream effect of pva2pva dropping a `ChannelCacheEntry` —
    /// `channel->destroy()` → `channelStateChange(DESTROYED)` fanout to
    /// every interested downstream `GWChannel` (`p2pApp/server.cpp:130-135`,
    /// `chancache.cpp:34-99`). The PVA gateway uses this so an operator
    /// `<prefix>:drop` / `:flush` actually disconnects the live downstream
    /// channels instead of leaving them bound to a silently re-created
    /// upstream entry. The invalidation is lossless by construction (see
    /// [`ChannelInvalidator`]) — a large `:flush` cannot drop names.
    /// Default impl ignores it (a source that never invalidates channels
    /// out of band).
    fn set_channel_invalidator(&self, invalidator: ChannelInvalidator) {
        let _ = invalidator;
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
/// `marked == None` means the source declares no leaf set, so the server
/// frames the full request mask — a source whose every event carries a
/// whole value (pvxs's fully-marked `Value`).
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
    /// When `true`, this event signals an upstream **descriptor
    /// change** — a subscription boundary, not a value. `value`/`marked`
    /// are meaningless and MUST NOT be encoded under the monitor's
    /// negotiated INIT descriptor: the next decoded value is shaped for
    /// the new upstream descriptor and would be mis-encoded against the
    /// stale one. The decoded monitor dispatch loop checks this flag
    /// FIRST and emits `MONITOR FINISH` before reading `value`, so the
    /// client reopens with a fresh INIT — the decoded-path counterpart of
    /// [`RawMonitorEvent::type_changed`]. pvxs treats reconnect /
    /// type-change as a subscription boundary
    /// (pvalink_channel.cpp:342-351 `onTypeChange()`). Only the PVA
    /// gateway's fanout sets this; sources that own their descriptor
    /// leave it `false`.
    pub type_changed: bool,
    /// Dot-separated field paths whose intermediate transitions were
    /// LOST before this event — the port's own loss accounting, NOT a
    /// wire field.
    ///
    /// **It does not reach the cooked wire.** Every cooked MONITOR DATA
    /// frame this server builds ends in a hard-empty overrun bitset,
    /// because pvxs's does: `servermon.cpp:174-176` writes one
    /// unconditionally (`// TODO: placeholder for overrun mask`), and a
    /// pvxs client that sees overrun bits sets `servSquash` / bumps
    /// `nSrvSquash` (`clientmon.cpp:554-564`), a counter that stays 0
    /// against a real pvxs server. Only the RAW forwarder puts overrun
    /// bits on the wire, and only the ones an UPSTREAM server's frame
    /// already carried.
    ///
    /// The server's own queue overflow is one producer: when the
    /// monitor queue coalesces (squashes) a dropped intermediate into
    /// the surviving value, every leaf that
    /// changed in BOTH the dropped and the surviving update is recorded
    /// here, and the two updates' overrun sets union — pva2pva
    /// `moncache.cpp:160-168`
    /// (`overrun |= upstream_overrun | (changed & lastelem.changed)`).
    /// A fanout gateway is the other producer: it sets this when its
    /// downstream broadcast receiver lags. Sources with no loss to
    /// report leave it empty.
    pub overrun: Vec<String>,
}

impl MonitorUpdate {
    /// A descriptor-change boundary marker. Carries a placeholder value
    /// that consumers MUST NOT encode — the decoded monitor loop emits
    /// `MONITOR FINISH` on `type_changed` before any value read.
    pub fn type_change() -> Self {
        Self {
            value: PvField::Null,
            marked: None,
            type_changed: true,
            overrun: Vec::new(),
        }
    }
}

impl From<PvField> for MonitorUpdate {
    /// A plain value with no explicit marked set — the server frames the
    /// full request mask, as it does for any wholly-assigned value. No
    /// overrun: a freshly produced value reports no lost intermediate.
    fn from(value: PvField) -> Self {
        Self {
            value,
            marked: None,
            type_changed: false,
            overrun: Vec::new(),
        }
    }
}

/// A value the server is about to FRAME with a changed-bitset — a GET
/// reply, a PUT_GET readback, or a connect-time monitor seed — plus the
/// leaves the source assigned into it. The read-side mirror of
/// [`MonitorUpdate`]'s `value` + `marked` pair, and `marked` carries the
/// identical meaning on both: the dot-separated field paths the source
/// actually wrote, or `None` for "everything the request selected".
///
/// This exists because pvxs reads a `cloneEmpty()` that its source only
/// partially fills (`IOCSource::initialize` + `IOCSource::get`), so
/// `to_wire_valid(R, value, &pvMask)` frames a SUBSET of the request mask.
/// The port's `PvField` is always fully populated, so the subset has to be
/// stated. See [`ChannelSource::read_checked`].
#[derive(Debug, Clone)]
pub struct SourceRead {
    /// The value to serialize.
    pub value: PvField,
    /// The leaves the source assigned, or `None` for "all of them".
    pub marked: Option<Vec<String>>,
}

impl SourceRead {
    /// A read whose marked leaves the source declares explicitly.
    pub fn marked(value: PvField, marked: Vec<String>) -> Self {
        Self {
            value,
            marked: Some(marked),
        }
    }
}

impl From<PvField> for SourceRead {
    /// A wholly-assigned value: the server frames every leaf the request
    /// selected, which is what pvxs's fully-marked `Value` frames.
    fn from(value: PvField) -> Self {
        Self {
            value,
            marked: None,
        }
    }
}

impl From<SourceRead> for MonitorUpdate {
    /// The connect-time seed IS the monitor's first post (pvxs's `first`),
    /// so it enters the queue as one — carrying the same marked set the
    /// source declared for the read.
    fn from(read: SourceRead) -> Self {
        Self {
            value: read.value,
            marked: read.marked,
            type_changed: false,
            overrun: Vec::new(),
        }
    }
}

/// Adapt a plain `PvField` monitor stream into a [`MonitorUpdate`]
/// stream that carries no explicit marked set (`marked: None`). Used by
/// every cooked source without a `+trigger` graph so the
/// [`ChannelSource::subscribe_checked_opts_marked`] item type is uniform
/// while the source keeps producing bare `PvField`s on its own channel.
///
/// Costs no task: [`MonitorStream::Mapped`] applies `MonitorUpdate::from`
/// as the consumer pulls. Before the ring widening this spawned a copy
/// loop purely because the trait pinned the return type to
/// `mpsc::Receiver`.
pub fn plain_monitor_updates(rx: MonitorStream<PvField>) -> MonitorStream<MonitorUpdate> {
    rx.map_plain(MonitorUpdate::from)
}

// ── MonitorStream — the monitor transport the ChannelSource trait carries ───

/// One PVA monitor update stream, whatever is actually producing it.
///
/// # Why this replaced `mpsc::Receiver<T>` in the trait
///
/// `ChannelSource`'s monitor methods used to return `mpsc::Receiver<T>`.
/// That pinned the *transport*, not just the element type, so every source
/// whose events arrive some other way had to spawn a task whose entire body
/// was "read from my real stream, push into an mpsc". Six such tasks existed
/// (two in `shared_pv`, one here, three in `server/native_source`), which put
/// a db-backed MONITOR at 2–3 tasks instead of 1 — the RTEMS task-count
/// problem in `doc/rtems-runtime-portability-design.md` §9 phase 6.
///
/// Widening the trait to this enum removes the reason those tasks existed:
/// a source hands back whatever it actually has, and the consumer pulls.
///
/// # Cost
///
/// Allocation-free, per event *and* per subscription. Every variant holds its
/// producer inline — `Mapped` too, because its inner is the non-recursive
/// [`PlainMonitor`] rather than another `MonitorStream`, so no `Box` is needed
/// — and transforms are `fn` pointers rather than boxed closures, so there is
/// no per-event dispatch allocation either.
///
/// # Surface
///
/// Deliberately exactly the two operations the server performs — `recv().await`
/// and [`try_recv`](Self::try_recv) (`tcp.rs` uses these and nothing else).
/// `try_recv` is what lets the RTEMS operation thread drain every monitor
/// without a reactor and park only when all are empty.
pub enum MonitorStream<T> {
    /// A source that pushes into a channel: the PVA gateway's fanout, the
    /// service framework, and every test source. Unchanged behaviour — this
    /// is what the trait used to return unconditionally.
    Channel(mpsc::Receiver<T>),
    /// A `SharedPV` monitor ring, consumed directly. Squash-to-tail
    /// (pvxs `servermon.cpp:283-286`) lives in the ring itself, so serving it
    /// through here preserves the exact queue semantics the bridge task
    /// forwarded.
    Ring(crate::server_native::shared_pv::MonitorRing<T>),
    /// A database/PV subscription owned by this stream, with the source's
    /// per-event transform applied on pull. Replaces the three
    /// `server/native_source` bridge tasks.
    Upstream(UpstreamMonitor<T>),
    /// A `PvField` producer served through an infallible per-item map. Only
    /// ever `PvField -> MonitorUpdate` ([`plain_monitor_updates`]).
    ///
    /// The inner producer is a [`PlainMonitor`], **not** another
    /// `MonitorStream`. That is what makes a mapped-mapped stream
    /// unrepresentable rather than merely unused: there is no variant to nest,
    /// so `recv` needs no recursion and therefore no `Box::pin` per call — the
    /// allocation this shape exists to avoid, since this is the default
    /// monitor path for every source without a `+trigger` graph.
    Mapped {
        inner: PlainMonitor,
        map: fn(PvField) -> T,
    },
}

/// A `PvField` monitor producer with no map applied — the inner half of
/// [`MonitorStream::Mapped`].
///
/// Deliberately a separate type rather than a reuse of `MonitorStream<PvField>`:
/// it holds exactly the producing variants and no `Mapped`, so "a map is applied
/// at most once" is a property of the type instead of a rule someone has to
/// remember.
///
/// The name is public because it appears in a public enum variant, but the
/// inner kind is private, so no caller outside this module can build one — and
/// therefore cannot build a [`MonitorStream::Mapped`] either. The single
/// construction site is `MonitorStream::map_plain`.
pub struct PlainMonitor(PlainMonitorKind);

enum PlainMonitorKind {
    Channel(mpsc::Receiver<PvField>),
    Ring(crate::server_native::shared_pv::MonitorRing<PvField>),
    Upstream(UpstreamMonitor<PvField>),
}

impl PlainMonitor {
    async fn recv(&mut self) -> Option<PvField> {
        match &mut self.0 {
            PlainMonitorKind::Channel(rx) => rx.recv().await,
            PlainMonitorKind::Ring(ring) => ring.recv().await,
            PlainMonitorKind::Upstream(up) => up.recv().await,
        }
    }

    fn try_recv(&mut self) -> Result<PvField, TryRecvError> {
        match &mut self.0 {
            PlainMonitorKind::Channel(rx) => rx.try_recv(),
            PlainMonitorKind::Ring(ring) => ring.try_recv(),
            PlainMonitorKind::Upstream(up) => up.try_recv(),
        }
    }
}

impl<T> MonitorStream<T> {
    /// Await the next update. `None` = the producer is gone and everything it
    /// queued has been delivered.
    pub async fn recv(&mut self) -> Option<T> {
        match self {
            Self::Channel(rx) => rx.recv().await,
            Self::Ring(ring) => ring.recv().await,
            Self::Upstream(up) => up.recv().await,
            Self::Mapped { inner, map } => inner.recv().await.map(*map),
        }
    }

    /// Non-blocking [`Self::recv`]. `Empty` = nothing right now but the
    /// producer is alive (park); `Disconnected` = terminal (tear down).
    ///
    /// Every variant bottoms out on a non-blocking primitive —
    /// `mpsc::Receiver::try_recv`, `MonitorRing::try_recv`, or
    /// `EventReader::try_recv` via the subscription — so a blocking drain
    /// loop can service any source shape without entering async.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        match self {
            Self::Channel(rx) => rx.try_recv(),
            Self::Ring(ring) => ring.try_recv(),
            Self::Upstream(up) => up.try_recv(),
            Self::Mapped { inner, map } => inner.try_recv().map(*map),
        }
    }
}

impl MonitorStream<PvField> {
    /// Serve this `PvField` stream as a `U` stream through `map`. The `fn`
    /// pointer (not a boxed closure) and the non-nesting [`PlainMonitor`] inner
    /// keep the result allocation-free per event.
    ///
    /// The `Mapped` arm below is unreachable by construction, not by luck: the
    /// only site that builds a `Mapped` is this function, and it always yields
    /// `MonitorStream<U>` for the caller's `U` — the port's single use is
    /// `U = MonitorUpdate`. A `MonitorStream<PvField>::Mapped` would require
    /// `map_plain::<PvField>`, which nothing calls.
    fn map_plain<U>(self, map: fn(PvField) -> U) -> MonitorStream<U> {
        let inner = match self {
            Self::Channel(rx) => PlainMonitor(PlainMonitorKind::Channel(rx)),
            Self::Ring(ring) => PlainMonitor(PlainMonitorKind::Ring(ring)),
            Self::Upstream(up) => PlainMonitor(PlainMonitorKind::Upstream(up)),
            Self::Mapped {
                inner,
                map: identity,
            } => {
                // Collapsing rather than nesting keeps the "at most one map"
                // property total: apply the existing map into a channel-free
                // producer is impossible without a closure, so this arm simply
                // cannot arise — see the doc comment above.
                let _ = identity;
                inner
            }
        };
        MonitorStream::Mapped { inner, map }
    }
}

/// A source that already produces `T` on a channel keeps working untouched —
/// this is what turns the ~60 existing `Some(rx)` returns into `Some(rx.into())`.
impl<T> From<mpsc::Receiver<T>> for MonitorStream<T> {
    fn from(rx: mpsc::Receiver<T>) -> Self {
        Self::Channel(rx)
    }
}

/// A database or process-variable subscription serving a PVA monitor
/// directly, with the source's per-event transform applied as the consumer
/// pulls.
///
/// Owning the subscription here is what deletes the three
/// `server/native_source` bridge tasks. It also preserves their two
/// behaviours that a blind deletion would have dropped:
///
/// * **the empty-mask filter** — `map` returns `None` for an event that marks
///   no leaf (C would have assigned none either), and both `recv` and
///   `try_recv` skip to the next event rather than reporting it. `try_recv`
///   must loop for the same reason `DbSubscription::try_next_event` does: a
///   filtered event is not "nothing available".
/// * **the initial seed** — `seed` is handed out once, ahead of the
///   subscription, reproducing the `tx.send(initial)` the `PvSubscription`
///   bridge performed before entering its loop.
///
/// Dropping this drops the subscription, whose own `Drop` unregisters the
/// subscriber slot — exactly what dropping the bridge task's captured
/// subscription did.
pub struct UpstreamMonitor<T> {
    upstream: UpstreamSub,
    /// `None` = this event marks nothing and is skipped (the bridge's
    /// `continue`). A plain `fn`, not a boxed closure: all three transforms
    /// are pure functions of the event.
    map: fn(epics_base_rs::server::pv::MonitorEvent) -> Option<T>,
    /// Connect-time value delivered before the subscription's own events.
    seed: Option<T>,
}

enum UpstreamSub {
    Db(epics_base_rs::server::database::db_access::DbSubscription),
    Pv(epics_base_rs::server::pv::PvSubscription),
}

/// `epics-base-rs`'s event queue defines its own `TryRecvError` mirror
/// (`event_queue.rs:527-535`) rather than depending on tokio's in a
/// signature. The orphan rule forbids a `From` impl here — both types are
/// foreign — so the two-variant mapping is spelled once, at the single seam
/// where an upstream subscription feeds a [`MonitorStream`].
fn from_queue_err(e: epics_base_rs::server::event_queue::TryRecvError) -> TryRecvError {
    match e {
        epics_base_rs::server::event_queue::TryRecvError::Empty => TryRecvError::Empty,
        epics_base_rs::server::event_queue::TryRecvError::Disconnected => {
            TryRecvError::Disconnected
        }
    }
}

impl<T> UpstreamMonitor<T> {
    /// Serve a PVA monitor straight off a record subscription.
    pub fn from_db(
        sub: epics_base_rs::server::database::db_access::DbSubscription,
        map: fn(epics_base_rs::server::pv::MonitorEvent) -> Option<T>,
    ) -> Self {
        Self {
            upstream: UpstreamSub::Db(sub),
            map,
            seed: None,
        }
    }

    /// Serve a PVA monitor straight off a `ProcessVariable` subscription.
    pub fn from_pv(
        sub: epics_base_rs::server::pv::PvSubscription,
        map: fn(epics_base_rs::server::pv::MonitorEvent) -> Option<T>,
    ) -> Self {
        Self {
            upstream: UpstreamSub::Pv(sub),
            map,
            seed: None,
        }
    }

    /// Deliver `value` once, before any subscription event.
    pub fn with_seed(mut self, value: T) -> Self {
        self.seed = Some(value);
        self
    }

    async fn recv(&mut self) -> Option<T> {
        if let Some(v) = self.seed.take() {
            return Some(v);
        }
        loop {
            let ev = match &mut self.upstream {
                UpstreamSub::Db(s) => s.recv_event().await?,
                UpstreamSub::Pv(s) => s.recv_event().await?,
            };
            if let Some(v) = (self.map)(ev) {
                return Some(v);
            }
        }
    }

    fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(v) = self.seed.take() {
            return Ok(v);
        }
        loop {
            let ev = match &mut self.upstream {
                UpstreamSub::Db(s) => s.try_recv_event().map_err(from_queue_err)?,
                UpstreamSub::Pv(s) => s.try_recv_event().map_err(from_queue_err)?,
            };
            if let Some(v) = (self.map)(ev) {
                return Ok(v);
            }
        }
    }
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

/// The single MONITOR seeding contract for [`ChannelSource`]. A
/// `subscribe_seeded` / `subscribe_raw_seeded` call returns exactly one
/// connect-time seed (`initial`) alongside the post-seed `updates`
/// stream. The server emits `initial` (if `Some`) as the MONITOR START
/// frame and then drains `updates`; it performs **no** independent
/// `get_value` seed, so a source cannot double-seed (the defect
/// [`ChannelSource::subscribe_seeded`] closes).
///
/// `updates` MUST carry only events that occur *after* the `initial`
/// snapshot. For sources whose initial value must be captured
/// atomically with subscriber registration (the PVA gateway shares one
/// upstream monitor and reads its cached `snapshot`), the seed is taken
/// inside the same critical section as the subscribe, which is why the
/// seed travels back with the stream rather than via a separate
/// `get_value` call the server would issue out of band.
///
/// `initial` is a decoded [`SourceRead`] even on the raw fast path: the
/// server always encodes the first frame through the regular encode
/// path (raw bodies may not be cached yet at START — see the raw seed
/// note in the server monitor task), so the seed value type is uniform
/// across cooked and raw subscriptions.
pub struct SubscriptionSeed<T> {
    /// The connect-time value to emit as the MONITOR START frame, or
    /// `None` when the source has no current value yet (e.g. an
    /// unopened `SharedPV` or a gateway entry awaiting its first
    /// upstream event) — the server then emits nothing until the first
    /// `updates` item.
    pub initial: Option<SourceRead>,
    /// Post-seed update stream. By contract this MUST NOT repeat
    /// `initial`.
    pub updates: MonitorStream<T>,
    /// Optional per-op MONITOR START/STOP gate. When the source backs
    /// this op with real upstream subscriptions it can no longer serve
    /// while the client has the monitor *stopped* (QSRV's per-record
    /// `DbSubscription`s), it returns a [`MonitorGate`] here. The wire
    /// layer drives it on this op's Executing↔Idle edge (the same edge
    /// that fires [`ChannelSource::notify_monitor_start`]) so the source
    /// disables those subscriptions on STOP/PAUSE and re-enables on
    /// START/RESUME — pvxs `MonitorControlOp::onStart` ⇒ `db_event_enable`
    /// / `db_event_disable` parity (`singlesource.cpp:151`). `None` (the
    /// default for sources that own no suspendable upstream — direct
    /// records, the fanout gateway) leaves the stream ungated.
    pub on_start: Option<MonitorGate>,
}

/// Per-op MONITOR START/STOP gate carried on a [`SubscriptionSeed`]. Wraps
/// a type-erased async setter the source supplies (e.g. QSRV toggling its
/// value+PROPERTY `DbSubscription`s); the wire layer's per-op edge owner
/// invokes [`Self::set_active`] with `true` on START/RESUME and `false` on
/// STOP/PAUSE. The setter is boxed (not a concrete handle) so this crate
/// stays free of any backend/record dependency.
#[derive(Clone)]
pub struct MonitorGate {
    set: Arc<
        dyn Fn(bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync,
    >,
}

impl MonitorGate {
    /// Build a gate from an async setter. `set(true)` resumes / `set(false)`
    /// suspends the source's backing event flow for this op.
    pub fn new<F, Fut>(set: F) -> Self
    where
        F: Fn(bool) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        Self {
            set: Arc::new(move |active| Box::pin(set(active))),
        }
    }

    /// Drive the gate: `active == true` resumes, `false` suspends.
    pub async fn set_active(&self, active: bool) {
        (self.set)(active).await
    }
}

impl std::fmt::Debug for MonitorGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorGate").finish_non_exhaustive()
    }
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
    /// Dyn forwarder for [`ChannelSource::searchable_from`].
    fn searchable_from<'a>(
        &'a self,
        name: &'a str,
        requester: SocketAddr,
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
    /// dyn forwarder for CREATE_CHANNEL owner resolution.
    fn resolve_owner<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<DynSource>> + Send + 'a>>;
    /// dyn forwarder for the channel's source-supplied report info.
    fn channel_report_info<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>>;
    fn get_value<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    /// Type-state-gated GET. Dyn forwarder.
    fn get_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PvField>> + Send + 'a>>;
    /// The framed read handoff (value + assigned leaves). Dyn forwarder.
    fn read_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SourceRead>> + Send + 'a>>;
    /// Per-source access gate. Dyn forwarder.
    fn access_gate(&self) -> &AccessGate;
    /// Source-registry beacon-change counter. Dyn forwarder.
    fn beacon_change(&self) -> u64;
    /// Monitor reload revalidation owner.
    fn revalidate_read<'a>(
        &'a self,
        pv_name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<AccessChecked>> + Send + 'a>>;
    fn put_value<'a>(
        &'a self,
        name: &'a str,
        value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    /// Dyn forwarder for type-state PUT.
    fn put_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    /// Dyn forwarder for type-state atomic BitSet-delta PUT.
    fn put_delta_checked<'a>(
        &'a self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &'a PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    /// Dyn forwarder for type-state atomic PUT_GET.
    fn put_get_checked<'a>(
        &'a self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &'a PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<SourceRead>, OpError>> + Send + 'a>,
    >;
    /// Dyn forwarder for ChannelArray INIT.
    fn channel_array_init<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FieldDesc, OpError>> + Send + 'a>>;
    /// Dyn forwarder for ChannelArray `getArray`.
    fn channel_array_get<'a>(
        &'a self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<PvField, OpError>> + Send + 'a>>;
    /// Dyn forwarder for ChannelArray `putArray`.
    fn channel_array_put<'a>(
        &'a self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    /// Dyn forwarder for ChannelArray `setLength`.
    fn channel_array_set_length<'a>(
        &'a self,
        checked: AccessChecked,
        length: u32,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    /// Dyn forwarder for ChannelArray `getLength`.
    fn channel_array_get_length<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u32, OpError>> + Send + 'a>>;
    fn is_writable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    /// Dyn forwarder for the MONITOR INIT pvRequest check
    /// ([`ChannelSource::check_monitor_request`]).
    fn check_monitor_request<'a>(
        &'a self,
        checked: &'a AccessChecked,
        ctx: &'a ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<PvField>>> + Send + 'a>,
    >;
    /// Dyn forwarder for type-state MONITOR.
    fn subscribe_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<PvField>>> + Send + 'a>,
    >;
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send + 'a>,
    >;
    /// Dyn forwarder for type-state raw MONITOR.
    fn subscribe_raw_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send + 'a>,
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
        Box<dyn std::future::Future<Output = Option<MonitorStream<PvField>>> + Send + 'a>,
    >;
    /// dyn forwarder for the cooked (`MonitorUpdate`) MONITOR
    /// with event-affecting options. The server's monitor dispatch uses this.
    fn subscribe_checked_opts_marked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<MonitorUpdate>>> + Send + 'a>,
    >;
    /// dyn forwarder for raw MONITOR with event-affecting options.
    fn subscribe_raw_checked_opts<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send + 'a>,
    >;
    /// dyn forwarder for the single-seed cooked MONITOR. The server's
    /// monitor START dispatch uses this.
    fn subscribe_seeded<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<SubscriptionSeed<MonitorUpdate>>> + Send + 'a>,
    >;
    /// dyn forwarder for the single-seed raw MONITOR fast path.
    fn subscribe_raw_seeded<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<SubscriptionSeed<RawMonitorEvent>>> + Send + 'a,
        >,
    >;
    fn rpc<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RpcReply, OpError>> + Send + 'a>>;
    /// Dyn forwarder for type-state RPC.
    fn rpc_checked<'a>(
        &'a self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RpcReply, OpError>> + Send + 'a>>;
    fn process<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    /// Dyn forwarder for type-state PROCESS.
    fn process_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>>;
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent);
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool);
    fn notify_channel_open(&self, name: &str, ctx: &ChannelContext);
    fn notify_channel_close(&self, name: &str, ctx: &ChannelContext);
    fn set_channel_invalidator(&self, invalidator: ChannelInvalidator);
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
    fn searchable_from<'a>(
        &'a self,
        name: &'a str,
        requester: SocketAddr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::searchable_from(
            self, name, requester,
        ))
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
    fn resolve_owner<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<DynSource>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::resolve_owner(self, name, ctx))
    }
    fn channel_report_info<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::channel_report_info(
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
    fn read_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SourceRead>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::read_checked(self, checked, ctx))
    }
    fn access_gate(&self) -> &AccessGate {
        <Self as ChannelSource>::access(self)
    }
    fn beacon_change(&self) -> u64 {
        <Self as ChannelSource>::beacon_change(self)
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value(self, name, value))
    }
    fn put_value_checked<'a>(
        &'a self,
        checked: AccessChecked,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_value_checked(
            self, checked, value, ctx,
        ))
    }
    fn put_delta_checked<'a>(
        &'a self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &'a PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::put_delta_checked(
            self, checked, desc, changed, delta, ctx,
        ))
    }
    fn put_get_checked<'a>(
        &'a self,
        checked: AccessChecked,
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &'a PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<SourceRead>, OpError>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::put_get_checked(
            self, checked, desc, changed, delta, ctx,
        ))
    }
    fn channel_array_init<'a>(
        &'a self,
        name: &'a str,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FieldDesc, OpError>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::channel_array_init(self, name, ctx))
    }
    fn channel_array_get<'a>(
        &'a self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<PvField, OpError>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::channel_array_get(
            self, checked, offset, count, stride, ctx,
        ))
    }
    fn channel_array_put<'a>(
        &'a self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::channel_array_put(
            self, checked, offset, stride, value, ctx,
        ))
    }
    fn channel_array_set_length<'a>(
        &'a self,
        checked: AccessChecked,
        length: u32,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::channel_array_set_length(
            self, checked, length, ctx,
        ))
    }
    fn channel_array_get_length<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u32, OpError>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::channel_array_get_length(
            self, checked, ctx,
        ))
    }
    fn is_writable<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::is_writable(self, name))
    }
    fn check_monitor_request<'a>(
        &'a self,
        checked: &'a AccessChecked,
        ctx: &'a ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::check_monitor_request(
            self, checked, ctx,
        ))
    }
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe(self, name))
    }
    fn subscribe_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<PvField>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_checked(
            self, checked, ctx,
        ))
    }
    fn subscribe_raw<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw(self, name))
    }
    fn subscribe_raw_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send + 'a>,
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
        Box<dyn std::future::Future<Output = Option<MonitorStream<PvField>>> + Send + 'a>,
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
        Box<dyn std::future::Future<Output = Option<MonitorStream<MonitorUpdate>>> + Send + 'a>,
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
        Box<dyn std::future::Future<Output = Option<MonitorStream<RawMonitorEvent>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw_checked_opts(
            self, checked, ctx, opts,
        ))
    }
    fn subscribe_seeded<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<SubscriptionSeed<MonitorUpdate>>> + Send + 'a>,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_seeded(
            self, checked, ctx, opts,
        ))
    }
    fn subscribe_raw_seeded<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
        opts: MonitorOptions,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<SubscriptionSeed<RawMonitorEvent>>> + Send + 'a,
        >,
    > {
        Box::pin(<Self as ChannelSource>::subscribe_raw_seeded(
            self, checked, ctx, opts,
        ))
    }
    fn rpc<'a>(
        &'a self,
        name: &'a str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RpcReply, OpError>> + Send + 'a>>
    {
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RpcReply, OpError>> + Send + 'a>>
    {
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::process(self, name))
    }
    fn process_checked<'a>(
        &'a self,
        checked: AccessChecked,
        ctx: ChannelContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OpError>> + Send + 'a>> {
        Box::pin(<Self as ChannelSource>::process_checked(self, checked, ctx))
    }
    fn notify_watermark(&self, name: &str, ctx: &ChannelContext, ev: WatermarkEvent) {
        <Self as ChannelSource>::notify_watermark(self, name, ctx, ev);
    }
    fn notify_monitor_start(&self, name: &str, ctx: &ChannelContext, start: bool) {
        <Self as ChannelSource>::notify_monitor_start(self, name, ctx, start);
    }
    fn notify_channel_open(&self, name: &str, ctx: &ChannelContext) {
        <Self as ChannelSource>::notify_channel_open(self, name, ctx);
    }
    fn notify_channel_close(&self, name: &str, ctx: &ChannelContext) {
        <Self as ChannelSource>::notify_channel_close(self, name, ctx);
    }
    fn set_channel_invalidator(&self, invalidator: ChannelInvalidator) {
        <Self as ChannelSource>::set_channel_invalidator(self, invalidator);
    }
    fn monitor_watermarks<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(usize, usize)>> + Send + 'a>>
    {
        Box::pin(<Self as ChannelSource>::monitor_watermarks(self, name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(names: &[&str]) -> Arc<[String]> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The channel invalidator must be lossless under backlog: a connection
    /// that has not yet drained must still see every published name, no matter
    /// how many a `:flush` produced. The pre-fix bounded `broadcast::<String>`
    /// (capacity 1024, one message per name) silently dropped names past the
    /// ring on a large flush; this unbounded per-connection queue cannot. Far
    /// exceed the old 1024 cap before draining, then assert nothing was lost.
    #[epics_macros_rs::epics_test]
    async fn invalidator_never_drops_under_backlog() {
        let inv = ChannelInvalidator::new();
        let mut rx = inv.subscribe();

        // 5000 single-name removals published with the receiver never polled —
        // 3976 past the old 1024-deep broadcast ring.
        const N: usize = 5000;
        for i in 0..N {
            inv.publish(batch(&[&format!("PV:{i}")]));
        }

        // Every one arrives, in order; none were dropped.
        for i in 0..N {
            let got = rx.try_recv().expect("no invalidation may be dropped");
            assert_eq!(got.to_vec(), vec![format!("PV:{i}")]);
        }
        assert!(rx.try_recv().is_err(), "exactly N batches, no more");
    }

    /// One removal command publishes its whole removed set as a single batch,
    /// regardless of how many entries it cleared — the per-name → snapshot
    /// shape change that lets a full-cache `:flush` ride in one message.
    #[epics_macros_rs::epics_test]
    async fn invalidator_delivers_one_batch_per_command() {
        let inv = ChannelInvalidator::new();
        let mut rx = inv.subscribe();

        inv.publish(batch(&["A", "B", "C"]));
        assert_eq!(
            rx.try_recv().expect("batch delivered").to_vec(),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        assert!(rx.try_recv().is_err(), "one command = one batch");
    }

    /// Fan-out reaches every live subscriber, and an empty batch is a no-op
    /// (a `:flush` of an empty cache publishes nothing).
    #[epics_macros_rs::epics_test]
    async fn invalidator_fans_out_and_skips_empty() {
        let inv = ChannelInvalidator::new();
        let mut a = inv.subscribe();
        let mut b = inv.subscribe();

        inv.publish(batch(&[])); // empty: no-op
        inv.publish(batch(&["X"]));

        assert_eq!(
            a.try_recv().expect("a sees X").to_vec(),
            vec!["X".to_string()]
        );
        assert_eq!(
            b.try_recv().expect("b sees X").to_vec(),
            vec!["X".to_string()]
        );
        assert!(a.try_recv().is_err(), "empty batch was not delivered");
        assert!(b.try_recv().is_err(), "empty batch was not delivered");
    }

    /// A subscriber whose receiver was dropped is pruned on the next publish,
    /// so a high connection-churn workload does not grow the registry. The
    /// surviving subscriber keeps receiving.
    #[epics_macros_rs::epics_test]
    async fn invalidator_prunes_dropped_subscribers() {
        let inv = ChannelInvalidator::new();
        let dead = inv.subscribe();
        let mut live = inv.subscribe();
        drop(dead);

        // Publish prunes the dead sender (its send fails) but still reaches the
        // live one.
        inv.publish(batch(&["Y"]));
        assert_eq!(
            live.try_recv().expect("live sees Y").to_vec(),
            vec!["Y".to_string()]
        );
    }
}
