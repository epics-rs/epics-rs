use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::sync::{mpsc, oneshot};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::channel::AccessRights;
use crate::client::state::ChannelState;

// --- Virtual-circuit identity ---

/// Identity of one CA virtual circuit: the server address paired with
/// the CA priority the channel was created at. libca keys its circuit
/// table (`caServerID`, `caServerID.h:28-38`) on exactly this pair —
/// `(sockaddr_in, ca_uint8_t pri)` — so two channels to the same IOC at
/// different priorities open independent TCP circuits. The Rust client
/// mirrors that: every map that used to be keyed by `SocketAddr`
/// (`connections`, `DirectServerWriters`, `ServerLastRxAt`, the
/// coordinator's `server_channels`, the per-circuit flow-control state)
/// is keyed by `CircuitKey`, and every `TransportCommand` carries the
/// `priority` so the transport manager can route to the right circuit.
///
/// `priority` is the libca CA priority (`0..=99`, `cacChannel::priorityMax`);
/// `0` is `priorityDefault` and reproduces the historical single-circuit
/// behaviour.
pub(crate) type CircuitKey = (SocketAddr, u8);

// --- Per-circuit last-RX timestamp sidecar (Option C, Phase D) ---

/// Last instant a frame was received from each server. Bumped by the
/// per-server transport `read_loop` whenever any TCP frame arrives —
/// covers `READ_NOTIFY`, `WRITE_NOTIFY`, `EVENT_ADD`, `ACCESS_RIGHTS`,
/// `CREATE_CH_RESP`, version negotiation, echoes, etc.
///
/// Phase A turned read/write responses into a transport-direct path
/// that never reaches the coordinator, so the coordinator can no
/// longer maintain this stamp from `TransportEvent`s alone — a
/// read-heavy client without monitors would look idle even though
/// frames are arriving every millisecond. The sidecar lets the
/// transport keep the stamp current and the coordinator (which is
/// the one answering `ca_receive_watchdog_delay`) read it directly.
///
/// keyed by [`CircuitKey`] so each priority circuit to one
/// server keeps an independent receive timestamp — libca's
/// `tcpRecvWatchdog` is per-circuit.
pub(crate) type ServerLastRxAt = Arc<DashMap<CircuitKey, Instant>>;

// --- Direct per-server writer sidecar (Option C, Phase E) ---

/// Send buffer backpressure threshold (matches C EPICS flushBlockThreshold).
/// If more than this many frames are pending, the connection is stalled.
pub(crate) const SEND_BACKPRESSURE_FRAMES: usize = 4096;

/// Cloneable write handle for an established virtual circuit.
///
/// Hot one-shot operations (`CaChannel::get` / `put`) use this sidecar
/// to enqueue frames straight to the per-server writer task, bypassing
/// the transport manager actor after the channel has already reached
/// `Operational`. Lifecycle operations still go through the transport
/// manager so connection setup/teardown remains centralized.
#[derive(Clone)]
pub(crate) struct DirectServerWriter {
    pub(crate) write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub(crate) pending_frames: Arc<AtomicUsize>,
}

impl DirectServerWriter {
    pub(crate) fn send_frame(&self, frame: Vec<u8>) -> CaResult<()> {
        let pending = self.pending_frames.load(Ordering::Relaxed);
        if pending >= SEND_BACKPRESSURE_FRAMES {
            return Err(CaError::Disconnected);
        }

        self.pending_frames.fetch_add(1, Ordering::Relaxed);
        if self.write_tx.send(frame).is_err() {
            // same accounting fix as write_loop — use atomic
            // CAS instead of load + store so a concurrent
            // `send_frame` increment cannot be silently overwritten.
            // The send-failure rollback decrements exactly one frame
            // and saturates at zero (a concurrent write_loop drain
            // may already have driven the counter below 1).
            let mut current = self.pending_frames.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_sub(1);
                match self.pending_frames.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
            return Err(CaError::Disconnected);
        }
        Ok(())
    }
}

/// Shared server-writer registry. Transport manager publishes; channel hot
/// paths read.
///
/// keyed by [`CircuitKey`]. A channel's hot path looks up its
/// writer by `(server_addr, priority)`, so two priorities to one server
/// write to their own circuits.
pub(crate) type DirectServerWriters = Arc<DashMap<CircuitKey, DirectServerWriter>>;

// --- Channel snapshot sidecar (Option C, Phase B) ---

/// Immutable, per-channel snapshot published by the coordinator
/// whenever lifecycle state changes. CaChannel hot paths
/// (`ch.get` / `ch.put` / `ch.subscribe`) read from this map
/// directly instead of round-tripping `CoordRequest::GetChannelInfo`
/// for every operation.
///
/// The coordinator inserts/updates an entry on every relevant
/// `TransportEvent` (ChannelCreated, AccessRightsChanged, …) and
/// removes it on Drop. Stale-read window: a tiny race exists where
/// a CaChannel sees an old snapshot for one nanosecond after a
/// state change; that's acceptable because the request will either
/// fail at the server (which already knows the new state) or get
/// retried on disconnect drain.
#[derive(Clone)]
pub(crate) struct ChannelSnapshotPublic {
    pub sid: u32,
    pub native_type: DbFieldType,
    pub element_count: u32,
    pub server_addr: SocketAddr,
    pub access_rights: AccessRights,
    pub state: ChannelState,
    /// Minor protocol version of the server owning this channel's
    /// circuit, as announced by its `CA_PROTO_VERSION` frame. Every
    /// request framed for this channel needs it: libca's
    /// `comQueSend::insertRequestHeader` takes `bool v49Ok` from
    /// `CA_V49 ( minorProtocolVersion )` and only emits the extended
    /// (24-byte) header when the peer speaks V49 — otherwise it throws
    /// `cacChannel::outOfBounds` (`comQueSend.cpp:285-363`).
    /// 0 until the circuit's VERSION frame is seen, i.e. pre-V49 —
    /// the same conservative starting point as C's `tcpiiu`.
    pub server_minor: u16,
}

/// Shared snapshot registry. Coordinator publishes; CaChannel reads.
pub(crate) type ChannelSnapshots = Arc<DashMap<u32, ChannelSnapshotPublic>>;

/// Per-channel SEARCH attempt counter (CA-035 `ca_search_attempts`).
/// SearchEngine bumps on every fanout call (immediate first SEARCH
/// after Schedule + each bucket-tick retransmit); one bump per
/// fanout regardless of how many UDP datagrams the addr_list /
/// nameserver duplication produces. CaChannel surfaces it via
/// [`super::CaChannel::search_attempts`]. Entry is removed when
/// the channel is cancelled or its connection succeeds (matching
/// libca, which resets attempts on circuit creation).
pub(crate) type SearchAttempts = Arc<DashMap<u32, std::sync::atomic::AtomicU32>>;

// --- CA-130 ca_add_exception_event ----------------------------------

/// Out-of-band / unrecoverable error categories surfaced via the
/// per-client exception handler. Mirrors the C `caEventHandlerArgs`
/// `op` field — but typed instead of a magic-number enum.
///
/// Variants are added when a real dispatch site exists. Adding a
/// variant without a live source would create a dead API; clients
/// would `match` on it and never receive the case.
///
/// `#[non_exhaustive]` so future variants (e.g. BeaconAnomaly when
/// that path gets a real source) can be added without breaking
/// downstream `match` blocks. Clients must include a `_ => …` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaExceptionKind {
    /// Server emitted a `CA_PROTO_ERROR` (cmd=11) with a status code
    /// for an operation that wasn't otherwise routed to a callback.
    /// `status` carries the ECA code, `message` the optional payload.
    ServerError,
    /// Server-initiated channel close (`CA_PROTO_SERVER_DISCONN`).
    /// Per-op waiters tied to the channel are released with
    /// `Disconnected`; the handler additionally fires for callers
    /// who want a global notification stream.
    ServerDisconnect,
}

/// The operation an exception is about — libca `CA_OP_*` (`cadef.h:150-160`).
/// The numeric code is what the default handler prints as `op=%u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaOp {
    Get,
    Put,
    CreateChannel,
    AddEvent,
    ClearEvent,
    /// libca `CA_OP_OTHER` — anything the exception table routes through
    /// `cac::defaultExcep`.
    Other,
}

impl CaOp {
    /// The `CA_OP_*` constant (`cadef.h`).
    pub fn code(self) -> u32 {
        match self {
            CaOp::Get => 0,
            CaOp::Put => 1,
            CaOp::CreateChannel => 2,
            CaOp::AddEvent => 3,
            CaOp::ClearEvent => 4,
            CaOp::Other => 5,
        }
    }
}

/// Single OOB-error notification delivered to a registered handler.
///
/// `#[non_exhaustive]` so additional context fields (e.g. timestamp,
/// retry-attempt count) can be added without breaking downstream
/// struct-literal construction. Construct via mutating an instance
/// from the public API or use functional update on a constructed
/// value; do not literal-init from the outside.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CaException {
    pub kind: CaExceptionKind,
    pub message: String,
    pub server_addr: Option<SocketAddr>,
    pub pv_name: Option<String>,
    /// ECA status code when applicable (server-error path).
    pub status: Option<u32>,
    /// Which operation failed. libca puts this in `exception_handler_args.op`
    /// and prints it as `op=%u` in the default block.
    pub op: CaOp,
    /// DBR type of the failed request, echoed back inside the
    /// `CA_PROTO_ERROR` payload. `None` when the error carried no request
    /// echo.
    pub data_type: Option<u16>,
    /// Element count of the failed request, same source as `data_type`.
    pub count: Option<u32>,
    /// Which libca site raised this exception. See [`ExceptionSite`] — it is
    /// not an `Option`, because "no `Source File:` line" is a claim about C
    /// (that the producer passes a null file), not a field a new call site may
    /// leave unfilled. Three sites did leave it unfilled, and each dropped a
    /// line C prints.
    pub source: ExceptionSite,
}

/// The `Source File: <file> line <n>` line of a `CA.Client.Exception` block.
///
/// libca's `genLocalExcep` macro (`iocinf.h:67`) *always* passes
/// `__FILE__`/`__LINE__`, and `ca_client_context::vSignal`
/// (`ca_client_context.cpp:388-391`) prints the line whenever the file is
/// non-null — so carrying the site is the rule and omitting it is the
/// exception. Modelling that as `Option<…>` made omission the cheap default:
/// the ECA_UNRESPTMO, ECA_DBLCHNL and circuit-disconnect blocks all shipped
/// without the line. Naming the null-file case after its one C producer means
/// a new site cannot omit the line without claiming, in the type, that C omits
/// it too.
///
/// The paths are C's verbatim (`../cac.cpp`, not a Rust path): this block is a
/// user-facing diagnostic that operators grep, and its text is C's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionSite {
    /// `__FILE__` / `__LINE__` of the raising site — printed.
    At(&'static str, u32),
    /// `cac::defaultExcep` (`cac.cpp:1006-1016`) passes a null file, which
    /// suppresses the line. The only libca producer that does.
    NullFile,
}

/// The peer's display name — C `tcpiiu::getHostName` (`hostNameCache.cpp`),
/// which every exception context and `cainfo`'s `Host:` line prints.
///
/// One owner for the whole client, so "how a peer is named" has a single
/// spelling regardless of which half of the crate is compiled. With the
/// reverse-DNS cache absent (`client-core`, where `hostname` is not built —
/// `getnameinfo` has no newlib backing) this returns exactly what C returns
/// for an address with no PTR record: the dotted address with its port
/// (`ipAddrToA`, `osiSock.c:129`). A caller can therefore never tell the two
/// configurations apart by the *shape* of what it gets back.
pub(crate) fn peer_display_name(addr: SocketAddr) -> String {
    #[cfg(feature = "client")]
    {
        crate::hostname::cached_name(addr)
    }
    #[cfg(not(feature = "client"))]
    {
        addr.to_string()
    }
}

/// The blocking half of [`peer_display_name`] — C `ipAddrToA` proper, which
/// waits for the resolver instead of reading the cache
/// (`CaChannel::host_name`, libca `ca_host_name`).
///
/// Also the one place `tokio::task::spawn_blocking` is reached from the
/// client, which is why the `client-core` arm is not merely "no DNS": there
/// is no tokio runtime on the RTEMS target to hand a blocking pool task to,
/// so that call must not exist there at all.
pub(crate) async fn peer_resolved_name(addr: SocketAddr) -> String {
    #[cfg(feature = "client")]
    {
        tokio::task::spawn_blocking(move || crate::hostname::ip_addr_to_a(addr))
            .await
            // A join failure is a runtime shutdown, not a channel error, and
            // C's `ipAddrToA` has a well-defined answer for "no name": the
            // dotted IP. Give that rather than failing a connected channel.
            .unwrap_or_else(|_| addr.to_string())
    }
    #[cfg(not(feature = "client"))]
    {
        addr.to_string()
    }
}

/// libca `oldChannelNotify::writeException` (`oldChannelNotify.cpp:158-159`) —
/// the site a server-rejected **plain** `CA_PROTO_WRITE` reaches, since
/// `cac::writeExcep` (`cac.cpp:1049-1061`) looks the channel up and there is no
/// per-op callback to complete. This is what a C `caput` prints under
/// `Source File:`.
pub(crate) const LIBCA_WRITE_EXCEPTION_SITE: ExceptionSite =
    ExceptionSite::At("../oldChannelNotify.cpp", 159);

/// libca `cac::destroyIIU` (`cac.cpp:1236-1240`) — the `genLocalExcep` a
/// virtual circuit raises when it dies with channels still on it.
pub(crate) const LIBCA_CIRCUIT_DISCONNECT_SITE: ExceptionSite =
    ExceptionSite::At("../cac.cpp", 1240);

/// libca `tcpiiu::unresponsiveCircuitNotify` (`tcpiiu.cpp:925`) — the
/// `genLocalExcep(ECA_UNRESPTMO, hostNameTmp)` an echo timeout raises. The
/// line is the one the *invocation opens on*, which is what GCC's `__LINE__`
/// yields for a macro spanning `tcpiiu.cpp:925-926` (verified against gcc).
pub(crate) const LIBCA_UNRESPONSIVE_SITE: ExceptionSite = ExceptionSite::At("../tcpiiu.cpp", 925);

/// libca `cac::pvMultiplyDefinedNotify` (`cac.cpp:1323`) — a direct
/// `this->exception(…, __FILE__, __LINE__)` call, so the line is its own.
pub(crate) const LIBCA_MULTIPLY_DEFINED_SITE: ExceptionSite = ExceptionSite::At("../cac.cpp", 1323);

/// The ECA_DISCONN block C prints when a circuit carrying channels dies.
/// Verified byte-for-byte against compiled `camonitor` (7.0.10.1-DEV) with
/// the IOC killed under it:
///
/// ```text
/// CA.Client.Exception...............................................
///     Warning: "Virtual circuit disconnect"
///     Context: "localhost:15064"
///     Source File: ../cac.cpp line 1240
///     Current Time: Mon Jul 13 2026 20:57:51.950625115
/// ..................................................................
/// ```
///
/// The context is the *resolved* peer name (C `tcpiiu::getHostName`), not
/// the dotted address, and it is the whole context — this is the plain
/// (non-channel) `ca_client_context::exception` overload, so `data_type`
/// stays `None` and the renderer prints `message` verbatim.
pub(crate) fn circuit_disconnect_exception(server_addr: SocketAddr) -> CaException {
    CaException {
        kind: CaExceptionKind::ServerError,
        message: peer_display_name(server_addr),
        server_addr: Some(server_addr),
        pv_name: None,
        status: Some(crate::protocol::ECA_DISCONN),
        op: CaOp::Other,
        data_type: None,
        count: None,
        source: LIBCA_CIRCUIT_DISCONNECT_SITE,
    }
}

/// The ECA_UNRESPTMO block: an echo timeout on a circuit that still has
/// connected channels (C `tcpiiu::unresponsiveCircuitNotify`,
/// `tcpiiu.cpp:922-926`).
///
/// The context is `getHostName()` — the circuit's *resolved* peer name and
/// nothing else. The port used to invent a sentence of its own
/// (`"circuit unresponsive: 127.0.0.1:15064 (matches libca ECA_UNRESPTMO)"`),
/// which is not a string any C tool prints, and dropped the `Source File:`
/// line that `genLocalExcep` always produces.
pub(crate) fn unresponsive_circuit_exception(server_addr: SocketAddr) -> CaException {
    CaException {
        kind: CaExceptionKind::ServerError,
        message: peer_display_name(server_addr),
        server_addr: Some(server_addr),
        pv_name: None,
        status: Some(crate::protocol::ECA_UNRESPTMO),
        op: CaOp::Other,
        data_type: None,
        count: None,
        source: LIBCA_UNRESPONSIVE_SITE,
    }
}

/// The ECA_DBLCHNL block: the same PV answered from two servers
/// (C `cac::pvMultiplyDefinedNotify`, `cac.cpp:1314-1323`). C builds the
/// context with one `epicsSnprintf` and raises it through the plain (non-
/// channel) overload, so the text prints verbatim — but it goes through
/// `exception(…, __FILE__, __LINE__)`, so the `Source File:` line is there.
pub(crate) fn multiply_defined_pv_exception(
    pv_name: String,
    accepted: SocketAddr,
    ignored: SocketAddr,
) -> CaException {
    CaException {
        kind: CaExceptionKind::ServerError,
        message: format!("Channel: \"{pv_name}\", Connecting to: {accepted}, Ignored: {ignored}"),
        server_addr: Some(ignored),
        pv_name: Some(pv_name),
        status: Some(crate::protocol::ECA_DBLCHNL),
        op: CaOp::Other,
        data_type: None,
        count: None,
        source: LIBCA_MULTIPLY_DEFINED_SITE,
    }
}

/// Boxed handler. Returns `()`; logs are emitted regardless so a
/// handler that panics or is slow can't suppress the existing
/// tracing diagnostics.
pub type CaExceptionHandler = Arc<dyn Fn(&CaException) + Send + Sync>;

/// Shared slot for the per-client handler. `parking_lot::RwLock`
/// keeps the read path lock-free in the common (no handler set)
/// case after the first install. One slot per CaClient instance —
/// not a process-global singleton.
pub(crate) type CaExceptionSlot = Arc<parking_lot::RwLock<Option<CaExceptionHandler>>>;

/// Best-effort dispatch — never panics, even if the handler does.
///
/// The slot is the ONLY gate: an exception either reaches the registered
/// handler or the C-parity default one ([`print_default_exception`]). libca
/// works the same way (`ca_client_context::exception`, `ca_client_context.cpp`
/// :289-349: `if (pFunc) (*pFunc)(args); else this->signal(...)`), so a client
/// that installs no handler still sees the `CA.Client.Exception` block. Rust
/// used to drop the no-handler case on the floor, which is why a
/// server-rejected `caput` printed nothing at all (R13-24).
pub(crate) fn dispatch_exception(slot: &CaExceptionSlot, exc: CaException) {
    let handler = slot.read().clone();
    match handler {
        // Catch panics so a buggy handler doesn't poison the
        // dispatching task. We can't recover the handler's bug but
        // we can keep the rest of the client functional.
        Some(h) => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&exc)));
        }
        None => print_default_exception(&exc),
    }
}

/// libca's default exception handler: `ca_client_context::vSignal`
/// (`ca_client_context.cpp:361-417`). Writes to stderr, in one `write` so two
/// concurrent exceptions cannot interleave mid-block:
///
/// ```text
/// CA.Client.Exception...............................................
///     Warning: "Channel write request failed"
///     Context: "op=1, channel=TST:LO, type=DBR_STRING, count=1, ctx="TST:LO""
///     Source File: ../oldChannelNotify.cpp line 159
///     Current Time: Mon Jul 13 2026 09:16:02.135621071
/// ..................................................................
/// ```
///
/// DEVIATION, deliberate: C ends `vSignal` with `abort()` for any status that
/// is neither a success nor a `CA_K_WARNING` — a library aborting the host
/// process on a server-side error is not something this port reproduces. We
/// always print the closing rule and return.
pub(crate) fn print_default_exception(exc: &CaException) {
    use std::io::Write as _;

    if let Some(block) = render_default_exception(exc, &exception_timestamp()) {
        // One `write_all`: two exceptions racing must not interleave mid-block.
        let _ = std::io::stderr().write_all(block.as_bytes());
    }
}

/// The block itself, split out from the stderr write so the exact C text is
/// testable. `None` means C prints nothing for this exception.
pub(crate) fn render_default_exception(exc: &CaException, now: &str) -> Option<String> {
    use std::fmt::Write as _;

    // `CA_PROTO_SERVER_DISCONN` never reaches libca's `vSignal`: it goes to
    // `cac::verifyAndDisconnectChan` and out through the channel's connection
    // callback, so C prints nothing. The [`CaExceptionKind::ServerDisconnect`]
    // notification is a port extension for library users who want a global
    // stream, and printing it would put blocks on a C tool's stderr that C
    // does not print.
    if exc.kind == CaExceptionKind::ServerDisconnect {
        return None;
    }

    // `ca_client_context.cpp:365-375`, indexed by CA_EXTRACT_SEVERITY.
    const SEVERITY: [&str; 8] = [
        "Warning", "Success", "Error", "Info", "Fatal", "Fatal", "Fatal", "Fatal",
    ];

    let status = exc.status.unwrap_or(crate::protocol::ECA_INTERNAL);
    let severity = SEVERITY[(crate::protocol::eca_severity(status) & 0x7) as usize];

    let mut block = String::with_capacity(320);
    block.push_str("CA.Client.Exception...............................................\n");
    let _ = writeln!(
        block,
        "    {severity}: \"{}\"",
        crate::protocol::eca_message(status)
    );
    let _ = writeln!(block, "    Context: \"{}\"", exception_context(exc));
    if let ExceptionSite::At(file, line) = exc.source {
        let _ = writeln!(block, "    Source File: {file} line {line}");
    }
    let _ = writeln!(block, "    Current Time: {now}");
    block.push_str("..................................................................\n");
    Some(block)
}

/// The `Context:` payload. libca has exactly two shapes, one per
/// `ca_client_context::exception` overload:
///
/// * channel-scoped (`ca_client_context.cpp:317-349`) — the args carry a
///   chid/type/count, and `signal` renders
///   `op=%u, channel=%s, type=%s, count=%lu, ctx="%s"`;
/// * plain (`ca_client_context.cpp:289-315`) — the ctx text is printed as-is.
///   Producers that want more in it put it there themselves, exactly as
///   `cac::defaultExcep` does with its `host=%s ctx=%.400s`.
///
/// `data_type` is the discriminator because it is set only where libca has a
/// `caHdrLargeArray` to take it from — a channel exception.
fn exception_context(exc: &CaException) -> String {
    match (&exc.pv_name, exc.data_type) {
        (Some(pv), Some(dbr)) => format!(
            "op={}, channel={pv}, type={}, count={}, ctx=\"{}\"",
            exc.op.code(),
            epics_base_rs::types::dbr_type_to_text(dbr),
            exc.count.unwrap_or(0),
            exc.message,
        ),
        _ => exc.message.clone(),
    }
}

/// C `epicsTime::strftime("%a %b %d %Y %H:%M:%S.%f")` — local time, and `%f`
/// is nine digits of nanoseconds (`epicsTime.cpp:243-262`).
fn exception_timestamp() -> String {
    let now = chrono::Local::now();
    format!(
        "{}.{:09}",
        now.format("%a %b %d %Y %H:%M:%S"),
        now.timestamp_subsec_nanos()
    )
}

// --- Client identity (user / host advertised on circuit handshakes) ---

/// User and host names advertised to servers on every CA virtual-circuit
/// handshake (`CA_PROTO_CLIENT_NAME` / `CA_PROTO_HOST_NAME`).
///
/// libca resolves these once at context creation — user from `$USER`
/// (then `$USERNAME` on Windows), host from the local hostname — and the
/// `tcpiiu` constructor (`tcpiiu.cpp:755-762`) queues them on every new
/// circuit. This port keeps them in a shared slot instead so the names
/// can be overridden at runtime; the handshake builder reads the slot
/// rather than the environment.
#[derive(Clone, Debug)]
pub(crate) struct ClientIdentity {
    pub(crate) user: String,
    pub(crate) host: String,
}

impl ClientIdentity {
    /// Resolve the identity from the environment the way libca does:
    /// user from `$USER` falling back to `$USERNAME`, host from the
    /// local hostname.
    pub(crate) fn from_env() -> Self {
        let user = epics_base_rs::runtime::env::get("USER")
            .or_else(|| epics_base_rs::runtime::env::get("USERNAME"))
            .unwrap_or_else(|| "unknown".to_string());
        let host = epics_base_rs::runtime::env::hostname();
        Self { user, host }
    }
}

/// Shared, runtime-mutable [`ClientIdentity`]. Owned by the `CaClient`;
/// cloned into the transport manager so every new circuit handshake
/// reads the current value, and mutated by `CaClient::set_user_name` /
/// `set_host_name`.
pub(crate) type ClientIdentitySlot = Arc<parking_lot::RwLock<ClientIdentity>>;

// --- Direct in-flight op registries (Option C) ---

pub(crate) enum ReadReply {
    Plain {
        dbr_type: DbFieldType,
        value: EpicsValue,
    },
    Raw {
        data_type: u16,
        count: u32,
        data: Vec<u8>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum ReadReplyMode {
    Plain,
    Raw,
}

/// Reply channel type for reads.
pub(crate) type ReadReplyTx = oneshot::Sender<CaResult<ReadReply>>;
/// Reply channel type for one-shot writes (write-notify completion).
pub(crate) type WriteReplyTx = oneshot::Sender<CaResult<()>>;

/// Reusable Sender slot used by `ReadWaiter::Warm` — the channel-side
/// caller refills it before each call, the dispatcher takes it on
/// response. Wrapped in a `parking_lot::Mutex` because both sides hold
/// the lock for nanoseconds (just `take`/`replace`).
pub(crate) type WarmReplySlot = Arc<parking_lot::Mutex<Option<ReadReplyTx>>>;

pub(crate) enum ReadWaiter {
    OneShot {
        cid: u32,
        mode: ReadReplyMode,
        reply_tx: ReadReplyTx,
    },
    /// Persistent waiter installed by `CachedRead`. Same ioid stays in
    /// the registry across calls so subsequent reads skip
    /// `alloc_ioid` + DashMap insert/remove. The dispatcher takes the
    /// `Sender` from `slot` on response without removing the entry; the
    /// channel-side caller refills `slot` before each frame send. See
    /// `transport::dispatch_read_reply_with` and
    /// `client::CaChannel::cached_read` for the full lifecycle.
    Warm {
        cid: u32,
        mode: ReadReplyMode,
        slot: WarmReplySlot,
    },
}

impl ReadWaiter {
    pub(crate) fn cid(&self) -> u32 {
        match self {
            Self::OneShot { cid, .. } => *cid,
            Self::Warm { cid, .. } => *cid,
        }
    }

    pub(crate) fn mode(&self) -> ReadReplyMode {
        match self {
            Self::OneShot { mode, .. } => *mode,
            Self::Warm { mode, .. } => *mode,
        }
    }

    /// Consume the waiter and signal `result`. Used by the disconnect
    /// drain path (`drain_waiters_for_cids`) where we want both
    /// `OneShot` and `Warm` waiters notified-and-evicted.
    pub(crate) fn send(self, result: CaResult<ReadReply>) {
        match self {
            Self::OneShot { reply_tx, .. } => {
                let _ = reply_tx.send(result);
            }
            Self::Warm { slot, .. } => {
                if let Some(tx) = slot.lock().take() {
                    let _ = tx.send(result);
                }
            }
        }
    }
}

/// Shared in-flight op registry. Channel handles insert reply oneshots
/// here keyed by `ioid`; the per-server transport read loop removes
/// and fulfils them on `ReadResponse` / `WriteResponse` arrival.
///
/// This replaces the previous design where every read/write went
/// through the coordinator's `tokio::select!` loop twice (once on
/// op submission to register the waiter, once on response to dispatch
/// it). With ~25 µs of coordinator-iteration overhead on each touch,
/// `bulk_caget(20)` showed ~1.8 ms wall time in benchmarks against a
/// localhost IOC despite the 20 spawned tasks all "running in
/// parallel". Routing reads/writes directly here removes both
/// touches; the coordinator only sees the lifecycle path
/// (`RegisterChannel`, search-found, TCP close, beacon anomaly).
///
/// The `cid` field stored alongside each reply lets the disconnect-
/// cleanup path filter pending ops by channel when a server's
/// virtual circuit dies (Phase D).
#[derive(Clone)]
pub(crate) struct InFlightOps {
    pub(crate) reads: Arc<DashMap<u32, ReadWaiter>>,
    pub(crate) writes: Arc<DashMap<u32, (u32, WriteReplyTx)>>,
    /// `ioid -> (cid, subid)` for the `READ_NOTIFY`s issued by the
    /// circuit-recovery re-subscribe (C `tcpiiu::subscriptionUpdateRequest`,
    /// `tcpiiu.cpp:1610-1643`). C sends that request under the
    /// subscription's *own* id, so its reply lands on the subscription's
    /// callback rather than on a get waiter. The port keeps the `ioid` and
    /// `subid` spaces separate, so the reply's owner is recorded here and
    /// the response dispatcher consults this map first.
    pub(crate) sub_updates: Arc<DashMap<u32, (u32, u32)>>,
    /// `cid -> pv_name`, written by the issuer of a fire-and-forget
    /// `CA_PROTO_WRITE` and read by the circuit's ERROR decoder.
    ///
    /// A plain write carries no `ioid` and completes nothing, so unlike
    /// every other request it leaves no per-op record — which is why the
    /// exception path used to recover the channel name by looking the
    /// error's cid up in the coordinator's live `channels` map. That
    /// lookup answers "is this channel still alive", not "what was this
    /// request issued against", and a `CA_PROTO_ERROR` that overtakes the
    /// channel's own `DropChannel` therefore printed a context with no
    /// channel in it. libca has the record because the name lives on the
    /// `nciu` the write was issued through (`cac::writeExcep` →
    /// `chanTable.lookup(hdr.m_available)`, `cac.cpp:1050-1061`, the
    /// ECHOED request's cid); this map is that record.
    ///
    /// Entries outlive the channel on purpose. They are removed when the
    /// server can no longer answer for the cid: the `CA_PROTO_CLEAR_CHANNEL`
    /// confirmation (`rsrv/camessage.c:1944-1957` echoes `m_cid`/
    /// `m_available`, and a circuit answers in order, so it is a fence and
    /// not a delay), `CA_PROTO_SERVER_DISCONN`, or a `DropChannel` that
    /// sent no CLEAR_CHANNEL because the channel was not operational.
    pub(crate) write_identities: Arc<DashMap<u32, Arc<str>>>,
    /// monotonic `ioid` source owned by the same registry
    /// that holds the live ids. Keeping the counter here (rather than
    /// a process-global static) lets [`Self::alloc_ioid`] probe
    /// `reads`/`writes` so a counter that wraps through 2^32 cannot
    /// reissue an id whose read/write is still pending. Shared across
    /// `InFlightOps` clones (it is `Arc`), so the coordinator and all
    /// channels of one client draw from a single id space.
    next_ioid: Arc<AtomicU32>,
}

impl InFlightOps {
    pub(crate) fn new() -> Self {
        Self {
            reads: Arc::new(DashMap::new()),
            writes: Arc::new(DashMap::new()),
            sub_updates: Arc::new(DashMap::new()),
            write_identities: Arc::new(DashMap::new()),
            next_ioid: Arc::new(AtomicU32::new(1)),
        }
    }

    /// Allocate an `ioid` that is not currently live in either
    /// in-flight table. the monotonic counter alone can wrap
    /// onto an id whose operation is still pending (≈11.9 h at 100k
    /// ops/s); a late response for the stale op would then wake the
    /// wrong waiter. Probing `reads`/`writes` skips any live id. Two
    /// concurrent allocations never collide because the counter is
    /// monotonic, so the probe only guards against prior-wrap
    /// survivors.
    pub(crate) fn alloc_ioid(&self) -> u32 {
        crate::channel::alloc_nonzero_probe(&self.next_ioid, |v| {
            self.reads.contains_key(&v)
                || self.writes.contains_key(&v)
                || self.sub_updates.contains_key(&v)
        })
    }

    /// Register the reply owner of a circuit-recovery subscription update.
    /// Returns the `ioid` to put on the wire.
    pub(crate) fn register_sub_update(&self, cid: u32, subid: u32) -> u32 {
        let ioid = self.alloc_ioid();
        self.sub_updates.insert(ioid, (cid, subid));
        ioid
    }

    /// Claim the subscription a recovery `READ_NOTIFY` reply belongs to.
    /// `None` for every ordinary get, which keeps the get path unchanged.
    pub(crate) fn take_sub_update(&self, ioid: u32) -> Option<u32> {
        self.sub_updates.remove(&ioid).map(|(_, (_, subid))| subid)
    }

    /// Test-only: seed the next-ioid counter to drive the wrap path
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn seed_next_ioid(&self, v: u32) {
        self.next_ioid.store(v, Ordering::Relaxed);
    }
}

// --- cid allocator ---

/// `cid` allocator with a live-set. Unlike `ioid`/`subid` — whose
/// owning tables are reachable where they are allocated — the cid is
/// minted in [`super::CaClient::create_channel`], which is synchronous
/// and uses the cid immediately (search schedule + lifecycle handle),
/// so it cannot be allocated inside the coordinator. This shared
/// allocator therefore owns the live-set directly: [`Self::allocate`]
/// reserves an id, skipping any cid still live after a 2^32 wrap, and
/// the coordinator calls [`Self::release`] at its single channel-
/// removal site (`CoordRequest::DropChannel`).
///
/// Invariant: the live-set mirrors the coordinator's `channels` map
/// keyed by cid — reserved at `create_channel`, released at
/// `DropChannel`. Disconnect keeps a channel's entry (it will
/// reconnect), and `DropChannel` is the one and only place a cid leaves
/// `channels`, so the two views never disagree on which cids are live.
#[derive(Clone)]
pub(crate) struct CidAllocator {
    next: Arc<AtomicU32>,
    live: Arc<DashMap<u32, ()>>,
}

impl CidAllocator {
    pub(crate) fn new() -> Self {
        Self {
            next: Arc::new(AtomicU32::new(1)),
            live: Arc::new(DashMap::new()),
        }
    }

    /// Reserve a fresh non-zero cid not currently live. The monotonic
    /// counter guarantees two concurrent allocations never receive the
    /// same value, so the probe only has to guard against a prior-wrap
    /// survivor; the subsequent insert records the reservation for
    /// future probes and for [`Self::release`].
    pub(crate) fn allocate(&self) -> u32 {
        let cid = crate::channel::alloc_nonzero_probe(&self.next, |v| self.live.contains_key(&v));
        self.live.insert(cid, ());
        cid
    }

    /// Release a cid at channel teardown (`DropChannel`). Idempotent —
    /// a missing entry (already released) is a no-op.
    pub(crate) fn release(&self, cid: u32) {
        self.live.remove(&cid);
    }

    /// Test-only view of the current live-set size.
    #[cfg(test)]
    pub(crate) fn live_len(&self) -> usize {
        self.live.len()
    }

    /// Test-only: seed the next-cid counter to drive the wrap path
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn seed_next(&self, v: u32) {
        self.next.store(v, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod default_exception_tests {
    use super::*;

    fn put_failed() -> CaException {
        CaException {
            kind: CaExceptionKind::ServerError,
            message: "TST:LO".to_string(),
            server_addr: Some("127.0.0.1:5064".parse().unwrap()),
            pv_name: Some("TST:LO".to_string()),
            status: Some(crate::protocol::ECA_PUTFAIL),
            op: CaOp::Put,
            data_type: Some(0), // DBR_STRING
            count: Some(1),
            source: LIBCA_WRITE_EXCEPTION_SITE,
        }
    }

    /// Byte-for-byte the block the compiled C `caput` (EPICS 7.0.10.1-DEV)
    /// wrote to stderr for `caput TST:LO abc` against a live softIoc, with the
    /// clock pinned.
    #[test]
    fn a_rejected_write_renders_c_s_block() {
        let got = render_default_exception(&put_failed(), "Mon Jul 13 2026 09:25:31.803490065")
            .expect("C prints a block for a channel write exception");
        assert_eq!(
            got,
            concat!(
                "CA.Client.Exception...............................................\n",
                "    Warning: \"Channel write request failed\"\n",
                "    Context: \"op=1, channel=TST:LO, type=DBR_STRING, count=1, ctx=\"TST:LO\"\"\n",
                "    Source File: ../oldChannelNotify.cpp line 159\n",
                "    Current Time: Mon Jul 13 2026 09:25:31.803490065\n",
                "..................................................................\n",
            )
        );
    }

    /// R18-20: the ECA_UNRESPTMO block. `tcpiiu::unresponsiveCircuitNotify`
    /// (`tcpiiu.cpp:922-926`) passes `getHostName()` — the resolved peer name,
    /// nothing else — as the whole context, and reaches `vSignal` through
    /// `genLocalExcep`, which always carries `__FILE__`/`__LINE__`
    /// (`iocinf.h:67`), so the `Source File:` line is printed. Pre-fix the port
    /// invented its own sentence for the context ("circuit unresponsive:
    /// 127.0.0.1:15064 (matches libca ECA_UNRESPTMO)") and dropped the line.
    #[test]
    fn an_unresponsive_circuit_renders_c_s_eca_unresptmo_block() {
        let addr: SocketAddr = "127.0.0.1:15064".parse().unwrap();
        let mut exc = unresponsive_circuit_exception(addr);
        exc.message = "localhost:15064".to_string();
        let got = render_default_exception(&exc, "Mon Jul 13 2026 20:57:51.950625115")
            .expect("C prints a block when a circuit stops answering echoes");
        assert_eq!(
            got,
            concat!(
                "CA.Client.Exception...............................................\n",
                "    Warning: \"Virtual circuit unresponsive\"\n",
                "    Context: \"localhost:15064\"\n",
                "    Source File: ../tcpiiu.cpp line 925\n",
                "    Current Time: Mon Jul 13 2026 20:57:51.950625115\n",
                "..................................................................\n",
            )
        );
    }

    /// R18-20: the ECA_DBLCHNL block. The context is C's `epicsSnprintf` text
    /// (`cac.cpp:1317-1318`) — the port already matched that — but the raise is
    /// `exception(…, __FILE__, __LINE__)` at `cac.cpp:1323`, so the block has a
    /// `Source File:` line the port was dropping.
    #[test]
    fn a_multiply_defined_pv_renders_c_s_eca_dblchnl_block() {
        let exc = multiply_defined_pv_exception(
            "TST:AI".to_string(),
            "127.0.0.1:15064".parse().unwrap(),
            "127.0.0.1:15065".parse().unwrap(),
        );
        let got = render_default_exception(&exc, "T").expect("C prints a block");
        assert_eq!(
            got,
            concat!(
                "CA.Client.Exception...............................................\n",
                "    Warning: \"Identical process variable names on multiple servers\"\n",
                "    Context: \"Channel: \"TST:AI\", Connecting to: 127.0.0.1:15064, \
                 Ignored: 127.0.0.1:15065\"\n",
                "    Source File: ../cac.cpp line 1323\n",
                "    Current Time: T\n",
                "..................................................................\n",
            )
        );
    }

    /// R18-19: byte-for-byte the block compiled `camonitor` (7.0.10.1-DEV)
    /// wrote to stderr when its softIoc was killed under it. Pre-fix the
    /// circuit-gone path raised no exception at all, so this shape had no
    /// producer.
    #[test]
    fn a_dead_circuit_renders_c_s_eca_disconn_block() {
        let addr: SocketAddr = "127.0.0.1:15064".parse().unwrap();
        let mut exc = circuit_disconnect_exception(addr);
        // `cached_name` resolves asynchronously; pin the name the live C run
        // printed so the test asserts the block, not the resolver.
        exc.message = "localhost:15064".to_string();
        let got = render_default_exception(&exc, "Mon Jul 13 2026 20:57:51.950625115")
            .expect("C prints a block when a circuit with channels dies");
        assert_eq!(
            got,
            concat!(
                "CA.Client.Exception...............................................\n",
                "    Warning: \"Virtual circuit disconnect\"\n",
                "    Context: \"localhost:15064\"\n",
                "    Source File: ../cac.cpp line 1240\n",
                "    Current Time: Mon Jul 13 2026 20:57:51.950625115\n",
                "..................................................................\n",
            )
        );
    }

    /// The severity word comes from the status, not from the kind
    /// (`ca_client_context.cpp:365-375`).
    #[test]
    fn the_severity_word_tracks_the_eca_status() {
        let mut exc = put_failed();
        exc.status = Some(crate::protocol::ECA_BADTYPE); // CA_K_ERROR
        let block = render_default_exception(&exc, "T").unwrap();
        assert!(
            block.contains("    Error: \"The data type specified is invalid\"\n"),
            "{block}"
        );
    }

    /// A non-channel exception takes libca's plain overload: the ctx text is
    /// printed as-is and there is NO `Source File:` line
    /// (`cac::defaultExcep` passes a null file).
    #[test]
    fn a_non_channel_exception_prints_the_bare_context() {
        let exc = CaException {
            kind: CaExceptionKind::ServerError,
            message: "host=127.0.0.1:5064 ctx=whatever".to_string(),
            server_addr: Some("127.0.0.1:5064".parse().unwrap()),
            pv_name: None,
            status: Some(crate::protocol::ECA_BADMASK),
            op: CaOp::Other,
            data_type: None,
            count: None,
            source: ExceptionSite::NullFile,
        };
        let block = render_default_exception(&exc, "T").unwrap();
        assert!(
            block.contains("    Context: \"host=127.0.0.1:5064 ctx=whatever\"\n"),
            "{block}"
        );
        assert!(!block.contains("Source File"), "{block}");
    }

    /// A server-initiated disconnect is a port-only notification: libca
    /// delivers it through the connection callback and prints nothing.
    #[test]
    fn a_server_disconnect_prints_nothing() {
        let exc = CaException {
            kind: CaExceptionKind::ServerDisconnect,
            message: "server-initiated channel close".to_string(),
            server_addr: None,
            pv_name: Some("TST:LO".to_string()),
            status: None,
            op: CaOp::Other,
            data_type: None,
            count: None,
            source: ExceptionSite::NullFile,
        };
        assert!(render_default_exception(&exc, "T").is_none());
    }
}

#[cfg(test)]
mod id_alloc_tests {
    use super::*;

    #[test]
    fn ioid_alloc_is_monotonic_and_distinct() {
        let f = InFlightOps::new();
        let a = f.alloc_ioid();
        let b = f.alloc_ioid();
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
    }

    #[test]
    fn ioid_alloc_skips_live_id_on_wrap() {
        let f = InFlightOps::new();
        // Park a live read waiter at ioid 1.
        f.reads.insert(
            1,
            ReadWaiter::Warm {
                cid: 1,
                mode: ReadReplyMode::Plain,
                slot: Arc::new(parking_lot::Mutex::new(None)),
            },
        );
        // Force the counter to wrap back onto the live id.
        f.seed_next_ioid(1);
        let id = f.alloc_ioid();
        assert_ne!(
            id, 1,
            "must not reissue an ioid whose read is still in flight"
        );
        assert!(!f.reads.contains_key(&id) && !f.writes.contains_key(&id));
    }

    #[test]
    fn ioid_alloc_skips_live_write_on_wrap() {
        let f = InFlightOps::new();
        let (tx, _rx) = oneshot::channel();
        f.writes.insert(2, (7, tx));
        f.seed_next_ioid(2);
        let id = f.alloc_ioid();
        assert_ne!(
            id, 2,
            "must not reissue an ioid whose write is still in flight"
        );
    }

    #[test]
    fn cid_allocator_reserves_distinct_and_releases() {
        let a = CidAllocator::new();
        let c1 = a.allocate();
        let c2 = a.allocate();
        assert_ne!(c1, c2);
        assert_ne!(c1, 0);
        assert_eq!(a.live_len(), 2);
        a.release(c1);
        assert_eq!(a.live_len(), 1);
        // Idempotent: releasing an absent cid is a no-op.
        a.release(c1);
        assert_eq!(a.live_len(), 1);
    }

    #[test]
    fn cid_allocator_skips_live_cid_on_wrap() {
        let a = CidAllocator::new();
        let live = a.allocate();
        // Force the counter to wrap back onto the still-live cid.
        a.seed_next(live);
        let again = a.allocate();
        assert_ne!(
            again, live,
            "must not reissue a cid whose channel is still live"
        );
        assert_eq!(a.live_len(), 2);
    }

    #[test]
    fn cid_allocator_reuses_released_cid_only_after_release() {
        let a = CidAllocator::new();
        let c1 = a.allocate();
        a.release(c1);
        // After release the id is free; a wrap onto it may reuse it.
        a.seed_next(c1);
        let c2 = a.allocate();
        assert_eq!(c2, c1, "a released cid is reusable on wrap");
    }
}

// --- Search Engine messages ---

/// Why a search is being initiated — affects bucket assignment.
///
/// pvxs-style bucket scheduler dispatches new searches into a 30-bucket
/// ring. `Initial` and `BeaconAnomaly` searches go into the immediately
/// next bucket (fire within 1 tick); `Reconnect` searches are hashed by
/// cid across all buckets so a server-side event disconnecting N channels
/// doesn't materialize as one burst of N searches per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchReason {
    /// Fresh channel creation.
    Initial,
    /// Re-search after TCP disconnect / server disconnect.
    Reconnect,
    /// Beacon anomaly detected for the server this channel was on.
    BeaconAnomaly,
}

pub(crate) enum SearchRequest {
    /// Schedule a PV for searching.
    Schedule {
        cid: u32,
        pv_name: String,
        reason: SearchReason,
    },
    /// Cancel searching for a PV (channel dropped or connected).
    Cancel { cid: u32 },
    /// Feedback from coordinator about TCP connection outcome.
    ConnectResult {
        cid: u32,
        success: bool,
        server_addr: SocketAddr,
    },
    /// Append a unicast address to the search engine's working
    /// address list. Mirrors libca
    /// `addAddrToChannelAccessAddressList` (iocinf.cpp:45). The
    /// new entry is consulted on the next scheduled search round;
    /// already-pending searches do NOT auto-restart against the
    /// new address — they reach it at their natural retry. There is
    /// no way to force a round: [`super::CaClient`] exposes none, and
    /// neither does libca, whose `addAddrToChannelAccessAddressList`
    /// only fills a list and never touches a live `cac`.
    AddAddress(SocketAddr),
    /// Remove a unicast address from the search engine's working
    /// address list. Used when a discovery backend reports an IOC
    /// went away (`DiscoveryEvent::Removed`). No-op if the address
    /// isn't present. Already-pending searches against the removed
    /// address run to their natural retry; only future search rounds
    /// stop targeting it.
    ///
    /// `client` only: a `DiscoveryEvent::Removed` is the sole producer —
    /// there is no `CaClient::remove_address` counterpart to
    /// [`super::CaClient::add_address`], because libca has no
    /// `removeAddrFromChannelAccessAddressList` either.
    #[cfg(feature = "client")]
    RemoveAddress(SocketAddr),
    /// Replace the entire working address list. Mirrors libca
    /// `configureChannelAccessAddressList` (iocinf.cpp:166). Use
    /// when the application has authoritative knowledge of the
    /// IOC topology and wants to override env-derived state at
    /// runtime.
    SetAddressList(Vec<SocketAddr>),
}

pub(crate) enum SearchResponse {
    Found {
        cid: u32,
        server_addr: SocketAddr,
    },
    /// dispatched when a second SEARCH reply for the same cid
    /// names a different server (the libca
    /// `cac.cpp::msgForMultiplyDefinedPV` condition). The coordinator
    /// fans this out to the exception handler as `ECA_DBLCHNL`,
    /// matching libca's `pvMultiplyDefinedNotify` → `this->exception`
    /// path.
    MultiplyDefined {
        pv_name: String,
        prev_addr: SocketAddr,
        new_addr: SocketAddr,
    },
}

// --- Transport Manager messages ---

pub(crate) enum TransportCommand {
    CreateChannel {
        cid: u32,
        pv_name: String,
        server_addr: SocketAddr,
        priority: u8,
    },
    ReadNotify {
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        server_addr: SocketAddr,
        priority: u8,
    },
    Write {
        sid: u32,
        /// Client cid of the channel the write is issued on. Goes on the
        /// wire in `m_available`, where libca puts it
        /// (`tcpiiu::writeRequest`, `tcpiiu.cpp:1430-1432`) so the server's
        /// echo in a `CA_PROTO_ERROR` names the channel back
        /// (`cac::writeExcep` reads `hdr.m_available`).
        cid: u32,
        data_type: u16,
        count: u32,
        payload: Vec<u8>,
        server_addr: SocketAddr,
        priority: u8,
    },
    WriteNotify {
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        payload: Vec<u8>,
        server_addr: SocketAddr,
        priority: u8,
    },
    Subscribe {
        sid: u32,
        data_type: u16,
        count: u32,
        subid: u32,
        mask: u16,
        server_addr: SocketAddr,
        priority: u8,
    },
    Unsubscribe {
        sid: u32,
        subid: u32,
        data_type: u16,
        /// Original requested element count from the EVENT_ADD that
        /// installed this subscription. C `libca/tcpiiu.cpp::
        /// subscriptionCancelRequest()` includes the subscription's
        /// stored count in the CANCEL request; we echo the same
        /// shape so strict CA dissectors / replay tooling see the
        /// libca-equivalent frame.
        count: u32,
        server_addr: SocketAddr,
        priority: u8,
    },
    ClearChannel {
        cid: u32,
        sid: u32,
        server_addr: SocketAddr,
        priority: u8,
    },
    /// Beacon arrival routed from the beacon monitor to the per-circuit
    /// receive watchdog. `anomaly = false` for healthy beacons (mirrors
    /// libca `tcpRecvWatchdog::beaconArrivalNotify` — pet the watchdog
    /// so a quiet circuit isn't probed unnecessarily). `anomaly = true`
    /// when the monitor classified the beacon as anomalous — a fresh
    /// sequence (`IdMismatch`) or an off-band period (libca
    /// `bhe.cpp:226-262`); the read loop only sets a
    /// flag (mirrors libca `beaconAnomalyNotify`) and lets the existing
    /// idle watchdog expire on its own schedule rather than firing an
    /// immediate echo probe — under load that immediate probe was the
    /// trigger for spurious 5-s echo timeouts and reconnect storms.
    ///
    /// a beacon is a per-server UDP signal, but the watchdog it
    /// pets lives on each circuit, so the notify fans out to every
    /// priority circuit for `server_addr` (see `process_command`).
    #[cfg(ca_beacon_monitor)]
    BeaconArrivalNotify {
        server_addr: SocketAddr,
        anomaly: bool,
    },
}

pub(crate) enum TransportEvent {
    ChannelCreated {
        cid: u32,
        sid: u32,
        data_type: u16,
        element_count: u32,
        access: AccessRights,
        server_addr: SocketAddr,
        /// priority of the circuit the CREATE_CH_RESP arrived
        /// on. Lets the coordinator clear a late response for an
        /// already-dropped cid on the right circuit.
        priority: u8,
    },
    MonitorData {
        subid: u32,
        data_type: u16,
        count: u32,
        data: Vec<u8>,
    },
    /// Server emitted a monitor frame with a non-NORMAL `m_cid` (ECA
    /// status), e.g. `no_read_access_event` after an ACF reload
    /// revoked read access on an active subscription. libca
    /// `cac::eventAddRespAction` (`cac.cpp:973-977`) routes this to
    /// the per-subscription `pmiu->exception` callback with the
    /// reported status — the user's monitor callback receives an
    /// Err result. Pre-fix Rust warn+dropped the frame, so an
    /// `ECA_NORDACCESS` from a C IOC was silently invisible to the
    /// subscriber.
    MonitorStatusError {
        subid: u32,
        eca_status: u32,
    },
    AccessRightsChanged {
        cid: u32,
        access: AccessRights,
    },
    ChannelCreateFailed {
        cid: u32,
    },
    ServerError {
        /// ECA status code (caerr.h) — the server's resp.cid carries
        /// this in CA_PROTO_ERROR. This is what `ca_extract_msg_no(stat)`
        /// would parse on the C side.
        eca_status: u32,
        /// Original request command that triggered the error
        /// (from the first u16 of the error payload's copy of the
        /// original header). Diagnostic only — distinct from `eca_status`.
        original_request: Option<u16>,
        message: String,
        server_addr: SocketAddr,
        /// DBR type and element count of the echoed request header — libca's
        /// `hdr.m_dataType` / `hdr.m_count`, which the exception block prints.
        data_type: Option<u16>,
        count: Option<u32>,
        /// Channel name the failing request was issued against, taken from
        /// [`InFlightOps::write_identities`] by the cid the ECHOED request
        /// header carries. Resolved here, on the circuit that owns the
        /// request, rather than by the coordinator against its live
        /// `channels` map: the identity of a request that already failed
        /// does not depend on whether its channel is still open.
        ///
        /// This variant deliberately carries no raw cid. It carried one, and
        /// the coordinator resolved the name from it against `channels` —
        /// which is the lookup that produced a nameless exception whenever
        /// the error overtook the channel's own teardown. Naming the channel
        /// here and nowhere else is what makes that outcome unrepresentable.
        pv_name: Option<Arc<str>>,
    },
    TcpClosed {
        server_addr: SocketAddr,
        /// which priority circuit closed. Only channels on
        /// `(server_addr, priority)` are torn down; sibling circuits to
        /// the same server at other priorities are untouched.
        priority: u8,
    },
    ServerDisconnect {
        cid: u32,
        server_addr: SocketAddr,
    },
    /// Echo timed out once — circuit may be unresponsive but TCP is still up.
    CircuitUnresponsive {
        server_addr: SocketAddr,
        priority: u8,
    },
    /// Data received after unresponsive state — circuit recovered.
    CircuitResponsive {
        server_addr: SocketAddr,
        priority: u8,
    },
    /// Server's CA minor protocol version, parsed from CA_PROTO_VERSION
    /// during TCP handshake. Mirrors libca `tcpiiu::minorProtocolVersion`
    /// (BUG_ARCHAEOLOGY d763541 / `ca_host_minor_protocol`).
    ServerVersion {
        server_addr: SocketAddr,
        priority: u8,
        minor_version: u16,
    },
    /// A fresh TCP circuit was just inserted into the connections map.
    /// Used by the coordinator to issue a `BeaconControl::ResetServer`
    /// to the beacon monitor (libca `bhe.cpp` "new client connect"
    /// EMA reset) so a stale steady-state period estimate doesn't
    /// misclassify the server's `online_notify_task` ramp-up as a
    /// short-period anomaly cascade after reconnect. Emitted exactly once
    /// per circuit, before any other event for that circuit.
    ///
    /// `client` only: its single consumer is the beacon EMA reset, and
    /// `client-core` has no beacon monitor to reset.
    #[cfg(feature = "client")]
    ServerConnected {
        server_addr: SocketAddr,
    },
}
