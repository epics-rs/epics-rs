//! Persistent TCP virtual circuit to a single PVA server.
//!
//! Replaces the old "open-fresh-socket-per-op" `Connection`. Spawns three
//! background tasks per connection:
//!
//! - **Reader**: parses incoming frames, routes them to per-IOID waiters
//!   (oneshot for one-shot ops, mpsc for monitor streams). Updates the
//!   `last_rx` timestamp used by the heartbeat.
//! - **Writer**: drains a `mpsc<Vec<u8>>` queue and writes to the socket.
//!   Owning a single writer task lets every channel/op share the connection
//!   safely without holding an `AsyncMutex` across awaits.
//! - **Heartbeat**: sends an application `CMD_ECHO` every
//!   `max(1, min(15, tcp_timeout×3/8))` s (pvxs clientconn.cpp:163-165,496);
//!   if no `last_rx` update has happened for `tcp_timeout`, declares the
//!   connection dead (pvxs clientconn.cpp:73-74).
//!
//! When any task exits (read EOF, write error, or heartbeat timeout) the
//! cancellation token fires and the connection is torn down. Channels
//! holding an `Arc<ServerConn>` observe the closed state via [`ServerConn::is_alive`]
//! and transition to "Reconnecting".

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
// NOT `use tokio::net::TcpStream`. An unconditional import is resolved whether
// or not anything reaches the item, so on `armv7-rtems-eabihf` — where
// `tokio::net` does not exist — one import line was an E0432 that poisoned this
// whole module, and rustc then suppressed every downstream error in code naming
// its items. That is what made `ops_v2.rs` report zero errors for the target
// without that zero meaning anything (`doc/pvalink-rtems-design.md` §1.2, §6
// item 1). Both remaining uses sit inside `cfg` blocks that the target does not
// compile, so they name the type by full path and the module resolves cleanly.
use epics_base_rs::runtime::task::{interval, timeout};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::error::{PvaError, PvaResult};
use crate::proto::{
    ByteOrder, Command, ControlCommand, HeaderFlags, MessageType, PvaHeader, ReadExt, Status,
    WriteExt, decode_string, encode_string_into,
};

use super::decode::{
    Frame, PeerRole, decode_connection_validated, decode_connection_validation_request,
    try_parse_frame_role,
};

/// How often we send the heartbeat application CMD_ECHO.
///
/// Resolved at call time from `EPICS_PVA_CONN_TMO`, through the two owners
/// that define the client's clock: the effective TCP timeout
/// (`effective_tcp_timeout_secs`) and the echo cadence pvxs derives from it
/// (`echo_period_secs` = `max(1, min(15, tcpTimeout*3/8))`,
/// clientconn.cpp:163). Default 15 s, matching pvxs's "tcpTimeout(40) -> 15
/// second echo period".
pub fn heartbeat_interval() -> Duration {
    let effective =
        crate::config::env::effective_tcp_timeout_secs(crate::config::env::conn_timeout_secs());
    Duration::from_secs_f64(crate::config::env::echo_period_secs(effective))
}

/// Maximum time we'll wait between any incoming bytes before declaring
/// the connection dead. The effective timeout is configured CONN_TMO ×
/// 4/3 (`tmoScale`) put through pvxs `enforceTimeout` — the 2 s floor AND
/// the `>= double(time_t::max)` → 40 s reset. Both come from the single
/// owner `config::env::effective_tcp_timeout_secs`, so client and server
/// cannot drift apart on the same CONN_TMO.
pub fn heartbeat_timeout() -> Duration {
    let configured = crate::config::env::conn_timeout_secs();
    Duration::from_secs_f64(crate::config::env::effective_tcp_timeout_secs(configured))
}

/// Per-connection timeouts and limits threaded from the client builder
/// into each dialed [`ServerConn`]. Bundled into one value so the dial
/// signatures stay below clippy's argument-count threshold as knobs are
/// added (the three fields always travel together through
/// `connect` / `connect_tls` / `run_handshake_and_spawn`).
#[derive(Clone, Copy, Debug)]
pub struct ConnConfig {
    /// Per-operation I/O deadline for the dial + handshake (pvxs
    /// `Config::operationTimeout`).
    pub op_timeout: Duration,
    /// TCP idle timeout governing the heartbeat task (pvxs
    /// `effective.tcpTimeout`, clientconn.cpp:73-74).
    pub tcp_timeout: Duration,
    /// optional opt-in cap on a single inbound message's
    /// payload length. `None` = **unbounded**, matching pvxs, which
    /// deliberately keeps no client-side RX message-size limit. The
    /// streaming reader stays bounded regardless via incremental 4 KiB
    /// reads plus the heartbeat/`op_timeout` deadlines, so the absence
    /// of a cap is not itself an OOM vector. `Some(n)` rejects (and
    /// drops the connection on) any server header announcing more than
    /// `n` bytes.
    pub max_message_size: Option<usize>,
}

/// Routing slot for a registered IOID.
///
/// GET/PUT register a `TwoShot` (2 oneshots for INIT + DATA).
/// MONITOR registers a `Stream` (unbounded mpsc).
pub(crate) enum IoidSlot {
    /// Pipelined two-frame ops (GET, PUT, RPC): FIFO queue of oneshots.
    TwoShot(VecDeque<oneshot::Sender<Frame>>),
    /// Streaming ops (MONITOR): unbounded channel.
    Stream(mpsc::UnboundedSender<Frame>),
    /// Long-lived warm-GET op: a single mutex-guarded oneshot slot
    /// that the caller refills before each new GET frame send. Lets
    /// the channel skip INIT for subsequent GETs against the same
    /// (sid, ioid) — server keeps the introspection binding alive
    /// because we never DESTROY the ioid.
    Reusable(Arc<Mutex<Option<oneshot::Sender<Frame>>>>),
}

/// A persistent server connection.
pub struct ServerConn {
    pub addr: SocketAddr,
    /// Current outbound byte order, latched from the most recent
    /// SET_BYTE_ORDER control frame the server sent. pvxs latches
    /// `sendBE = header[2] & pva_flags::MSB` on every received SetEndian
    /// (conn.cpp:169-188) and reads it at each send, so a server that
    /// re-negotiates the order mid-connection is honoured on the next
    /// outbound frame. `true` = Big. Written only by the reader task
    /// (single owner, on SET_BYTE_ORDER arrival); read by every op
    /// builder via [`ServerConn::byte_order`] and by the heartbeat task.
    out_order: Arc<AtomicBool>,
    /// X.509 identity of the *server* peer, derived from the verified
    /// TLS certificate chain (`pvas://` only — `None` for a plain
    /// `pva://` TCP connection). Mirrors pvxs `Connected::cred`, which
    /// `pvxinfo -v` prints as the server's credentials. Populated by
    /// [`ServerConn::connect_tls`] before the TLS stream is split.
    server_identity: Option<crate::auth::X509Credentials>,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    cancel: CancellationToken,
    alive: Arc<AtomicBool>,
    last_rx_nanos: Arc<AtomicU64>,
    /// total bytes read off / written to this connection's
    /// socket, for `PvaClient::report`. Shared with the reader/writer
    /// tasks.
    bytes_rx: Arc<AtomicU64>,
    bytes_tx: Arc<AtomicU64>,
    /// Per-IOID routing: DashMap for lock-free access.
    by_ioid: Arc<DashMap<u32, IoidSlot>>,
    /// CREATE_CHANNEL response routing by CID.
    by_cid: Arc<DashMap<u32, oneshot::Sender<Frame>>>,
    /// Per-SID server-initiated CMD_DESTROY_CHANNEL signals.
    by_sid_close: Arc<DashMap<u32, (Arc<AtomicBool>, Arc<tokio::sync::Notify>)>>,
    /// Reverse map ioid → sid for DESTROY_CHANNEL cleanup.
    ioid_to_sid: Arc<DashMap<u32, u32>>,
    /// command (`Command` code) the IOID was opened with.
    /// Set on every `register_ioid_*` call; consulted in
    /// `route_frame` so an inbound frame's command must match the
    /// expected one before the payload is delivered to the sink. A
    /// mismatch closes the connection — mirrors pvxs
    /// `clientget.cpp:463-470` / `clientmon.cpp:570-579` per-op
    /// command checks. Without this gate a buggy or malicious
    /// server could satisfy a GET with a MONITOR-shaped frame
    /// because IOID alone is enough to find a registered sink.
    ioid_to_cmd: Arc<DashMap<u32, u8>>,
    /// Per-IOID introspection captured by the reader task from each op's
    /// INIT response (Get/Put/Monitor → the single descriptor; PUT_GET →
    /// getIF). The reader consults it to value-flatten 0xFD/0xFE markers
    /// embedded in an `any` DATA value (see
    /// [`flatten_type_cache_markers`](crate::client_native::decode::flatten_type_cache_markers)).
    /// Inserted on INIT, dropped on MONITOR FINISH and on
    /// [`Self::unregister_ioid`]. The connection's single 0xFD/0xFE type-
    /// cache lives reader-task-local (`reader_type_cache`) — there is no
    /// shared decode-time cache, so per-op decoders never race over it.
    op_introspection: Arc<DashMap<u32, crate::pvdata::FieldDesc>>,
    /// Per-channel (by server-assigned SID) byte counters + PV name, for
    /// pvxs `Context::report` per-channel `Report::Channel` parity
    /// (client.cpp:464-501). pvxs bumps `chan->statTx` on each op-body send
    /// (clientget.cpp:321, clientmon.cpp:143, …) and `chan->statRx` on each
    /// op reply decode (clientget.cpp:496, clientmon.cpp:608); we mirror that
    /// via [`Self::send_for_channel`] and the IOID→SID attribution in
    /// `route_frame`. Connection-level `bytes_rx`/`bytes_tx` stay the socket
    /// aggregate — the per-channel counters are a subset, exactly as pvxs
    /// keeps both `Connection::statTx` and `Channel::statTx`.
    chan_stats: Arc<DashMap<u32, ChanStat>>,
}

/// Per-channel byte counters tracked on a [`ServerConn`], keyed by the
/// server-assigned SID. Mirrors pvxs `Channel::statTx` / `statRx` +
/// `Channel::name` (the fields `Context::report` copies into each
/// `Report::Channel`, client.cpp:495-496).
#[derive(Debug)]
struct ChanStat {
    name: String,
    rx: AtomicU64,
    tx: AtomicU64,
}

// NOTE: ServerConn intentionally does NOT have a Drop impl that fires
// `cancel.cancel()`. The reader/writer/heartbeat tasks each hold their
// own clone of the CancellationToken AND clones of the writer_tx /
// router Arcs, which keep ServerConn's underlying state alive past
// the last user-facing Arc<ServerConn>. The tasks unwind on socket
// close (reader Ok(0)) or queue-closed (writer drops once the last
// writer_tx clone is gone) within ~5 s, and the heartbeat exits on
// idle_timeout. Adding `cancel.cancel()` to Drop here interferes with
// the reconnect path (client/channel.rs:355) — by the time Drop fires
// the new connection's TCP-level connect can race with the OS-level
// release of the old port, surfacing as ConnectionRefused.

/// Type-erased read half. We accept a plain TCP read half, a TLS read half, or
/// a blocking pump's adapter through the same code path.
pub(crate) type DynRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
/// Type-erased write half.
pub(crate) type DynWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;

/// The EPICS priority a client connection's two pump threads run at.
///
/// pvxs gives its client reactor thread `epicsThreadPriorityCAServerLow-2`
/// (`pvxs/src/client.cpp`, the same `PVXTCP` band the server's acceptor takes),
/// and the server driver in this crate takes 18 for exactly that reason. A
/// client pump does the same job — move bytes between a socket and a frame
/// pipeline — so it takes the same number rather than a new one: splitting how
/// work is scheduled *internally* must not change how it is scheduled relative
/// to everything else.
///
/// Passed to `drive_socket_blocking` for **both** pumps. That primitive takes a
/// reader and a writer band separately because libca derives two for a CA
/// circuit (`tcpiiu.cpp:677-682`); pvxs derives one, and passing it twice is
/// how this side states that its two pumps are one band by intent rather than
/// by an API that could not express the difference.
const PVA_CLIENT_PRIORITY: epics_base_rs::runtime::task::ThreadPriority =
    epics_base_rs::runtime::task::ThreadPriority::Custom(18);

/// Every TCP dial this client makes, on a bounded set of permanent threads.
///
/// One pool for the process rather than one per `PvaClient`: `pvalink` builds a
/// client per link (`pvalink/registry.rs` keys on the link, not the IOC), so a
/// per-client pool would multiply the bound by the link count — the count this
/// exists to bound. The pool is per *role* instead, which is what the band
/// requires: workers enter `PVA_CLIENT_PRIORITY`, the band of the pumps they
/// precede, and the CA client's dials cannot share threads with them.
///
/// `"PVAC-dial"` and not `"PVAC-connect {target}"`: a reused worker cannot be
/// named for one target. A thread dump therefore still says *how many* dials
/// are stuck but no longer *which server* is not answering — the trade for
/// bounding the count. The target remains in the caller's own error and in the
/// `debug!` the connection path emits.
static PVA_DIAL_POOL: epics_base_rs::runtime::blocking_io::DialPool =
    epics_base_rs::runtime::blocking_io::DialPool::new("PVAC-dial", PVA_CLIENT_PRIORITY);

/// The most concurrent PVA circuits this process serves with blocking pumps —
/// a bound on thread *creations*, so a client that redials the same server
/// reuses its two pump threads rather than leaking 2 × 176 B per reconnect on
/// RTEMS. Past the bound a circuit's pumps are refused (`EAGAIN`), which the
/// caller sees as a failed connect. Generous: a PVA client legitimately holds a
/// circuit per distinct server.
const PVA_CIRCUIT_POOL_CAPACITY: usize = 64;

/// The pumps for every PVA circuit, borrowed from a bounded set of permanent
/// threads. pvxs runs one reactor band for both directions, so both roles take
/// `PVA_CLIENT_PRIORITY`; per band, so it is a separate pool from the CA
/// client's.
static PVA_CIRCUIT_POOL: std::sync::LazyLock<epics_base_rs::runtime::worker_pool::WorkerPool<2>> =
    std::sync::LazyLock::new(|| {
        epics_base_rs::runtime::worker_pool::WorkerPool::new(
            "PVAC",
            epics_base_rs::runtime::blocking_io::circuit_roster(
                PVA_CLIENT_PRIORITY,
                PVA_CLIENT_PRIORITY,
            ),
            PVA_CIRCUIT_POOL_CAPACITY,
        )
    });

/// BRING-UP PROBE: dial attempts submitted to [`PVA_DIAL_POOL`] since boot.
///
/// The denominator of the on-target bound measurement. `worker_count()` alone
/// cannot distinguish "bounded" from "never dialled twice", so the rig needs
/// the attempt count next to it — and `ns_task`'s own per-attempt diagnostic
/// is a `debug!`, which the target's console subscriber (INFO) drops.
#[cfg(feature = "bringup-probes")]
static PVA_DIAL_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// BRING-UP PROBE: `(workers created, dial attempts submitted)` for the PVA
/// client's dial pool.
///
/// Behind `bringup-probes` with the rest of the measurement rig
/// (`doc/pvalink-rtems-design.md` §12): a default image compiles neither the
/// counter nor this accessor.
#[cfg(feature = "bringup-probes")]
pub fn dial_pool_probe() -> (usize, usize) {
    (
        PVA_DIAL_POOL.worker_count(),
        PVA_DIAL_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Dial `target` and drive it with two blocking pump threads.
///
/// Compiled on every target, not only the ones that select it: the RTEMS build
/// is not the only caller — [`ServerConn::connect_blocking`] is public, and a
/// host test that wants both transports in one binary needs this to exist
/// unconditionally. Nothing here names `tokio::net`, so it costs the target
/// nothing to keep it always-on.
///
/// The two bounds are deliberately different values and must not be collapsed
/// into one; see [`dial_pva`] for what each one is.
///
/// # Why the connect gets a thread
///
/// The connect is a *blocking* syscall bounded by `connect_timeout`, and every
/// caller of this function is a task — `ns_task` and the channel pool through
/// [`dial_pva`], and [`ServerConn::connect_blocking`] directly. On the exec
/// backend a task runs on a cooperative callback-band worker shared with every
/// other future on its band, so running the connect inline parks the band for
/// the whole bound. Measured exactly there (gdb all-thread dump, host-linux
/// `realtime-pva-ioc`): the single `cbMedium` worker sat in `poll(timeout=39999)`
/// under `TcpStream::connect_timeout` ← `dial_blocking` ← `ns_task` — a name
/// server that did not answer starved every future on Medium for ~40 s per
/// attempt (the executor failure class of `doc/qsrv-rtems-design.md` §9.15.1).
///
/// So the connect takes a dial thread — the CA client's `dial_blocking` shape
/// (`epics-ca-rs/src/client/transport.rs`) — at the same band as the two pumps
/// it precedes, and the caller parks on a oneshot instead: the receiver
/// registers the task's waker, the send from the dial thread wakes it, and the
/// band worker is released for the whole dial.
///
/// # Why the thread is borrowed and not created
///
/// The dial thread comes from [`PVA_DIAL_POOL`], a permanent set of at most
/// `MAX_DIAL_WORKERS` workers, and is returned to it when the connect resolves.
/// It used to be created per attempt and left to exit. That is unbounded in
/// thread *creations*, and creations are the cost on RTEMS: every
/// `std::thread` leaks 128 B there permanently (its TLS key is freed before the
/// key's destructor runs). A search engine whose name server is down redials
/// roughly every 10 s for as long as the IOC runs, so per-attempt creation
/// leaked with no ceiling — a leak the bound now removes by construction rather
/// than caps at runtime. See `runtime::blocking_io::DialPool`.
///
/// A worker is still the **single finalizer** for the socket it opens: a
/// receiver dropped by an aborted caller only makes the send fail, and the
/// fresh socket is dropped — and closed — right there, before the worker takes
/// its next request.
///
/// # Why the thread's connect is plain-blocking, and where the bound lives
///
/// The thread issues `TcpStream::connect`, **not** `connect_timeout`: the
/// plain blocking connect is the CA client's proven on-target dial, C parity
/// with `tcpiiu.cpp`'s blocking `::connect()`, and a thread that owns its
/// blocking needs no poll machinery. (An earlier measurement blamed
/// `connect_timeout` for aborting on the RTEMS target; that RST was forged
/// by the QEMU rig's SLIRP hub port, not sent by the guest, and the claim is
/// withdrawn — `doc/pvalink-rtems-design.md` §6 item 4. The same target
/// dials out and connects once the rig is fixed.)
///
/// The application-level bound (`connect_timeout`, pvxs `operationTimeout`)
/// therefore moves to the awaiting side: `runtime::task::timeout` around the
/// oneshot — the timer mechanism the exec backend already runs everywhere on
/// target — fails the *dial* at the deadline while the thread keeps blocking
/// under the OS's own connect ladder — 75 s on the RTEMS target this dial is
/// written for (libbsd `TCPTV_KEEP_INIT`, `75 * hz`, measured), ~130 s on a
/// Linux host (`tcp_syn_retries`). A
/// timed-out dial's worker stays inside the connect until that OS bound, still
/// the socket's single finalizer: if the connect completes after the caller
/// gave up, the failed send drops the fresh socket. The occupancy is bounded
/// (`MAX_DIAL_WORKERS` for the whole process, and past that dials queue and
/// still fail at their own bound) and only occurs on a blackholed peer — a
/// refused or reachable peer resolves the connect promptly.
async fn dial_blocking(
    target: SocketAddr,
    connect_timeout: Duration,
    write_deadline: Duration,
) -> PvaResult<(DynRead, DynWrite)> {
    use epics_base_rs::runtime::blocking_io::{PumpConfig, drive_socket_blocking};

    // Plain blocking `connect` on a pooled worker; the application bound is
    // applied by the awaiting side below. See "Why the thread's connect is
    // plain-blocking" above.
    #[cfg(feature = "bringup-probes")]
    PVA_DIAL_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dialed_rx = PVA_DIAL_POOL.dial(target).map_err(PvaError::Io)?;
    let stream = match timeout(connect_timeout, dialed_rx).await {
        // The pvxs `operationTimeout` bound: the dial fails now; the worker
        // finishes under the OS connect ladder and closes whatever it opens.
        Err(_) => return Err(PvaError::Timeout),
        Ok(Ok(Ok(s))) => s,
        Ok(Ok(Err(e))) => {
            return Err(match e.kind() {
                std::io::ErrorKind::TimedOut => PvaError::Timeout,
                _ => PvaError::Io(e),
            });
        }
        // The thread ended without sending: it panicked.
        Ok(Err(_)) => {
            return Err(PvaError::Io(std::io::Error::other(
                "circuit dial thread ended without a result",
            )));
        }
    };
    // `read_timeout` keeps `PumpConfig`'s effectively-infinite default, and
    // that is load-bearing rather than a default taken for lack of a better
    // value. `reader_pump` ends the connection when its `SO_RCVTIMEO` expires,
    // so any finite value here is an idle-disconnect bound — and an idle PVA
    // circuit is *supposed* to be silent between echoes (15 s by default,
    // `heartbeat_interval`), with `tcp_timeout` (40 s) the only thing entitled
    // to call it dead. Handing the pump a per-operation deadline instead makes
    // every connection quieter than one operation self-destruct. What ends a
    // parked reader here is `ReaderPumpGuard`'s `shutdown`, driven by the
    // cancellation token the heartbeat already fires — the same mechanism the
    // server driver relies on for the same reason.
    let (reader, writer) = drive_socket_blocking(
        &PVA_CIRCUIT_POOL,
        stream,
        &target.to_string(),
        &PumpConfig {
            send_timeout: write_deadline,
            ..PumpConfig::default()
        },
    )
    .map_err(PvaError::Io)?;
    Ok((Box::new(reader), Box::new(writer)))
}

/// Dial `target` and return the connection's two byte halves.
///
/// **This is the client's one TCP dial.** [`ServerConn::connect`] and the
/// name-server connection in `search_engine` both come through it, so there is
/// one place where "how does this client get bytes onto a socket" is decided
/// and one place a new transport would have to be added.
///
/// Two implementations, selected at compile time and never at runtime:
///
/// * `tokio_backend` — `tokio::net::TcpStream`, split into its two owned
///   halves. What shipped before this seam existed, unchanged.
/// * `exec_backend` or `--cfg pva_blocking_client` —
///   `runtime::blocking_io`'s two blocking pump threads over one
///   `Arc<TcpStream>`.
///
/// The arm is chosen by the *backend*, not by the target. `exec_backend` is
/// `epics_embedded_target` (`target_os` in `{"rtems", "vxworks"}`) **or**
/// `--features rtems-exec-model` (`build.rs`), and what both share is that a
/// future started through `runtime::task::spawn` runs with no tokio reactor
/// entered — on RTEMS or VxWorks because there is none and `tokio::net` does
/// not compile for either triple, on a host exec-model build because the
/// future lands on a callback-pool worker the runtime was never entered on.
/// A `tokio::net::TcpStream::connect` there panics ("there is no reactor
/// running") even though the process has a runtime elsewhere. Gating
/// this seam on `target_os = "rtems"` named the target where the fact it needs
/// is the backend, which is why `realtime-pva-ioc` still panicked on its first
/// dial after the UDP seam was fixed (`doc/calink-rtems-design.md` §10.10
/// item 2).
///
/// The returned halves are the same `DynRead`/`DynWrite` the TLS path produces,
/// which is the whole reason the client needed no protocol change to gain a
/// second transport: `ServerConn::run_handshake_and_spawn` cannot tell them
/// apart.
///
/// # The two bounds
///
/// * `connect_timeout` — how long the TCP connect itself may take. Both
///   transports honour it; it is the client's per-operation deadline
///   (`ConnConfig::op_timeout`, pvxs `operationTimeout`).
/// * `write_deadline` — how long **one whole frame** may take to reach the
///   wire. It exists only on the blocking side, because only there is a write
///   a parked thread that something has to be entitled to reclaim
///   (`blocking_io::write_frame_deadline`); the hosted writer task has no such
///   bound and needs none. Callers pass the connection's own liveness bound —
///   `ConnConfig::tcp_timeout` — so the pump can never end a circuit the
///   protocol would still consider alive. Recorded as a deviation in
///   `doc/pvalink-rtems-design.md` §9.
pub(crate) async fn dial_pva(
    target: SocketAddr,
    connect_timeout: Duration,
    write_deadline: Duration,
) -> PvaResult<(DynRead, DynWrite)> {
    #[cfg(not(any(exec_backend, pva_blocking_client)))]
    {
        // The hosted transport has no per-frame write bound to apply.
        let _ = write_deadline;
        let stream = timeout(connect_timeout, tokio::net::TcpStream::connect(target))
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(PvaError::Io)?;
        stream.set_nodelay(true).ok();
        let (reader, writer) = stream.into_split();
        Ok((Box::new(reader), Box::new(writer)))
    }
    #[cfg(any(exec_backend, pva_blocking_client))]
    {
        dial_blocking(target, connect_timeout, write_deadline).await
    }
}

impl ServerConn {
    /// Open a plain TCP connection, run the handshake, and start
    /// background tasks.
    ///
    /// `op_timeout` guards the handshake I/O; `tcp_timeout` is stored and
    /// used by the spawned heartbeat task as the connection idle timeout
    /// (pvxs `effective.tcpTimeout`, clientconn.cpp:73-74).
    pub async fn connect(
        target: SocketAddr,
        user: &str,
        host: &str,
        conn: ConnConfig,
    ) -> PvaResult<Arc<Self>> {
        let (reader, writer) = dial_pva(target, conn.op_timeout, conn.tcp_timeout).await?;
        // Plain `pva://` TCP — no TLS, so no server X.509 identity.
        Self::run_handshake_and_spawn(target, reader, writer, None, user, host, conn).await
    }

    /// Open a plain TCP connection driven by **two blocking threads** instead of
    /// the tokio reactor, run the handshake, and start background tasks.
    ///
    /// The third constructor, beside [`connect`](Self::connect) and
    /// [`connect_tls`](Self::connect_tls), and the one an RTEMS target uses:
    /// there is no tokio reactor there, and `tokio::net` does not compile for
    /// the triple at all.
    ///
    /// It is *not* a second protocol. It hands
    /// [`run_handshake_and_spawn`](Self::run_handshake_and_spawn) the same
    /// `DynRead`/`DynWrite` the other two do, built from
    /// `runtime::blocking_io`'s adapters, so the handshake, the reader task's
    /// frame loop, the writer task, the heartbeat and every operation state
    /// machine are the same code reached by the same path. The two pump threads
    /// are owned by the adapters, so they retire when the reader and writer
    /// tasks let go of them — the same lifecycle the hosted socket halves have.
    ///
    /// [`connect`](Self::connect) already selects this transport on RTEMS and
    /// under `--cfg pva_blocking_client`. This entry point exists for a caller
    /// that wants it explicitly, and for tests that want both transports in one
    /// binary.
    pub async fn connect_blocking(
        target: SocketAddr,
        user: &str,
        host: &str,
        conn: ConnConfig,
    ) -> PvaResult<Arc<Self>> {
        let (reader, writer) = dial_blocking(target, conn.op_timeout, conn.tcp_timeout).await?;
        Self::run_handshake_and_spawn(target, reader, writer, None, user, host, conn).await
    }

    /// Open a TLS-wrapped connection (`pvas://`).
    ///
    /// The only client entry point that names a rustls type, so it is the
    /// only one gated on the `tls` feature; the `Option<Arc<TlsClientConfig>>`
    /// plumbing that reaches it compiles either way (see [`crate::auth`]).
    #[cfg(feature = "tls")]
    pub async fn connect_tls(
        target: SocketAddr,
        server_name: &str,
        tls: Arc<crate::auth::TlsClientConfig>,
        user: &str,
        host: &str,
        conn: ConnConfig,
    ) -> PvaResult<Arc<Self>> {
        let stream = timeout(conn.op_timeout, tokio::net::TcpStream::connect(target))
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(PvaError::Io)?;
        stream.set_nodelay(true).ok();

        let connector = tokio_rustls::TlsConnector::from(tls.config.clone());
        let dnsname = rustls::pki_types::ServerName::try_from(server_name.to_string())
            .map_err(|e| PvaError::Protocol(format!("invalid TLS server name: {e}")))?;
        let tls_stream = timeout(conn.op_timeout, connector.connect(dnsname, stream))
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(PvaError::Io)?;

        // Derive the *server*'s X.509 identity from the verified
        // certificate chain before the stream is split — rustls only
        // exposes `peer_certificates()` on the whole `TlsStream`. The
        // chain has already passed the client-side verifier, so this
        // is the cryptographically-checked server identity that pvxs
        // `pvxinfo -v` reports (`Connected::cred`).
        let server_identity = {
            let (_, tls_conn) = tls_stream.get_ref();
            tls_conn
                .peer_certificates()
                .and_then(crate::auth::x509_credentials_from_chain)
        };

        let (reader, writer) = tokio::io::split(tls_stream);
        let reader: DynRead = Box::new(reader);
        let writer: DynWrite = Box::new(writer);
        Self::run_handshake_and_spawn(target, reader, writer, server_identity, user, host, conn)
            .await
    }

    /// Internal: takes already-split read/write halves, runs the handshake,
    /// then spawns the reader/writer/heartbeat tasks. Used by both
    /// [`connect`] and [`connect_tls`].
    async fn run_handshake_and_spawn(
        target: SocketAddr,
        mut reader: DynRead,
        writer: DynWrite,
        server_identity: Option<crate::auth::X509Credentials>,
        user: &str,
        host: &str,
        conn: ConnConfig,
    ) -> PvaResult<Arc<Self>> {
        let ConnConfig {
            op_timeout,
            tcp_timeout,
            max_message_size,
        } = conn;
        // Step 1+2: read handshake frames until we get CONNECTION_VALIDATION.
        let mut rx_buf: Vec<u8> = Vec::with_capacity(8192);
        let (byte_order, _server_buf, _server_reg, auth_methods) =
            read_handshake_init(&mut reader, &mut rx_buf, op_timeout, max_message_size).await?;

        // Choose auth method: prefer "ca" if offered.
        let negotiated_auth = select_client_auth(&auth_methods);

        // Step 3: send our CONNECTION_VALIDATION reply on the (still-not-spawned) writer.
        let mut writer_owned = writer;
        let reply = build_client_connection_validation(
            byte_order,
            DEFAULT_BUFFER_SIZE,
            DEFAULT_REGISTRY_SIZE,
            0,
            negotiated_auth,
            user,
            host,
        );
        timeout(op_timeout, writer_owned.write_all(&reply))
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(PvaError::Io)?;

        // Step 4: wait for CONNECTION_VALIDATED.
        wait_for_validated(&mut reader, &mut rx_buf, op_timeout, max_message_size).await?;

        // Spawn background tasks.
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let cancel = CancellationToken::new();
        // Outbound byte order as a shared, mutable per-connection cell,
        // seeded from the handshake SET_BYTE_ORDER. The reader task latches
        // a new value if the server sends another SET_BYTE_ORDER mid-stream
        // (pvxs conn.cpp:169-188 `sendBE`); op builders and the heartbeat
        // read it at each send. `true` = Big. Single writer (reader task),
        // many readers.
        let out_order = Arc::new(AtomicBool::new(byte_order.is_big()));
        let alive = Arc::new(AtomicBool::new(true));
        let last_rx_nanos = Arc::new(AtomicU64::new(now_nanos()));
        let bytes_rx = Arc::new(AtomicU64::new(0));
        let bytes_tx = Arc::new(AtomicU64::new(0));
        let by_ioid: Arc<DashMap<u32, IoidSlot>> = Arc::new(DashMap::new());
        let by_cid: Arc<DashMap<u32, oneshot::Sender<Frame>>> = Arc::new(DashMap::new());
        let by_sid_close: Arc<DashMap<u32, (Arc<AtomicBool>, Arc<tokio::sync::Notify>)>> =
            Arc::new(DashMap::new());
        let ioid_to_sid: Arc<DashMap<u32, u32>> = Arc::new(DashMap::new());
        let ioid_to_cmd: Arc<DashMap<u32, u8>> = Arc::new(DashMap::new());
        let op_introspection: Arc<DashMap<u32, crate::pvdata::FieldDesc>> =
            Arc::new(DashMap::new());
        let chan_stats: Arc<DashMap<u32, ChanStat>> = Arc::new(DashMap::new());

        // Writer task
        let cancel_writer = cancel.clone();
        let alive_writer = alive.clone();
        let bytes_tx_writer = bytes_tx.clone();
        epics_base_rs::runtime::task::spawn(async move {
            let mut batch = Vec::with_capacity(8192);
            loop {
                tokio::select! {
                    _ = cancel_writer.cancelled() => break,
                    msg = writer_rx.recv() => match msg {
                        Some(bytes) => {
                            batch.extend_from_slice(&bytes);
                            while let Ok(next) = writer_rx.try_recv() {
                                batch.extend_from_slice(&next);
                            }
                            if writer_owned.write_all(&batch).await.is_err() {
                                break;
                            }
                            // count bytes written to the socket.
                            bytes_tx_writer
                                .fetch_add(batch.len() as u64, Ordering::Relaxed);
                            batch.clear();
                        }
                        None => break,
                    }
                }
            }
            alive_writer.store(false, Ordering::SeqCst);
            cancel_writer.cancel();
        });

        // Reader task
        let cancel_reader = cancel.clone();
        let alive_reader = alive.clone();
        let last_rx_reader = last_rx_nanos.clone();
        let bytes_rx_reader = bytes_rx.clone();
        let by_ioid_reader = by_ioid.clone();
        let by_cid_reader = by_cid.clone();
        let by_sid_close_reader = by_sid_close.clone();
        let ioid_to_sid_reader = ioid_to_sid.clone();
        let ioid_to_cmd_reader = ioid_to_cmd.clone();
        let op_introspection_reader = op_introspection.clone();
        let chan_stats_reader = chan_stats.clone();
        let writer_tx_reader = writer_tx.clone();
        let out_order_reader = out_order.clone();
        epics_base_rs::runtime::task::spawn(async move {
            let mut buf = rx_buf;
            let mut chunk = vec![0u8; 4096];
            // client-side segmented-message reassembly. Mirror
            // of the server-side state machine. pvxs
            // sends large monitor events (NTNDArray frames, multi-MiB
            // arrays, big NTTable INIT descriptors) as
            // SegFirst..SegMiddle*..SegLast sequences; without
            // reassembly the client decodes each segment as if it
            // were a fresh complete frame, the IOID-routed receiver
            // gets garbage, and the application surfaces a Decode
            // error (or worse — wrong shape silently parsed).
            let mut seg_buf: Vec<u8> = Vec::new();
            let mut seg_cmd: u8 = 0;
            let mut seg_flags: crate::proto::HeaderFlags = crate::proto::HeaderFlags(0);
            let mut expect_seg = false;
            // Reader-task-owned type cache. Type-cache markers (0xFD
            // define / 0xFE reference) are resolved here, in strict wire
            // order, before frames are routed to per-op tasks — see
            // `flatten_type_cache_markers`. The per-op tasks then decode
            // self-contained frames, so a 0xFE reference can never be
            // decoded before the 0xFD define that fills its slot.
            let mut reader_type_cache = crate::pvdata::encode::TypeCache::new();
            loop {
                tokio::select! {
                    _ = cancel_reader.cancelled() => break,
                    res = reader.read(&mut chunk) => match res {
                        Ok(0) => {
                            debug!("server closed");
                            break;
                        }
                        Ok(n) => {
                            if let Err(e) = crate::peer_buf::try_extend(
                                &mut buf, &chunk[..n], "the connection receive buffer"
                            ) {
                                warn!(error = %e, "PVA client reader: closing");
                                cancel_reader.cancel();
                                return;
                            }
                            last_rx_reader.store(now_nanos(), Ordering::SeqCst);
                            // count bytes read off the socket.
                            bytes_rx_reader.fetch_add(n as u64, Ordering::Relaxed);
                            // Peek the header once we have 8 bytes — when
                            // the client opted into a cap, drop the
                            // connection if the announced payload exceeds
                            // it (`None` = unbounded, pvxs
                            // parity). Defends a hardened client against a
                            // compromised server announcing a 4 GiB header.
                            if buf.len() >= crate::proto::PvaHeader::SIZE {
                                // decode the prefix to enforce
                                // the payload cap. An undecodable
                                // header here would have been
                                // swallowed by `if let Ok` pre-fix —
                                // close the connection so the cap
                                // path is reachable for every header
                                // shape we receive. pvxs
                                // `conn.cpp:153-165` disconnects
                                // immediately on bad magic / zero
                                // version / direction-bit mismatch.
                                match crate::proto::PvaHeader::decode(
                                    &mut std::io::Cursor::new(&buf[..])
                                ) {
                                    Ok(hdr) => {
                                        // only enforce when the
                                        // client opted into a cap; `None`
                                        // is unbounded (pvxs parity).
                                        if let Some(cap) = max_message_size {
                                            if !hdr.flags.is_control()
                                                && hdr.payload_length as usize > cap
                                            {
                                                warn!(
                                                    payload = hdr.payload_length,
                                                    cap,
                                                    "PVA inbound payload exceeds cap, closing"
                                                );
                                                cancel_reader.cancel();
                                                return;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            error = %e,
                                            "PVA client reader: malformed header from server, closing"
                                        );
                                        cancel_reader.cancel();
                                        return;
                                    }
                                }
                            }
                            // split frame-parse result. `Ok(None)`
                            // keeps buffering for more bytes; `Ok(Some(..))`
                            // drains + dispatches; `Err(e)` closes the
                            // connection. Pre-fix `while let Ok(Some(..))`
                            // treated parse errors as "no complete frame
                            // yet", so a malformed prefix stayed pinned in
                            // `buf` (and could keep growing if the peer
                            // kept sending). Mirrors pvxs
                            // `conn.cpp:153-165` direction-bit disconnect.
                            //
                            // Role-aware parse: a client's inbound frames
                            // must have the Server direction bit SET.
                            loop {
                                let (frame, fn_) =
                                    match try_parse_frame_role(&buf, PeerRole::Client) {
                                        Ok(Some(pair)) => pair,
                                        Ok(None) => break, // need more bytes
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                "PVA client reader: frame parse failed, closing"
                                            );
                                            cancel_reader.cancel();
                                            return;
                                        }
                                    };
                                buf.drain(..fn_);
                                if frame.header.flags.is_control() {
                                    handle_control_frame(
                                        &frame,
                                        &writer_tx_reader,
                                        &out_order_reader,
                                    );
                                    continue;
                                }
                                // segmentation gate (mirrors
                                // server-side pvxs conn.cpp:
                                // 228-244). Validate continuation
                                // invariants; accumulate until
                                // SegLast (or unsegmented), then
                                // dispatch the synthetic Frame.
                                let raw_seg = frame.header.flags.0
                                    & crate::proto::HeaderFlags::SEGMENT_MASK;
                                let continuation = raw_seg
                                    & crate::proto::HeaderFlags::SEGMENT_LAST
                                    != 0;
                                if continuation ^ expect_seg
                                    || (continuation
                                        && frame.header.command != seg_cmd)
                                {
                                    warn!(
                                        expect_seg,
                                        continuation,
                                        cmd = frame.header.command,
                                        saved = seg_cmd,
                                        "PVA segmentation violation from server, closing"
                                    );
                                    cancel_reader.cancel();
                                    return;
                                }
                                if raw_seg == 0
                                    || raw_seg
                                        == crate::proto::HeaderFlags::SEGMENT_FIRST
                                {
                                    expect_seg = true;
                                    seg_cmd = frame.header.command;
                                    seg_flags = frame.header.flags;
                                    seg_buf.clear();
                                }
                                // Cap reassembly when the client opted
                                // into a cap; a peer that streams
                                // SegFirst → SegMiddle … forever would
                                // grow seg_buf without bound otherwise.
                                // `None` = unbounded (pvxs
                                // parity).
                                if let Some(cap) = max_message_size {
                                    if seg_buf.len().saturating_add(frame.payload.len()) > cap {
                                        warn!(
                                            accumulated = seg_buf.len(),
                                            next = frame.payload.len(),
                                            cap,
                                            "PVA reassembled message exceeds cap, closing"
                                        );
                                        cancel_reader.cancel();
                                        return;
                                    }
                                }
                                if let Err(e) = crate::peer_buf::try_extend(
                                    &mut seg_buf,
                                    &frame.payload,
                                    "the segment-reassembly buffer",
                                ) {
                                    warn!(error = %e, "PVA client reader: closing");
                                    cancel_reader.cancel();
                                    return;
                                }
                                if raw_seg != 0
                                    && raw_seg
                                        != crate::proto::HeaderFlags::SEGMENT_LAST
                                {
                                    continue;
                                }
                                expect_seg = false;
                                let mut dispatch_frame = if raw_seg == 0 {
                                    frame
                                } else {
                                    Frame {
                                        header: crate::proto::PvaHeader {
                                            version: frame.header.version,
                                            // Strip the segment bits — the
                                            // dispatch path expects an
                                            // unsegmented application frame.
                                            flags: crate::proto::HeaderFlags(
                                                seg_flags.0
                                                    & !crate::proto::HeaderFlags::SEGMENT_MASK,
                                            ),
                                            command: seg_cmd,
                                            payload_length: seg_buf.len() as u32,
                                        },
                                        payload: std::mem::take(&mut seg_buf),
                                    }
                                };
                                // Flatten type-cache markers in wire
                                // order before routing, so per-op tasks
                                // never decode a 0xFE reference ahead of
                                // its 0xFD define (cross-op decode order
                                // is not guaranteed).
                                crate::client_native::decode::flatten_type_cache_markers(
                                    &mut dispatch_frame,
                                    &mut reader_type_cache,
                                    &op_introspection_reader,
                                );
                                route_frame(dispatch_frame, &by_ioid_reader, &by_cid_reader, &by_sid_close_reader, &ioid_to_sid_reader, &ioid_to_cmd_reader, &chan_stats_reader, &writer_tx_reader, &cancel_reader);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            alive_reader.store(false, Ordering::SeqCst);
            cancel_reader.cancel();
            // Drain the router — drops all per-ioid senders so any
            // outstanding `stream.recv().await` (e.g. monitor loops)
            // wakes with `None` and can react to the disconnect.
            // Also clear `by_sid_close` and `ioid_to_sid`: the conn
            // is dying, so no further DESTROY_CHANNEL frames will
            // fire those signals — leaving the entries pinned would
            // be a small leak the next reconnect would have to
            // recover via stale-sid detection in is_active().
            by_ioid_reader.clear();
            by_cid_reader.clear();
            by_sid_close_reader.clear();
            ioid_to_sid_reader.clear();
            ioid_to_cmd_reader.clear();
            // Drop every captured op introspection — the conn is dying and
            // no further DATA frames will reference it. Keeps the reader the
            // single owner of this map's lifecycle on teardown.
            op_introspection_reader.clear();
            // pvxs drops the per-channel counters with the connection
            // (Channel objects are torn down on disconnect); clear them so
            // a reconnect's fresh SIDs start from zero.
            chan_stats_reader.clear();
        });

        // Heartbeat task
        let cancel_hb = cancel.clone();
        let alive_hb = alive.clone();
        let last_rx_hb = last_rx_nanos.clone();
        let writer_tx_hb = writer_tx.clone();
        let out_order_hb = out_order.clone();
        epics_base_rs::runtime::task::spawn(async move {
            // pvxs clientconn.cpp:163-165: echo interval = max(1, min(15, tcpTimeout * 3/8))
            // pvxs clientconn.cpp:73-74: socket inactivity timeout = tcpTimeout
            let hb_interval = Duration::from_secs_f64(crate::config::env::echo_period_secs(
                tcp_timeout.as_secs_f64(),
            ));
            let hb_timeout = tcp_timeout;
            let mut tick = interval(hb_interval);
            tick.tick().await; // skip first immediate tick
            loop {
                tokio::select! {
                    _ = cancel_hb.cancelled() => break,
                    _ = tick.tick() => {
                        // Liveness check: are we receiving anything?
                        let last = last_rx_hb.load(Ordering::SeqCst);
                        let elapsed = now_nanos().saturating_sub(last);
                        if elapsed > hb_timeout.as_nanos() as u64 {
                            warn!("PVA connection idle > {hb_timeout:?}, closing");
                            break;
                        }
                        // Heartbeat probe = application CMD_ECHO with an
                        // empty payload, matching pvxs clientconn.cpp:496
                        // (`Header{CMD_ECHO, 0, 0}`); the server echoes it
                        // back (serverconn.cpp:166-178). A *control*
                        // EchoRequest is drained and ignored by pvxs
                        // (conn.cpp:180-194), so on an idle
                        // Rust-client→pvxs-server link the probe drew no
                        // reply and `last_rx` went stale, tearing down a
                        // healthy connection. Any inbound frame — including
                        // the echo reply — refreshes `last_rx`; control echo
                        // stays supported inbound as a Rust-only extension.
                        // Read the current outbound order — the server may
                        // have re-negotiated it via a mid-stream
                        // SET_BYTE_ORDER (pvxs conn.cpp:169-188).
                        let order_hb = if out_order_hb.load(Ordering::Relaxed) {
                            ByteOrder::Big
                        } else {
                            ByteOrder::Little
                        };
                        let h = PvaHeader::application(false, order_hb, Command::Echo.code(), 0);
                        let mut bytes = Vec::with_capacity(8);
                        h.write_into(&mut bytes);
                        if writer_tx_hb.send(bytes).is_err() {
                            break;
                        }
                    }
                }
            }
            alive_hb.store(false, Ordering::SeqCst);
            cancel_hb.cancel();
        });

        Ok(Arc::new(Self {
            addr: target,
            out_order,
            server_identity,
            writer_tx,
            cancel,
            alive,
            last_rx_nanos,
            bytes_rx,
            bytes_tx,
            by_ioid,
            by_cid,
            by_sid_close,
            ioid_to_sid,
            ioid_to_cmd,
            op_introspection,
            chan_stats,
        }))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Current negotiated outbound byte order. Latched from the server's
    /// most recent SET_BYTE_ORDER control frame (pvxs conn.cpp:169-188
    /// `sendBE`). Every op builder reads this per-send rather than caching
    /// a handshake-time value, so a mid-connection re-negotiation is
    /// honoured on the very next outbound frame.
    pub fn byte_order(&self) -> ByteOrder {
        if self.out_order.load(Ordering::Relaxed) {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        }
    }

    /// snapshot `(bytes_rx, bytes_tx)` for this connection,
    /// optionally zeroing them after the read (pvxs `report(bool zero)`
    /// delta semantics).
    pub fn byte_counters(&self, zero: bool) -> (u64, u64) {
        if zero {
            // `swap(0)` reads the exact pre-reset count and clears it in
            // one atomic step. A `load` then `store(0)` would drop any
            // increment the reader/writer IO tasks `fetch_add` between
            // the read and the store — neither reported in this delta nor
            // carried into the next.
            (
                self.bytes_rx.swap(0, Ordering::Relaxed),
                self.bytes_tx.swap(0, Ordering::Relaxed),
            )
        } else {
            (
                self.bytes_rx.load(Ordering::Relaxed),
                self.bytes_tx.load(Ordering::Relaxed),
            )
        }
    }

    /// Register a channel's PV name under its server-assigned SID so this
    /// connection can attribute per-channel byte traffic for
    /// `PvaClient::report` (pvxs adds the channel to `conn->chanBySID`,
    /// client.cpp:495). Idempotent: re-registering the same SID refreshes
    /// the name and keeps existing counters.
    pub fn register_channel(&self, sid: u32, name: &str) {
        self.chan_stats.entry(sid).or_insert_with(|| ChanStat {
            name: name.to_owned(),
            rx: AtomicU64::new(0),
            tx: AtomicU64::new(0),
        });
    }

    /// Drop a channel's per-channel counters when it leaves this
    /// connection (reconnect to a different server, explicit close, or a
    /// server-initiated DESTROY_CHANNEL). pvxs removes the channel from
    /// `chanBySID` on the same transitions.
    pub fn unregister_channel(&self, sid: u32) {
        self.chan_stats.remove(&sid);
    }

    /// Per-channel `(name, sid, bytes_rx, bytes_tx)` snapshot for
    /// `PvaClient::report`. When `zero` is true each channel's counters are
    /// reset after the read, matching the connection-level
    /// [`Self::byte_counters`] delta semantics and pvxs
    /// `Context::report(zero)` (client.cpp:499).
    pub fn channel_reports(&self, zero: bool) -> Vec<(String, u32, u64, u64)> {
        self.chan_stats
            .iter()
            .map(|e| {
                let s = e.value();
                let (rx, tx) = if zero {
                    (
                        s.rx.swap(0, Ordering::Relaxed),
                        s.tx.swap(0, Ordering::Relaxed),
                    )
                } else {
                    (s.rx.load(Ordering::Relaxed), s.tx.load(Ordering::Relaxed))
                };
                (s.name.clone(), *e.key(), rx, tx)
            })
            .collect()
    }

    /// The server peer's verified X.509 identity, or `None` for a
    /// plain `pva://` connection (or a `pvas://` server presenting no
    /// usable certificate). Mirrors pvxs `Connected::cred` — the
    /// `account` / `authority` `pvxinfo -v` prints as the server's
    /// credentials.
    pub fn server_identity(&self) -> Option<&crate::auth::X509Credentials> {
        self.server_identity.as_ref()
    }

    /// True iff this is a TLS (`pvas://`) connection. Inferred from a
    /// present server X.509 identity — the identity is only populated
    /// after a successful TLS handshake.
    pub fn is_tls(&self) -> bool {
        self.server_identity.is_some()
    }

    pub fn close(&self) {
        self.cancel.cancel();
        self.alive.store(false, Ordering::SeqCst);
    }

    /// Send a fully-built frame (synchronous — no .await needed).
    ///
    /// The writer channel is unbounded so this never blocks. Frames are
    /// batched and flushed by the writer task. This matches CA's
    /// `DirectServerWriter::send_frame` pattern.
    pub fn send_sync(&self, frame: Vec<u8>) -> PvaResult<()> {
        if !self.is_alive() {
            return Err(PvaError::Protocol("server connection closed".into()));
        }
        self.writer_tx
            .send(frame)
            .map_err(|_| PvaError::Protocol("writer queue closed".into()))
    }

    /// Async wrapper around [`Self::send_sync`] for backward compatibility.
    /// New code should prefer `send_sync` to avoid unnecessary async overhead.
    pub async fn send(&self, frame: Vec<u8>) -> PvaResult<()> {
        self.send_sync(frame)
    }

    /// Like [`Self::send_sync`] but attributes the frame's wire length to
    /// the channel `sid`'s transmit counter first. Mirrors pvxs
    /// `chan->statTx += conn->enqueueTxBody(...)` at every op-body send
    /// (clientget.cpp:321/354, clientmon.cpp:143/350/451,
    /// clientintrospect.cpp:93). The connection-level `bytes_tx` is still
    /// bumped by the writer task, so the per-channel counter is a subset of
    /// the aggregate — exactly as pvxs keeps both. A frame for an
    /// unregistered SID just bumps the connection aggregate.
    pub fn send_for_channel_sync(&self, sid: u32, frame: Vec<u8>) -> PvaResult<()> {
        if let Some(s) = self.chan_stats.get(&sid) {
            s.tx.fetch_add(frame.len() as u64, Ordering::Relaxed);
        }
        self.send_sync(frame)
    }

    /// Async wrapper around [`Self::send_for_channel_sync`].
    pub async fn send_for_channel(&self, sid: u32, frame: Vec<u8>) -> PvaResult<()> {
        self.send_for_channel_sync(sid, frame)
    }

    /// Best-effort, non-blocking enqueue. Returns `false` if the
    /// connection has shut down.
    pub fn try_send(&self, frame: Vec<u8>) -> bool {
        if !self.is_alive() {
            return false;
        }
        self.writer_tx.send(frame).is_ok()
    }

    /// Register a one-shot waiter for a CREATE_CHANNEL response.
    pub fn register_cid_waiter(&self, cid: u32) -> oneshot::Receiver<Frame> {
        let (tx, rx) = oneshot::channel();
        self.by_cid.insert(cid, tx);
        rx
    }

    /// Register two oneshot receivers for a pipelined GET/PUT/RPC op.
    ///
    /// The server sends two responses (INIT + DATA) for the same ioid.
    /// The reader task pops oneshots FIFO: first frame → first oneshot,
    /// second frame → second oneshot. This avoids creating an
    /// `unbounded_channel` per GET (heap allocation + vtable dispatch).
    pub fn register_ioid_twoshot(
        &self,
        sid: u32,
        ioid: u32,
        expected_cmd: u8,
    ) -> (oneshot::Receiver<Frame>, oneshot::Receiver<Frame>) {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        let mut q = VecDeque::with_capacity(2);
        q.push_back(tx1);
        q.push_back(tx2);
        self.by_ioid.insert(ioid, IoidSlot::TwoShot(q));
        self.ioid_to_sid.insert(ioid, sid);
        self.ioid_to_cmd.insert(ioid, expected_cmd);
        (rx1, rx2)
    }

    /// Register a stream of frames matching a particular ioid (MONITOR).
    pub fn register_ioid_stream(
        &self,
        sid: u32,
        ioid: u32,
        expected_cmd: u8,
    ) -> mpsc::UnboundedReceiver<Frame> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.by_ioid.insert(ioid, IoidSlot::Stream(tx));
        self.ioid_to_sid.insert(ioid, sid);
        self.ioid_to_cmd.insert(ioid, expected_cmd);
        rx
    }

    /// Register a reusable single-frame slot for warm-GET reuse.
    ///
    /// Caller keeps the returned `Arc<Mutex<Option<oneshot>>>` and
    /// refills it with a fresh oneshot before each warm-GET frame
    /// send. The reader task `take()`s the current sender on every
    /// matching frame. The slot itself stays in `by_ioid` until
    /// explicitly unregistered (e.g. on channel teardown).
    pub fn register_ioid_reusable(
        &self,
        sid: u32,
        ioid: u32,
        expected_cmd: u8,
    ) -> Arc<Mutex<Option<oneshot::Sender<Frame>>>> {
        let slot = Arc::new(Mutex::new(None));
        self.by_ioid.insert(ioid, IoidSlot::Reusable(slot.clone()));
        self.ioid_to_sid.insert(ioid, sid);
        self.ioid_to_cmd.insert(ioid, expected_cmd);
        slot
    }

    pub fn unregister_ioid(&self, ioid: u32) {
        self.by_ioid.remove(&ioid);
        self.ioid_to_sid.remove(&ioid);
        self.ioid_to_cmd.remove(&ioid);
        // Drop the reader-captured introspection for this op. The reader is
        // otherwise the only mutator (insert on INIT, remove on MONITOR
        // FINISH); this covers GET/PUT/RPC and any op torn down before a
        // FINISH so the map cannot leak across the op's lifetime.
        self.op_introspection.remove(&ioid);
    }

    pub fn register_sid_close(
        &self,
        sid: u32,
        flag: Arc<AtomicBool>,
        notify: Arc<tokio::sync::Notify>,
    ) {
        self.by_sid_close.insert(sid, (flag, notify));
    }

    pub fn unregister_sid_close(&self, sid: u32) {
        self.by_sid_close.remove(&sid);
    }

    /// Wait for the connection to terminate (returns when reader/writer/heartbeat
    /// all have stopped).
    pub async fn wait_closed(&self) {
        self.cancel.cancelled().await;
    }

    /// Time elapsed since the last incoming byte.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_rx_nanos.load(Ordering::SeqCst);
        let now = now_nanos();
        Duration::from_nanos(now.saturating_sub(last))
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

// match pvxs clientconn.cpp:292-293 — serverReceiveBufferSize = 0x10000 ("not used").
pub(super) const DEFAULT_BUFFER_SIZE: u32 = 0x10000;
pub(super) const DEFAULT_REGISTRY_SIZE: u16 = 32_767;

/// Select the client-side auth method from the server's advertised list,
/// preferring `"ca"` when offered and falling back to `"anonymous"`. pvxs
/// `Connection::handle_CONNECTION_VALIDATION` scans the advertised methods and
/// selects `"ca"` when present (clientconn.cpp:215-263), with no name-server
/// exception — so both the normal TCP path and the name-server path must use
/// this same rule rather than a separate hard-coded policy.
pub(super) fn select_client_auth(auth_methods: &[String]) -> &'static str {
    if auth_methods.iter().any(|m| m == "ca") {
        "ca"
    } else {
        "anonymous"
    }
}

fn now_nanos() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub(super) async fn read_handshake_init<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    rx_buf: &mut Vec<u8>,
    op_timeout: Duration,
    max_message_size: Option<usize>,
) -> PvaResult<(ByteOrder, u32, u16, Vec<String>)> {
    let mut byte_order = ByteOrder::Little;
    loop {
        let frame = read_one_frame(reader, rx_buf, op_timeout, max_message_size).await?;
        if frame.header.flags.is_control() {
            if frame.header.command == ControlCommand::SetByteOrder.code() {
                byte_order = frame.header.flags.byte_order();
            }
            continue;
        }
        if frame.header.command == Command::ConnectionValidation.code() {
            let req = decode_connection_validation_request(&frame)?;
            return Ok((
                byte_order,
                req.server_buffer_size,
                req.server_registry_size,
                req.auth_methods,
            ));
        }
    }
}

pub(super) async fn wait_for_validated<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    rx_buf: &mut Vec<u8>,
    op_timeout: Duration,
    max_message_size: Option<usize>,
) -> PvaResult<()> {
    loop {
        let frame = read_one_frame(reader, rx_buf, op_timeout, max_message_size).await?;
        if frame.header.flags.is_control() {
            continue;
        }
        if frame.header.command == Command::ConnectionValidated.code() {
            // pvxs `clientconn.cpp:303-313`: a non-success
            // CONNECTION_VALIDATED means the server refused the offered
            // credentials, but pvxs logs "Trying to proceed w/o cred" and
            // proceeds anyway (`ready = true; createChannels()`) — the
            // server may still serve PVs anonymously. Only a malformed
            // frame (`!M.good()`) disconnects, which here is the `?`
            // decode error below. Hard-failing on non-success instead
            // left a Rust client unable to reach a refuse-cred-serve-anon
            // server: the connection tore down and reconnected forever.
            let st = decode_connection_validated(&frame)?;
            if !st.is_success() {
                warn!("server refused auth ({st:?}); proceeding without credentials");
            }
            return Ok(());
        }
    }
}

async fn read_one_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    rx_buf: &mut Vec<u8>,
    op_timeout: Duration,
    max_message_size: Option<usize>,
) -> PvaResult<Frame> {
    loop {
        // Role-aware: read_one_frame is used by client connections, so
        // require the Server direction bit on inbound frames (pvxs
        // `conn.cpp:160` parity).
        if let Some((frame, n)) = try_parse_frame_role(rx_buf, PeerRole::Client)? {
            rx_buf.drain(..n);
            return Ok(frame);
        }
        // Same opt-in payload peek as the streaming reader.
        // `None` = unbounded (pvxs parity); the handshake
        // read is `op_timeout`-deadlined regardless.
        if let Some(cap) = max_message_size {
            if rx_buf.len() >= crate::proto::PvaHeader::SIZE {
                if let Ok(hdr) =
                    crate::proto::PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[..]))
                {
                    if !hdr.flags.is_control() && hdr.payload_length as usize > cap {
                        return Err(PvaError::Protocol(format!(
                            "inbound payload {} exceeds max_message_size {}",
                            hdr.payload_length, cap
                        )));
                    }
                }
            }
        }
        let mut chunk = [0u8; 4096];
        let n = match timeout(op_timeout, reader.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(PvaError::Io(e)),
            Err(_) => return Err(PvaError::Timeout),
        };
        if n == 0 {
            return Err(PvaError::Protocol("server closed during handshake".into()));
        }
        crate::peer_buf::try_extend(rx_buf, &chunk[..n], "the connection receive buffer")?;
    }
}

fn handle_control_frame(
    frame: &Frame,
    writer_tx: &mpsc::UnboundedSender<Vec<u8>>,
    out_order: &Arc<AtomicBool>,
) {
    // A server may re-negotiate the connection byte order mid-stream with
    // another SET_BYTE_ORDER control frame. pvxs latches
    // `sendBE = header[2] & pva_flags::MSB` on every received SetEndian
    // (conn.cpp:169-188) and uses it for all subsequent sends; old
    // pvAccess accepts it from either peer at any time. Latch it here so
    // every later outbound frame (echo response, ops, heartbeat) adopts
    // the new order. The flag is read from the control frame's own header
    // bit 7 (`frame.order()`), not the size field — pvAccessCPP/Java
    // ignore the size field and assume the 0x00000000 ("use this order")
    // behaviour.
    if frame.header.command == ControlCommand::SetByteOrder.code() {
        out_order.store(frame.order().is_big(), Ordering::Relaxed);
        return;
    }
    if frame.header.command == ControlCommand::EchoRequest.code() {
        // Server pinged us — bounce back. Direct unbounded send: no
        // scheduler hop, mirrors the CA `DirectServerWriter` pattern.
        let order = if out_order.load(Ordering::Relaxed) {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        };
        let resp = PvaHeader::control(
            false,
            order,
            ControlCommand::EchoResponse.code(),
            frame.header.payload_length,
        );
        let mut bytes = Vec::with_capacity(8);
        resp.write_into(&mut bytes);
        let _ = writer_tx.send(bytes);
    }
    // Other control messages (SetMarker, AckMarker, EchoResponse) update
    // last_rx implicitly; no further action.
}

/// A server frame pvxs would refuse to process: its handler hits
/// `!M.good()` (or an explicit `M.fault()`) and the connection is torn down
/// (`bev.reset()` at each client handler, or the `catch` around the command
/// switch in `conn.cpp:277-281`). The reason is log-only, exactly as in pvxs.
#[derive(Debug)]
struct FrameFault(String);

/// Route one server frame, tearing the circuit down on a protocol fault.
///
/// **Invariant:** a frame whose routing key or payload cannot be decoded MUST
/// NOT be swallowed — pvxs disconnects (`clientconn.cpp:334-338,417-421,
/// 454-455`, `clientget.cpp:490-494`), so the port must too, or a
/// non-conforming server keeps a half-understood circuit alive. This function
/// is the single owner of that teardown; every handler below signals the fault
/// by returning [`FrameFault`] and never cancels on its own.
#[allow(clippy::too_many_arguments)]
fn route_frame(
    frame: Frame,
    by_ioid: &Arc<DashMap<u32, IoidSlot>>,
    by_cid: &Arc<DashMap<u32, oneshot::Sender<Frame>>>,
    by_sid_close: &Arc<DashMap<u32, (Arc<AtomicBool>, Arc<tokio::sync::Notify>)>>,
    ioid_to_sid: &Arc<DashMap<u32, u32>>,
    ioid_to_cmd: &Arc<DashMap<u32, u8>>,
    chan_stats: &Arc<DashMap<u32, ChanStat>>,
    writer_tx: &mpsc::UnboundedSender<Vec<u8>>,
    cancel: &CancellationToken,
) {
    let cmd = frame.header.command;
    if let Err(FrameFault(reason)) = route_frame_checked(
        frame,
        by_ioid,
        by_cid,
        by_sid_close,
        ioid_to_sid,
        ioid_to_cmd,
        chan_stats,
        writer_tx,
    ) {
        tracing::warn!(
            cmd,
            reason,
            "PVA client router: server sent an invalid frame, closing circuit"
        );
        cancel.cancel();
    }
}

#[allow(clippy::too_many_arguments)]
fn route_frame_checked(
    frame: Frame,
    by_ioid: &Arc<DashMap<u32, IoidSlot>>,
    by_cid: &Arc<DashMap<u32, oneshot::Sender<Frame>>>,
    by_sid_close: &Arc<DashMap<u32, (Arc<AtomicBool>, Arc<tokio::sync::Notify>)>>,
    ioid_to_sid: &Arc<DashMap<u32, u32>>,
    ioid_to_cmd: &Arc<DashMap<u32, u8>>,
    chan_stats: &Arc<DashMap<u32, ChanStat>>,
    writer_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(), FrameFault> {
    let cmd = frame.header.command;
    // pvxs attributes the received frame's wire length to the owning
    // channel's `statRx` at op reply decode (clientget.cpp:496,
    // clientmon.cpp:608, clientintrospect.cpp:151). We do the same here:
    // any frame carrying an IOID we can resolve to a SID adds its wire
    // length (8-byte header + payload) to that channel's receive counter.
    // The connection-level `bytes_rx` (socket aggregate) is unchanged.
    let rx_wire_len = (PvaHeader::SIZE + frame.payload.len()) as u64;
    // Route by THIS frame's header byte order, not the startup handshake
    // order. pvxs sets `peerBE = header[2]&pva_flags::MSB` per received
    // application frame before command dispatch (conn.cpp:195-198) and
    // reads every routing key (CREATE_CHANNEL CID/SID, GET/PUT/RPC/MONITOR/
    // GET_FIELD IOID) from `EvInBuf M(peerBE, ...)`. A server may legally
    // send a response whose header selects a different order than the
    // handshake; peeking the CID/IOID with the stale connection order would
    // miss the slot, misroute, or trip the command-mismatch close path.
    let order = frame.order();

    // CMD_MESSAGE — log server diagnostic, don't route by IOID. A frame
    // that does not decode is a fault: pvxs `handle_MESSAGE` throws on
    // `!M.good()` (clientconn.cpp:454-455) and the catch resets the bev.
    if cmd == Command::Message.code() {
        return log_server_message(&frame.payload, order);
    }

    // CMD_DESTROY_CHANNEL from server.
    if cmd == Command::DestroyChannel.code() {
        let Some(sid) = peek_u32(&frame.payload, 0, order) else {
            // pvxs clientconn.cpp:417-421 — invalid DESTROY_CHANNEL,
            // "Disconnecting...".
            return Err(FrameFault(
                "DESTROY_CHANNEL payload too short for SID".into(),
            ));
        };
        {
            let mut dropped_ioids = 0usize;
            // Collect matching ioids first, then remove.
            let matching: Vec<u32> = ioid_to_sid
                .iter()
                .filter(|r| *r.value() == sid)
                .map(|r| *r.key())
                .collect();
            for ioid in &matching {
                // CMD_DESTROY_CHANNEL cleanup is the same owner
                // boundary as `unregister_ioid` and must remove ALL three
                // IOID maps. Leaving `ioid_to_cmd` behind leaks a stale
                // command expectation for the connection's lifetime, and
                // a late frame on a destroyed IOID would hit the
                // command-mismatch gate (line ~966) and cancel the whole
                // connection before discovering that no dispatch slot
                // exists.
                ioid_to_sid.remove(ioid);
                by_ioid.remove(ioid);
                ioid_to_cmd.remove(ioid);
                dropped_ioids += 1;
            }
            // Fire the close signal.
            if let Some((_, (flag, notify))) = by_sid_close.remove(&sid) {
                flag.store(true, Ordering::Relaxed);
                notify.notify_waiters();
                tracing::warn!(
                    sid,
                    dropped_ioids,
                    "server destroyed channel — triggering re-search"
                );
            } else {
                tracing::debug!(sid, "server destroyed unknown channel (already torn down?)");
            }
            // CMD_DESTROY_CHANNEL is the single owner of per-SID client-side
            // teardown: drop this SID's `ClientReport` counters too, matching
            // pvxs `Channel::disconnect()` (called on disconnect OR
            // CMD_DESTROY_CHANNEL) which does `current->chanBySID.erase(sid)`
            // in the Active case (client.cpp:149,170). Without this the report
            // still lists the destroyed channel until the next ChannelState
            // transition, and a same-connection SID reuse keeps the stale
            // name/counters because `register_channel` is `or_insert_with`.
            // The other per-SID teardown owners already drop it (set_state via
            // `unregister_channel`, connection drop via `chan_stats.clear()`);
            // this closes the last bypass.
            chan_stats.remove(&sid);
        }
        return Ok(());
    }

    // CREATE_CHANNEL responses route by CID.
    if cmd == Command::CreateChannel.code() {
        let Some(cid) = peek_u32(&frame.payload, 0, order) else {
            // pvxs clientconn.cpp:334-338 — invalid CREATE_CHANNEL,
            // "Disconnecting...".
            return Err(FrameFault(
                "CREATE_CHANNEL payload too short for CID".into(),
            ));
        };
        {
            if let Some((_, tx)) = by_cid.remove(&cid) {
                // even when we have a waiter, the receiver
                // might have already been dropped (timeout race).
                // pvxs `clientconn.cpp:359-379` checks the same case
                // and on Status::isSuccess sends CMD_DESTROY_CHANNEL
                // for the stale channel. The send to the waiter is
                // best-effort; on Err, the receiver is gone → emit
                // the destroy.
                if let Err(rejected_frame) = tx.send(frame) {
                    maybe_destroy_stale_create_channel(&rejected_frame, cid, writer_tx, order);
                }
                return Ok(());
            }
            // no waiter at all — the caller timed out,
            // dropped its receiver, and CID was already evicted.
            // pvxs still sends DESTROY_CHANNEL so the server's
            // ChannelState is reaped. Pre-fix Rust silently dropped
            // the frame and left the server-side channel open until
            // TCP close.
            maybe_destroy_stale_create_channel(&frame, cid, writer_tx, order);
            return Ok(());
        }
    }

    // Application op responses (GET/PUT/MONITOR/RPC/GET_FIELD) route by IOID.
    // Every one of those pvxs handlers reads the IOID off the wire first and
    // disconnects when the payload cannot carry it (`!M.good()` →
    // "sends invalid op%02x. Disconnecting...", clientget.cpp:490-494;
    // clientmon.cpp / clientintrospect.cpp do the same). Any OTHER command
    // stays ignorable: pvxs's dispatch switch drains an unexpected command's
    // body and continues, for forward compatibility (conn.cpp:250-252).
    let Some(ioid) = peek_u32(&frame.payload, 0, order) else {
        return if is_op_reply(cmd) {
            Err(FrameFault(format!(
                "op reply payload too short for IOID (cmd {cmd})"
            )))
        } else {
            Ok(())
        };
    };
    {
        // Attribute this reply's wire length to the owning channel's
        // receive counter (pvxs `chan->statRx += rxlen`). The IOID→SID map
        // resolves the channel; unmapped IOIDs (already torn down) just
        // miss the per-channel counter, leaving the connection aggregate.
        if let Some(sid) = ioid_to_sid.get(&ioid).map(|r| *r.value()) {
            if let Some(s) = chan_stats.get(&sid) {
                s.rx.fetch_add(rx_wire_len, Ordering::Relaxed);
            }
        }
        // verify the incoming frame's command matches the
        // command the IOID was opened with. Mirrors pvxs
        // `clientget.cpp:463-470` / `clientmon.cpp:570-579` per-op
        // command checks. A mismatch is protocol-fatal: cancel the
        // connection. Pre-fix Rust delivered any-cmd to the sink
        // matched by IOID alone — a buggy/malicious server could
        // satisfy a GET with a MONITOR-shaped frame.
        if let Some(expected) = ioid_to_cmd.get(&ioid).map(|r| *r.value()) {
            if expected != cmd {
                return Err(FrameFault(format!(
                    "frame command {cmd} does not match IOID {ioid}'s command {expected}"
                )));
            }
        }
        // Try to dispatch. For TwoShot, pop the first available oneshot.
        // For Stream, send to the unbounded channel.
        if let Some(mut entry) = by_ioid.get_mut(&ioid) {
            match entry.value_mut() {
                IoidSlot::TwoShot(q) => {
                    if let Some(tx) = q.pop_front() {
                        let _ = tx.send(frame);
                    }
                    // If queue is now empty, remove the entry entirely.
                    if q.is_empty() {
                        drop(entry);
                        by_ioid.remove(&ioid);
                    }
                }
                IoidSlot::Stream(tx) => {
                    let _ = tx.send(frame);
                }
                IoidSlot::Reusable(slot) => {
                    // Take the current sender (if any) and fulfil it.
                    // The slot itself stays registered — next warm
                    // GET will refill it.
                    if let Some(tx) = slot.lock().take() {
                        let _ = tx.send(frame);
                    }
                }
            }
        }
    }
    // An IOID with no dispatch slot drops silently (the op completed or was
    // cancelled; Beacons/SearchResponse are handled out-of-band by the search
    // engine, not here).
    Ok(())
}

/// Commands whose client-side pvxs handler routes by a leading IOID and
/// disconnects on a decode fault. Anything else the server sends is
/// forward-compatible filler that pvxs drains and ignores (conn.cpp:250-252).
fn is_op_reply(cmd: u8) -> bool {
    cmd == Command::Get.code()
        || cmd == Command::Put.code()
        || cmd == Command::PutGet.code()
        || cmd == Command::Monitor.code()
        || cmd == Command::Rpc.code()
        || cmd == Command::GetField.code()
}

/// Log a server-side CMD_MESSAGE at the level matching its mtype.
/// Payload layout: `ioid:u32 + mtype:u8 + message:PVA-string`.
///
/// pvxs decodes all three fields and throws when any is short or malformed
/// (`handle_MESSAGE`, clientconn.cpp:442-455) — the message is diagnostic, but
/// a server that cannot frame it has corrupted the stream, so the circuit
/// goes down rather than serving on.
fn log_server_message(payload: &[u8], order: ByteOrder) -> Result<(), FrameFault> {
    let mut cur = std::io::Cursor::new(payload);
    let Ok(ioid) = cur.get_u32(order) else {
        return Err(FrameFault("MESSAGE payload too short for IOID".into()));
    };
    let Ok(mtype) = cur.get_u8() else {
        return Err(FrameFault("MESSAGE payload too short for mtype".into()));
    };
    // `from_wire(M, msg)` — a truncated or oversized string length faults
    // `M`; a null string (0xFF) is not valid here either (pvxs's
    // `from_wire(std::string&)` faults on it), so both are decode errors.
    let Ok(Some(msg)) = decode_string(&mut cur, order) else {
        return Err(FrameFault(
            "MESSAGE payload carries no decodable string".into(),
        ));
    };
    // pvxs `handle_MESSAGE` maps the level through `mtype2level`
    // (pvaproto.h:704-712, clientconn.cpp:457): 0 -> Info, 1 -> Warn,
    // 2 -> Err, and default (Fatal=3 and every unknown value) -> Crit.
    // tracing has no Crit level, so Err, Fatal, and unknown types all map
    // to its highest level, error! — an unknown type must escalate, not
    // be downgraded to warn!.
    match mtype {
        x if x == MessageType::Info as u8 => {
            tracing::info!(ioid, msg, "server MESSAGE")
        }
        x if x == MessageType::Warning as u8 => {
            tracing::warn!(ioid, msg, "server MESSAGE")
        }
        other => {
            tracing::error!(ioid, mtype = other, msg, "server MESSAGE")
        }
    }
    Ok(())
}

fn peek_u32(payload: &[u8], offset: usize, order: ByteOrder) -> Option<u32> {
    if payload.len() < offset + 4 {
        return None;
    }
    let bytes: [u8; 4] = payload[offset..offset + 4].try_into().ok()?;
    Some(match order {
        ByteOrder::Big => u32::from_be_bytes(bytes),
        ByteOrder::Little => u32::from_le_bytes(bytes),
    })
}

/// when a CREATE_CHANNEL response arrives with no waiter
/// (the caller timed out or dropped its receiver), check the
/// status. If success, the server has a live channel we'll never
/// use — send CMD_DESTROY_CHANNEL to release the server-side
/// state. Mirrors pvxs `clientconn.cpp:359-379`.
///
/// Frame payload layout: `cid:u32 + sid:u32 + status`. Status
/// success means we have a live (sid, cid) pair to destroy.
fn maybe_destroy_stale_create_channel(
    frame: &Frame,
    cid: u32,
    writer_tx: &mpsc::UnboundedSender<Vec<u8>>,
    order: ByteOrder,
) {
    let payload = &frame.payload;
    // sid at offset 4
    let Some(sid) = peek_u32(payload, 4, order) else {
        return;
    };
    // Status starts at offset 8. Minimum status shape is one byte
    // (Status::Ok inline form). pvxs `Status::isSuccess` returns
    // true when the status type byte is 0xFF (OK inline) or the
    // wire status code is 0.
    if payload.len() < 9 {
        return;
    }
    let status_byte = payload[8];
    // Status: 0xFF = OK inline (success). Other shapes carry a code.
    // We only act on the unambiguous OK case — for non-OK statuses
    // there's no live channel to destroy.
    if status_byte != 0xFF {
        return;
    }
    tracing::debug!(
        sid,
        cid,
        "PVA client: late CREATE_CHANNEL success after waiter gone — sending DESTROY_CHANNEL"
    );
    // Build CMD_DESTROY_CHANNEL frame: header + (sid + cid).
    let mut payload_out: Vec<u8> = Vec::with_capacity(8);
    let sid_bytes = match order {
        ByteOrder::Big => sid.to_be_bytes(),
        ByteOrder::Little => sid.to_le_bytes(),
    };
    let cid_bytes = match order {
        ByteOrder::Big => cid.to_be_bytes(),
        ByteOrder::Little => cid.to_le_bytes(),
    };
    payload_out.extend_from_slice(&sid_bytes);
    payload_out.extend_from_slice(&cid_bytes);
    let header = PvaHeader::application(false, order, Command::DestroyChannel.code(), 8);
    let mut frame_out: Vec<u8> = Vec::with_capacity(PvaHeader::SIZE + 8);
    header.write_into(&mut frame_out);
    frame_out.extend_from_slice(&payload_out);
    let _ = writer_tx.send(frame_out);
}

pub(super) fn build_client_connection_validation(
    order: ByteOrder,
    buffer_size: u32,
    registry_size: u16,
    qos: u16,
    auth: &str,
    user: &str,
    host: &str,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(buffer_size, order);
    payload.put_u16(registry_size, order);
    payload.put_u16(qos, order);
    encode_string_into(auth, order, &mut payload);

    // pvxs always reads a Variant payload after the auth method string —
    // even for "anonymous". Send the null-variant marker (0xFF) for
    // anonymous, or an inline structure with user/host[/groups] for
    // "ca". The optional `groups` field carries POSIX group names so
    // server-side ACF can match `group:foo` rules — pvxs ca-auth
    // parity (osgroups.cpp).
    if auth == "ca" {
        let groups = crate::auth::posix_groups();
        // Variant tag (0xFD) + inline AuthZ structure carrying
        // user (str) + host (str) [+ groups (str[])].
        payload.put_u8(0xFD);
        payload.put_u16(1, order);
        payload.put_u8(0x80);
        payload.put_u8(0x00);
        let n_fields = if groups.is_empty() { 2u8 } else { 3u8 };
        payload.put_u8(n_fields);
        payload.put_u8(0x04);
        payload.extend_from_slice(b"user");
        payload.put_u8(0x60); // string
        payload.put_u8(0x04);
        payload.extend_from_slice(b"host");
        payload.put_u8(0x60); // string
        if !groups.is_empty() {
            payload.put_u8(0x06);
            payload.extend_from_slice(b"groups");
            payload.put_u8(0x68); // string[]
        }
        encode_string_into(user, order, &mut payload);
        encode_string_into(host, order, &mut payload);
        if !groups.is_empty() {
            // string-array length prefix (size_t encoding) + each
            // string.
            crate::proto::encode_size_into(groups.len() as u32, order, &mut payload);
            for g in &groups {
                encode_string_into(g, order, &mut payload);
            }
        }
    } else {
        // Null variant — pvxs `readVariant` returns `Value()` for 0xFF.
        payload.put_u8(0xFF);
    }

    let h = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut out = Vec::with_capacity(PvaHeader::SIZE + payload.len());
    h.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

#[allow(unused_imports)]
use crate::proto::decode_size;

#[allow(dead_code)]
fn _suppress(_: HeaderFlags, _: Status) {}

#[cfg(test)]
mod tests {
    use super::*;

    // The auth-selection rule shared by the normal TCP connection path and the
    // TCP name-server path (pvxs has no name-server auth exception): prefer "ca"
    // when the server offers it, else fall back to "anonymous".
    #[test]
    fn select_client_auth_prefers_ca_when_offered() {
        assert_eq!(
            select_client_auth(&["anonymous".to_string(), "ca".to_string()]),
            "ca",
            "must select ca when the server advertises anonymous, ca"
        );
        assert_eq!(
            select_client_auth(&["ca".to_string()]),
            "ca",
            "must select ca when it is the only method"
        );
        assert_eq!(
            select_client_auth(&["anonymous".to_string()]),
            "anonymous",
            "must fall back to anonymous when ca is not offered"
        );
        assert_eq!(
            select_client_auth(&[]),
            "anonymous",
            "must fall back to anonymous when no methods are advertised"
        );
    }

    /// R17-36: the ECHO cadence is pvxs's `max(1, min(15, tcpTimeout*3/8))`
    /// on the EFFECTIVE tcpTimeout (clientconn.cpp:163), not `CONN_TMO/2`.
    /// The two formulas coincide in the interior (4/3 × 3/8 = 1/2), so the
    /// divergence is exactly the missing 15 s CAP: pre-fix a 100 s CONN_TMO
    /// echoed every 50 s (C: 15 s), and an out-of-range CONN_TMO — whose
    /// effective timeout resets to 40 s — echoed every ~3.5e18 s, i.e.
    /// never.
    #[test]
    #[serial_test::serial(epics_env)]
    fn heartbeat_interval_is_capped_at_fifteen_seconds() {
        let prev = std::env::var("EPICS_PVA_CONN_TMO").ok();
        let set = |v: &str| unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", v) };

        // Default 30 s → effective 40 s → pvxs's documented 15 s period.
        set("30");
        assert_eq!(heartbeat_interval(), Duration::from_secs_f64(15.0));

        // Interior: 8 s → effective 10.667 s → 4 s.
        set("8");
        assert_eq!(heartbeat_interval(), Duration::from_secs_f64(4.0));

        // Above the cap: pre-fix this was CONN_TMO/2 = 50 s.
        set("100");
        assert_eq!(
            heartbeat_interval(),
            Duration::from_secs_f64(15.0),
            "echo period must be capped at 15s"
        );

        // Out-of-range CONN_TMO: effective timeout resets to 40 s, so the
        // cadence is 15 s — pre-fix it was ~3.5e18 s.
        set("7e18");
        assert_eq!(
            heartbeat_interval(),
            Duration::from_secs_f64(15.0),
            "an out-of-range CONN_TMO must still echo on C's 40s-derived clock"
        );

        // Floor: 1 s → effective 2 s → 0.75 s raw → floored at 1 s.
        set("1");
        assert_eq!(heartbeat_interval(), Duration::from_secs_f64(1.0));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("EPICS_PVA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_PVA_CONN_TMO"),
            }
        }
    }

    /// pvxs `enforceTimeout` (config.cpp:373-391) runs on the SCALED
    /// tcpTimeout, so its upper reset fires for configured values that
    /// `parse_timeout` accepts (<= time_t::max) but whose 4/3-scaled form
    /// crosses time_t::max: 7e18 × 4/3 ≈ 9.33e18 >= 9.22e18 → 40 s.
    /// Pre-fix this site applied only the 2 s floor and handed a ~9.33e18 s
    /// (≈ 3e11 years) window to the dead-connection detector, i.e. the
    /// client never timed out a dead server (R17-34).
    #[test]
    #[serial_test::serial(epics_env)]
    fn heartbeat_timeout_resets_out_of_range_conn_tmo_to_40s() {
        let prev = std::env::var("EPICS_PVA_CONN_TMO").ok();

        unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", "7e18") };
        assert_eq!(
            heartbeat_timeout(),
            Duration::from_secs_f64(40.0),
            "scaled CONN_TMO >= time_t::max must reset to pvxs's 40s default"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("EPICS_PVA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_PVA_CONN_TMO"),
            }
        }
    }

    /// `heartbeat_timeout` is the effective TCP idle-timeout owner: it
    /// scales the configured `EPICS_PVA_CONN_TMO` by pvxs `tmoScale` (4/3)
    /// and floors at 2s (`enforceTimeout`, config.cpp:373-391). A
    /// fractional CONN_TMO must scale from the full double, not a value
    /// truncated to integer seconds first. Boundaries: below the 2s
    /// effective floor, exactly at it, and above it.
    #[test]
    #[serial_test::serial(epics_env)]
    fn heartbeat_timeout_scales_fractional_conn_tmo_with_floor() {
        let prev = std::env::var("EPICS_PVA_CONN_TMO").ok();
        let set = |v: &str| unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", v) };

        // Below floor: 1.0 × 4/3 = 1.333 < 2 → clamped to 2s.
        set("1.0");
        assert_eq!(heartbeat_timeout(), Duration::from_secs_f64(2.0));

        // At floor: 1.5 × 4/3 = 2.0 exactly.
        set("1.5");
        assert_eq!(heartbeat_timeout(), Duration::from_secs_f64(2.0));

        // Above floor: 2.5 × 4/3 = 3.333…s. Pre-fix truncation to 2s gave
        // 2 × 4/3 = 2.667s, so assert both the exact value and the
        // inequality against the truncated result.
        set("2.5");
        assert_eq!(
            heartbeat_timeout(),
            Duration::from_secs_f64(2.5 * 4.0 / 3.0)
        );
        assert_ne!(
            heartbeat_timeout(),
            Duration::from_secs_f64(2.0 * 4.0 / 3.0),
            "fractional CONN_TMO must not be truncated to integer seconds"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("EPICS_PVA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_PVA_CONN_TMO"),
            }
        }
    }

    fn build_message_payload(order: ByteOrder, ioid: u32, mtype: u8, msg: &str) -> Vec<u8> {
        let mut p = Vec::new();
        p.put_u32(ioid, order);
        p.put_u8(mtype);
        encode_string_into(msg, order, &mut p);
        p
    }

    /// pvxs `clientconn.cpp:303-313` logs "Trying to proceed w/o
    /// cred" and proceeds (`ready = true; createChannels()`) after a
    /// non-success CONNECTION_VALIDATED — the server refused the offered
    /// credentials but may still serve PVs anonymously. Only a malformed
    /// frame disconnects. The pre-fix port hard-failed, so a Rust client
    /// could not reach a refuse-cred-serve-anon server (reconnect loop).
    /// `wait_for_validated` must return `Ok` on a non-success status.
    #[epics_macros_rs::epics_test]
    async fn wait_for_validated_proceeds_on_auth_refused() {
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        Status::error("auth refused").write_into(order, &mut payload);
        let mut frame = Vec::new();
        PvaHeader::application(
            true,
            order,
            Command::ConnectionValidated.code(),
            payload.len() as u32,
        )
        .write_into(&mut frame);
        frame.extend_from_slice(&payload);

        let mut reader = std::io::Cursor::new(frame);
        let mut rx_buf = Vec::new();
        let res = wait_for_validated(&mut reader, &mut rx_buf, Duration::from_secs(1), None).await;
        assert!(
            res.is_ok(),
            "non-success CONNECTION_VALIDATED must proceed anonymously (pvxs parity), got {res:?}"
        );
    }

    #[test]
    fn log_server_message_does_not_panic_on_well_formed_payloads() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            for mtype in [
                MessageType::Info as u8,
                MessageType::Warning as u8,
                MessageType::Error as u8,
                MessageType::Fatal as u8,
                99, // unknown
            ] {
                let payload = build_message_payload(order, 0xCAFEBABE, mtype, "hello world");
                log_server_message(&payload, order).expect("well-formed MESSAGE must decode");
            }
        }
    }

    /// pvxs `handle_MESSAGE` decodes ioid, mtype and the string, and throws on
    /// `!M.good()` (clientconn.cpp:454-455) — the dispatch catch then resets
    /// the bev. Every truncation is a decode fault, not something to swallow.
    #[test]
    fn log_server_message_faults_on_truncated_payload() {
        for payload in [
            &[][..],
            &[0x01][..],
            &[0u8; 4][..],                 // ioid only, no mtype
            &[0u8; 5][..],                 // ioid + mtype but no string
            &[0, 0, 0, 0, 1, 9, b'a'][..], // string claims 9 bytes, 1 follows
            &[0, 0, 0, 0, 1, 0xFF][..],    // null-string marker: faults in pvxs too
        ] {
            assert!(
                log_server_message(payload, ByteOrder::Little).is_err(),
                "a MESSAGE that does not decode must fault the circuit: {payload:?}"
            );
        }
    }

    /// Client half.
    /// pvxs client `handle_MESSAGE` maps the level through the same
    /// `mtype2level` as the server (clientconn.cpp:457, pvaproto.h:704-712):
    /// 0=Info, 1=Warn, 2=Err, default (Fatal=3 and every unknown value)=Crit.
    /// tracing has no Crit, so Err, Fatal, and unknown types all map to its
    /// highest level, error!. An unknown type must escalate to error!, not
    /// be downgraded to warn! as it was before.
    #[test]
    fn r0604_log_server_message_severity_matches_mtype2level() {
        use std::sync::{Arc, Mutex};
        use tracing::Level;
        use tracing_subscriber::layer::{Context, Layer};
        use tracing_subscriber::prelude::*;

        struct LevelCapture(Arc<Mutex<Vec<Level>>>);
        impl<S: tracing::Subscriber> Layer<S> for LevelCapture {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                self.0.lock().unwrap().push(*event.metadata().level());
            }
        }

        let order = ByteOrder::Little;
        let levels = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(LevelCapture(levels.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for mtype in [
                MessageType::Info as u8,
                MessageType::Warning as u8,
                MessageType::Error as u8,
                MessageType::Fatal as u8,
                99u8, // unknown
            ] {
                let payload = build_message_payload(order, 0xCAFEBABE, mtype, "m");
                log_server_message(&payload, order).expect("well-formed MESSAGE must decode");
            }
        });

        let got = levels.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                Level::INFO,
                Level::WARN,
                Level::ERROR,
                Level::ERROR,
                Level::ERROR,
            ],
            "server-MESSAGE mtypes Info/Warn/Err/Fatal/unknown must map to INFO/WARN/ERROR/ERROR/ERROR"
        );
    }

    /// Build a fresh set of router DashMaps + cancel token + writer_tx
    /// for unit tests. The writer receiver is leaked (Drop'd) so any
    /// destroy frames the route emits during the test go to /dev/null
    /// — tests that want to assert on the destroy bytes can clone the
    /// receiver via `let (tx, _rx) = mpsc::unbounded_channel(); ...`
    /// instead of calling `fresh_router`.
    fn fresh_router() -> (
        Arc<DashMap<u32, IoidSlot>>,
        Arc<DashMap<u32, oneshot::Sender<Frame>>>,
        Arc<DashMap<u32, (Arc<AtomicBool>, Arc<tokio::sync::Notify>)>>,
        Arc<DashMap<u32, u32>>,
        Arc<DashMap<u32, u8>>,
        mpsc::UnboundedSender<Vec<u8>>,
        CancellationToken,
    ) {
        let (writer_tx, _) = mpsc::unbounded_channel();
        (
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            writer_tx,
            CancellationToken::new(),
        )
    }

    /// pvxs `chan->statRx += rxlen` (clientmon.cpp:608, clientget.cpp:496):
    /// a reply frame routed by IOID must add its full wire length
    /// (8-byte header + payload) to the owning channel's receive counter,
    /// resolved via the IOID→SID map. A frame whose IOID has no SID mapping
    /// leaves the per-channel counters untouched.
    #[test]
    fn route_frame_attributes_rx_bytes_to_channel_by_ioid() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let chan_stats: Arc<DashMap<u32, ChanStat>> = Arc::new(DashMap::new());
        let sid = 5u32;
        let ioid = 77u32;
        chan_stats.insert(
            sid,
            ChanStat {
                name: "X:PV".into(),
                rx: AtomicU64::new(0),
                tx: AtomicU64::new(0),
            },
        );
        ioid_to_sid.insert(ioid, sid);
        ioid_to_cmd.insert(ioid, Command::Monitor.code());
        let (tx, _rx) = mpsc::unbounded_channel::<Frame>();
        by_ioid.insert(ioid, IoidSlot::Stream(tx));

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.extend_from_slice(&[0u8; 10]); // body
        let wire_len = (PvaHeader::SIZE + payload.len()) as u64;
        let header =
            PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &chan_stats,
            &writer_tx,
            &cancel,
        );
        assert_eq!(
            chan_stats.get(&sid).unwrap().rx.load(Ordering::Relaxed),
            wire_len
        );

        // A frame on an unmapped IOID must not touch any channel counter.
        let unmapped_ioid = 999u32;
        let (tx2, _rx2) = mpsc::unbounded_channel::<Frame>();
        by_ioid.insert(unmapped_ioid, IoidSlot::Stream(tx2));
        ioid_to_cmd.insert(unmapped_ioid, Command::Monitor.code());
        let mut payload2 = Vec::new();
        payload2.put_u32(unmapped_ioid, order);
        let header2 =
            PvaHeader::application(true, order, Command::Monitor.code(), payload2.len() as u32);
        route_frame(
            Frame {
                header: header2,
                payload: payload2,
            },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &chan_stats,
            &writer_tx,
            &cancel,
        );
        assert_eq!(
            chan_stats.get(&sid).unwrap().rx.load(Ordering::Relaxed),
            wire_len,
            "unmapped IOID must not change the channel counter"
        );
    }

    #[test]
    fn destroy_channel_fires_registered_close_signal() {
        use std::sync::atomic::Ordering as AtoOrd;
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let flag = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let sid = 0xDEADBEEFu32;
        by_sid_close.insert(sid, (flag.clone(), notify.clone()));

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        let header = PvaHeader::application(
            true,
            order,
            Command::DestroyChannel.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };

        route_frame(
            frame,
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &Arc::new(DashMap::new()),
            &writer_tx,
            &cancel,
        );
        assert!(flag.load(AtoOrd::Relaxed));
        assert!(!by_sid_close.contains_key(&sid));
    }

    /// A
    /// server-initiated `CMD_DESTROY_CHANNEL` must also drop the destroyed
    /// SID's `ClientReport` (`chan_stats`) entry, matching pvxs
    /// `Channel::disconnect()` doing `chanBySID.erase(sid)` (client.cpp:170).
    /// Before the fix the branch removed the IOID maps and close signal but
    /// left `chan_stats` populated, so a report taken before the next state
    /// transition still listed the destroyed channel, and a same-connection
    /// SID reuse kept the stale name/counters (`register_channel` is
    /// `or_insert_with`).
    #[test]
    fn destroy_channel_removes_chan_stats_report_entry() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let chan_stats: Arc<DashMap<u32, ChanStat>> = Arc::new(DashMap::new());
        let sid = 9u32;
        // A live channel with accumulated report counters.
        chan_stats.insert(
            sid,
            ChanStat {
                name: "OLD:PV".into(),
                rx: AtomicU64::new(123),
                tx: AtomicU64::new(456),
            },
        );
        let flag = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        by_sid_close.insert(sid, (flag.clone(), notify.clone()));

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        let header = PvaHeader::application(
            true,
            order,
            Command::DestroyChannel.code(),
            payload.len() as u32,
        );
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &chan_stats,
            &writer_tx,
            &cancel,
        );

        // pvxs erases chanBySID[sid]: the report no longer lists the channel.
        assert!(
            !chan_stats.contains_key(&sid),
            "server DESTROY_CHANNEL must drop the channel's report entry"
        );

        // Same-connection SID reuse: `register_channel`'s `or_insert_with`
        // now creates a fresh entry (new name, zeroed counters) instead of
        // keeping the destroyed channel's stale name/counters.
        chan_stats.entry(sid).or_insert_with(|| ChanStat {
            name: "NEW:PV".into(),
            rx: AtomicU64::new(0),
            tx: AtomicU64::new(0),
        });
        let reused = chan_stats.get(&sid).unwrap();
        assert_eq!(reused.name, "NEW:PV", "reused SID must take the new name");
        assert_eq!(reused.rx.load(Ordering::Relaxed), 0, "reused SID rx zeroed");
        assert_eq!(reused.tx.load(Ordering::Relaxed), 0, "reused SID tx zeroed");
    }

    /// `flag.store(true)` for the destroyed sid must run together with
    /// the `by_sid_close` removal so a concurrent re-register can't
    /// observe a torn state. With DashMap we get per-shard atomicity
    /// for the remove + the subsequent flag.store; both are observed
    /// before route_frame returns.
    #[test]
    fn destroy_critical_section_completes_before_route_frame_returns() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let flag = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let sid = 7u32;
        by_sid_close.insert(sid, (flag.clone(), notify.clone()));
        let mut payload = Vec::new();
        payload.put_u32(sid, ByteOrder::Little);
        let header = PvaHeader::application(
            true,
            ByteOrder::Little,
            Command::DestroyChannel.code(),
            payload.len() as u32,
        );
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &Arc::new(DashMap::new()),
            &writer_tx,
            &cancel,
        );
        assert!(!by_sid_close.contains_key(&sid));
        assert!(flag.load(Ordering::Relaxed));
    }

    /// route_frame on `CMD_DESTROY_CHANNEL` must also drop every
    /// in-flight op's frame sender whose ioid maps to the destroyed
    /// sid. Without this, blocked oneshot/stream awaits hang forever.
    #[test]
    fn destroy_channel_drops_associated_ioid_streams() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let sid = 42u32;
        let other_sid = 99u32;

        // Register two streams on the destroyed sid + one on another sid.
        let (tx_a, mut rx_a) = mpsc::unbounded_channel::<Frame>();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel::<Frame>();
        let (tx_c, mut rx_c) = mpsc::unbounded_channel::<Frame>();
        by_ioid.insert(1001, IoidSlot::Stream(tx_a));
        ioid_to_sid.insert(1001, sid);
        by_ioid.insert(1002, IoidSlot::Stream(tx_b));
        ioid_to_sid.insert(1002, sid);
        by_ioid.insert(1003, IoidSlot::Stream(tx_c));
        ioid_to_sid.insert(1003, other_sid);
        by_sid_close.insert(
            sid,
            (
                Arc::new(AtomicBool::new(false)),
                Arc::new(tokio::sync::Notify::new()),
            ),
        );

        let mut payload = Vec::new();
        payload.put_u32(sid, ByteOrder::Little);
        let header = PvaHeader::application(
            true,
            ByteOrder::Little,
            Command::DestroyChannel.code(),
            payload.len() as u32,
        );
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &Arc::new(DashMap::new()),
            &writer_tx,
            &cancel,
        );

        assert!(
            rx_a.try_recv().is_err(),
            "ioid 1001 stream should be closed"
        );
        assert!(
            rx_b.try_recv().is_err(),
            "ioid 1002 stream should be closed"
        );
        assert!(matches!(
            rx_c.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(!by_ioid.contains_key(&1001));
        assert!(!by_ioid.contains_key(&1002));
        assert!(by_ioid.contains_key(&1003));
        assert!(!ioid_to_sid.contains_key(&1001));
        assert!(ioid_to_sid.contains_key(&1003));
    }

    /// Regression: `CMD_DESTROY_CHANNEL` cleanup must remove ALL
    /// three IOID maps — `by_ioid`, `ioid_to_sid`, AND `ioid_to_cmd` —
    /// for every IOID belonging to the destroyed sid, the same owner
    /// boundary as `unregister_ioid`.
    ///
    /// Before the fix the destroy branch removed only `by_ioid` and
    /// `ioid_to_sid`, leaking the `ioid_to_cmd` command expectation for
    /// the connection's lifetime. A late frame on a destroyed IOID
    /// would then hit the command-mismatch gate (which consults
    /// `ioid_to_cmd` before the `by_ioid` lookup) and cancel the whole
    /// TCP connection if its command differed from the stale entry.
    #[test]
    fn destroy_channel_drops_ioid_to_cmd_entries() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let sid = 42u32;
        let other_sid = 99u32;

        // Register ops on the destroyed sid + one on another sid, each
        // with a command expectation in `ioid_to_cmd` exactly as
        // `register_ioid_twoshot` / `register_ioid_reusable` do.
        let (tx_a, _rx_a) = mpsc::unbounded_channel::<Frame>();
        let (tx_b, _rx_b) = mpsc::unbounded_channel::<Frame>();
        let (tx_c, _rx_c) = mpsc::unbounded_channel::<Frame>();
        by_ioid.insert(2001, IoidSlot::Stream(tx_a));
        ioid_to_sid.insert(2001, sid);
        ioid_to_cmd.insert(2001, Command::Get.code());
        by_ioid.insert(2002, IoidSlot::Stream(tx_b));
        ioid_to_sid.insert(2002, sid);
        ioid_to_cmd.insert(2002, Command::Monitor.code());
        by_ioid.insert(2003, IoidSlot::Stream(tx_c));
        ioid_to_sid.insert(2003, other_sid);
        ioid_to_cmd.insert(2003, Command::Get.code());
        by_sid_close.insert(
            sid,
            (
                Arc::new(AtomicBool::new(false)),
                Arc::new(tokio::sync::Notify::new()),
            ),
        );

        let mut payload = Vec::new();
        payload.put_u32(sid, ByteOrder::Little);
        let header = PvaHeader::application(
            true,
            ByteOrder::Little,
            Command::DestroyChannel.code(),
            payload.len() as u32,
        );
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &Arc::new(DashMap::new()),
            &writer_tx,
            &cancel,
        );

        // All three maps cleared for the destroyed sid's IOIDs.
        for ioid in [2001u32, 2002u32] {
            assert!(!by_ioid.contains_key(&ioid), "by_ioid leaked {ioid}");
            assert!(
                !ioid_to_sid.contains_key(&ioid),
                "ioid_to_sid leaked {ioid}"
            );
            assert!(
                !ioid_to_cmd.contains_key(&ioid),
                "ioid_to_cmd leaked {ioid} — stale command expectation"
            );
        }
        // The other sid's IOID is untouched in all three maps.
        assert!(by_ioid.contains_key(&2003));
        assert!(ioid_to_sid.contains_key(&2003));
        assert!(ioid_to_cmd.contains_key(&2003));
    }

    /// Regression: the router must peek routing keys with THIS frame's
    /// header byte order, not a fixed startup-handshake order. pvxs sets
    /// `peerBE` per received application frame (conn.cpp:195-198) and reads
    /// every routing key (CID/IOID) from `EvInBuf M(peerBE, ...)`. The
    /// IOIDs below are chosen so their big-endian and little-endian
    /// encodings differ — peeking with the wrong order would miss the slot
    /// (or trip the command-mismatch close path).
    #[test]
    fn route_frame_peeks_ioid_with_per_frame_byte_order() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let ioid = 0x01020304u32; // BE bytes != LE bytes
        let (tx, mut rx) = oneshot::channel::<Frame>();
        let mut q = std::collections::VecDeque::new();
        q.push_back(tx);
        by_ioid.insert(ioid, IoidSlot::TwoShot(q));
        ioid_to_cmd.insert(ioid, Command::Get.code());

        // Big-endian GET response (header MSB set, ioid encoded big-endian).
        let order = ByteOrder::Big;
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(0x00); // subcmd — not used by routing
        let header = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &Arc::new(DashMap::new()),
            &writer_tx,
            &cancel,
        );

        assert!(
            rx.try_recv().is_ok(),
            "a big-endian response must route to the IOID peeked with the frame's own order"
        );
        assert!(
            !cancel.is_cancelled(),
            "correct per-frame routing must not trip the command-mismatch close path"
        );
    }

    /// pvxs tears the circuit down for every server frame it cannot decode:
    /// `handle_MESSAGE` throws on `!M.good()` (clientconn.cpp:454-455) and the
    /// dispatch catch calls `bev.reset()` (conn.cpp:277-281); CREATE_CHANNEL
    /// (:334-338), DESTROY_CHANNEL (:417-421) and the op handlers
    /// (clientget.cpp:490-494) call `bev.reset()` directly. The port used to
    /// swallow all of them and keep serving a peer that had already corrupted
    /// the stream. An *unknown* command stays ignorable — pvxs drains its body
    /// for forward compatibility (conn.cpp:250-252).
    #[test]
    fn malformed_server_frames_are_circuit_fatal() {
        let order = ByteOrder::Little;
        let route = |cmd: u8, payload: Vec<u8>| {
            let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
                fresh_router();
            let header = PvaHeader::application(true, order, cmd, payload.len() as u32);
            route_frame(
                Frame { header, payload },
                &by_ioid,
                &by_cid,
                &by_sid_close,
                &ioid_to_sid,
                &ioid_to_cmd,
                &Arc::new(DashMap::new()),
                &writer_tx,
                &cancel,
            );
            cancel.is_cancelled()
        };

        // CMD_MESSAGE: string length runs past the end of the payload.
        let mut truncated_msg = Vec::new();
        truncated_msg.put_u32(7, order);
        truncated_msg.put_u8(MessageType::Warning as u8);
        truncated_msg.put_u8(9); // claims a 9-byte string...
        truncated_msg.extend_from_slice(b"abc"); // ...but only 3 bytes follow
        assert!(
            route(Command::Message.code(), truncated_msg),
            "a MESSAGE whose string is truncated must close the circuit"
        );

        // CMD_MESSAGE: payload cannot even carry the IOID.
        assert!(
            route(Command::Message.code(), vec![0x01, 0x02]),
            "a MESSAGE too short for its IOID must close the circuit"
        );

        // Control: a well-formed MESSAGE is logged and the circuit survives.
        assert!(
            !route(
                Command::Message.code(),
                build_message_payload(order, 7, MessageType::Info as u8, "hello")
            ),
            "a well-formed MESSAGE must be logged, not fatal"
        );

        // Sibling handlers: the routing key must decode or the circuit closes.
        assert!(
            route(Command::DestroyChannel.code(), vec![0x00, 0x01]),
            "a DESTROY_CHANNEL too short for its SID must close the circuit"
        );
        assert!(
            route(Command::CreateChannel.code(), vec![0x00]),
            "a CREATE_CHANNEL too short for its CID must close the circuit"
        );
        for cmd in [
            Command::Get.code(),
            Command::Put.code(),
            Command::PutGet.code(),
            Command::Monitor.code(),
            Command::Rpc.code(),
            Command::GetField.code(),
        ] {
            assert!(
                route(cmd, vec![0x00, 0x00, 0x00]),
                "an op reply (cmd {cmd}) too short for its IOID must close the circuit"
            );
        }

        // Forward compatibility: an unknown command with a short payload is
        // drained and ignored, exactly as pvxs's dispatch default does.
        assert!(
            !route(Command::MultipleData.code(), vec![0x00]),
            "an unhandled command must stay ignorable, not close the circuit"
        );
    }

    /// Same per-frame-order requirement for CREATE_CHANNEL CID routing —
    /// a CID peeked with the wrong order would strand a successful channel.
    #[test]
    fn route_frame_peeks_create_channel_cid_with_per_frame_byte_order() {
        let (by_ioid, by_cid, by_sid_close, ioid_to_sid, ioid_to_cmd, writer_tx, cancel) =
            fresh_router();
        let cid = 0x0A0B0C0Du32;
        let (tx, mut rx) = oneshot::channel::<Frame>();
        by_cid.insert(cid, tx);

        let order = ByteOrder::Big;
        let mut payload = Vec::new();
        payload.put_u32(cid, order);
        payload.put_u32(7, order); // sid
        Status::ok().write_into(order, &mut payload);
        let header = PvaHeader::application(
            true,
            order,
            Command::CreateChannel.code(),
            payload.len() as u32,
        );
        route_frame(
            Frame { header, payload },
            &by_ioid,
            &by_cid,
            &by_sid_close,
            &ioid_to_sid,
            &ioid_to_cmd,
            &Arc::new(DashMap::new()),
            &writer_tx,
            &cancel,
        );

        assert!(
            rx.try_recv().is_ok(),
            "a big-endian CREATE_CHANNEL response must route to the CID peeked with the frame's own order"
        );
    }

    /// Regression (client side): a server may re-negotiate the connection
    /// byte order mid-stream with another SET_BYTE_ORDER control frame.
    /// pvxs latches `sendBE = header[2] & pva_flags::MSB` on every received
    /// SetEndian (conn.cpp:169-188) and uses it for all subsequent sends.
    /// `handle_control_frame` is the single owner of that latch on the
    /// client side: it updates the shared outbound-order cell on
    /// SET_BYTE_ORDER, and every later outbound frame (here, the echo
    /// response it emits) must adopt the new order. Drives two
    /// SET_BYTE_ORDER frames (LE → BE → LE) and asserts both the latched
    /// cell and the echo-response frame order follow each.
    #[test]
    fn handle_control_frame_latches_mid_stream_set_byte_order() {
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // Seeded little-endian (the handshake default).
        let out_order = Arc::new(AtomicBool::new(false));

        // Build an 8-byte control frame carrying `order` in its header.
        let control = |cmd: u8, order: ByteOrder| Frame {
            header: PvaHeader::control(true, order, cmd, 0),
            payload: Vec::new(),
        };
        // Decode the byte order of the next frame the writer emitted.
        let sent_order = |rx: &mut mpsc::UnboundedReceiver<Vec<u8>>| -> ByteOrder {
            let bytes = rx.try_recv().expect("a response frame must be queued");
            let hdr = PvaHeader::decode(&mut std::io::Cursor::new(&bytes[..]))
                .expect("decode response header");
            hdr.flags.byte_order()
        };

        // 1st SET_BYTE_ORDER: Big. Latch flips LE → BE.
        handle_control_frame(
            &control(ControlCommand::SetByteOrder.code(), ByteOrder::Big),
            &writer_tx,
            &out_order,
        );
        assert!(
            out_order.load(Ordering::Relaxed),
            "SET_BYTE_ORDER(Big) must latch the outbound order to Big"
        );

        // EchoRequest → response must use the latched Big order. The
        // request's own header order is intentionally the OPPOSITE (Little)
        // to prove the response order comes from the latch, not the request.
        handle_control_frame(
            &control(ControlCommand::EchoRequest.code(), ByteOrder::Little),
            &writer_tx,
            &out_order,
        );
        assert_eq!(
            sent_order(&mut writer_rx),
            ByteOrder::Big,
            "echo response must adopt the latched Big order, not the request frame's order"
        );

        // 2nd SET_BYTE_ORDER: Little. Latch flips BE → LE.
        handle_control_frame(
            &control(ControlCommand::SetByteOrder.code(), ByteOrder::Little),
            &writer_tx,
            &out_order,
        );
        assert!(
            !out_order.load(Ordering::Relaxed),
            "a 2nd SET_BYTE_ORDER(Little) must re-latch the outbound order to Little"
        );

        // EchoRequest with an opposite (Big) request header → response Little.
        handle_control_frame(
            &control(ControlCommand::EchoRequest.code(), ByteOrder::Big),
            &writer_tx,
            &out_order,
        );
        assert_eq!(
            sent_order(&mut writer_rx),
            ByteOrder::Little,
            "echo response must follow the re-latched Little order after the 2nd SET_BYTE_ORDER"
        );
    }

    /// Regression: `tcp_timeout` passed to `ServerConn::connect` must
    /// govern the heartbeat idle timeout, not the process environment.
    ///
    /// Setup: a mock server completes the PVA handshake then goes silent.
    /// With `tcp_timeout = 500ms` the heartbeat declares the connection dead
    /// well within 4 seconds. Before the fix the heartbeat read from
    /// `EPICS_PVA_CONN_TMO` (default → 40 s) so the connection would still
    /// be alive at the 4 s deadline.
    ///
    /// pvxs upstream:
    ///   - inactivity timeout = `tcpTimeout`     (clientconn.cpp:73-74)
    ///   - echo interval = max(1, min(15, tcpTimeout × 3/8)) (clientconn.cpp:163-165)
    #[tokio::test]
    async fn pva_r2_tcp_timeout_applied() {
        use crate::proto::encode_size_into;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Build the mock server's three handshake frames.
        fn server_handshake_frames() -> Vec<u8> {
            let order = ByteOrder::Little;
            let mut buf = Vec::new();

            // Frame 1: SET_BYTE_ORDER (control, server→client).
            PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0)
                .write_into(&mut buf);

            // Frame 2: CONNECTION_VALIDATION request (server→client).
            let mut payload = Vec::new();
            payload.put_u32(0x10000, order); // buffer_size (match pvxs 0x10000)
            payload.put_u16(32_767, order); // registry_size
            encode_size_into(1, order, &mut payload); // 1 auth method
            encode_string_into("anonymous", order, &mut payload);
            PvaHeader::application(
                true,
                order,
                Command::ConnectionValidation.code(),
                payload.len() as u32,
            )
            .write_into(&mut buf);
            buf.extend_from_slice(&payload);

            buf
        }

        fn server_validated_frame() -> Vec<u8> {
            let order = ByteOrder::Little;
            let mut buf = Vec::new();
            // CONNECTION_VALIDATED with Status::ok() (single byte 0xFF).
            let payload = vec![0xFFu8];
            PvaHeader::application(
                true,
                order,
                Command::ConnectionValidated.code(),
                payload.len() as u32,
            )
            .write_into(&mut buf);
            buf.extend_from_slice(&payload);
            buf
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");

        // Mock server task: complete handshake then hold the socket open
        // without writing more bytes.
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(&server_handshake_frames()).await;
                // Drain the client's CONNECTION_VALIDATION reply (a single
                // write from the client; one read is sufficient).
                let mut drain = [0u8; 512];
                let _ =
                    tokio::time::timeout(Duration::from_millis(200), sock.read(&mut drain)).await;
                let _ = sock.write_all(&server_validated_frame()).await;
                // Drop the write half but keep the read half alive so TCP
                // doesn't send FIN — the client's reader stays pending and
                // the only exit is the heartbeat idle timeout.
                let (reader_half, _writer_half) = sock.into_split();
                // Hold reader_half so the OS doesn't RST the connection.
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(reader_half);
            }
        });

        // Short tcp_timeout so the heartbeat fires quickly:
        //   hb_interval = max(1, min(15, 0.5 * 3/8)) = 1 s
        //   hb_timeout  = 0.5 s
        // Connection must be declared dead at the first heartbeat tick (~1 s).
        let tcp_timeout = Duration::from_millis(500);
        let op_timeout = Duration::from_secs(2);

        let conn = ServerConn::connect(
            addr,
            "testuser",
            "testhost",
            ConnConfig {
                op_timeout,
                tcp_timeout,
                max_message_size: None,
            },
        )
        .await
        .expect("handshake must succeed");

        assert!(conn.is_alive(), "connection must be alive after handshake");

        // Wait for the heartbeat to declare the connection dead.
        // Deadline = 4 s; before the fix hb_timeout = 40 s (env default)
        // so this assertion would still be true at 4 s and the timeout
        // would fire, causing the test to fail.
        let deadline = Duration::from_secs(4);
        tokio::time::timeout(deadline, async {
            loop {
                if !conn.is_alive() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("connection must be declared dead within 4 s (tcp_timeout=500ms)");

        assert!(!conn.is_alive());
    }

    /// The client heartbeat must use an
    /// application `CMD_ECHO`, which pvxs servers answer, NOT a control
    /// EchoRequest, which pvxs drains and ignores (conn.cpp:180-194). A
    /// pvxs-shaped peer that replies only to application echo must keep a
    /// Rust client alive past several heartbeat intervals; before the fix
    /// the control probe drew no reply, `last_rx` went stale, and the
    /// connection was torn down despite a healthy server.
    #[tokio::test]
    async fn pva_rs_71_app_echo_keeps_pvxs_link_alive() {
        use crate::proto::encode_size_into;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let order = ByteOrder::Little;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");

        // pvxs-shaped server: complete the handshake, then drain control
        // frames and reply ONLY to application CMD_ECHO with a
        // server-direction CMD_ECHO (serverconn.cpp:166-178).
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };

            // Handshake frame 1+2: SET_BYTE_ORDER + CONNECTION_VALIDATION.
            let mut hs = Vec::new();
            PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0)
                .write_into(&mut hs);
            let mut vpayload = Vec::new();
            vpayload.put_u32(0x10000, order);
            vpayload.put_u16(32_767, order);
            encode_size_into(1, order, &mut vpayload);
            encode_string_into("anonymous", order, &mut vpayload);
            PvaHeader::application(
                true,
                order,
                Command::ConnectionValidation.code(),
                vpayload.len() as u32,
            )
            .write_into(&mut hs);
            hs.extend_from_slice(&vpayload);
            if sock.write_all(&hs).await.is_err() {
                return;
            }

            // Drain the client's CONNECTION_VALIDATION reply.
            let mut drain = [0u8; 512];
            let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut drain)).await;

            // CONNECTION_VALIDATED (Status::ok()).
            let mut vd = Vec::new();
            PvaHeader::application(true, order, Command::ConnectionValidated.code(), 1)
                .write_into(&mut vd);
            vd.push(0xFFu8);
            if sock.write_all(&vd).await.is_err() {
                return;
            }

            // Echo pump.
            let mut hdr = [0u8; 8];
            while let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_secs(6), sock.read_exact(&mut hdr)).await
            {
                let payload_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
                if payload_len > 0 {
                    let mut body = vec![0u8; payload_len];
                    if sock.read_exact(&mut body).await.is_err() {
                        break;
                    }
                }
                let is_control = hdr[2] & HeaderFlags::CONTROL != 0;
                // A control EchoRequest (the pre-fix probe) is drained with
                // no reply, exactly like pvxs.
                if !is_control && hdr[3] == Command::Echo.code() {
                    let mut reply = Vec::with_capacity(8);
                    PvaHeader::application(true, order, Command::Echo.code(), 0)
                        .write_into(&mut reply);
                    if sock.write_all(&reply).await.is_err() {
                        break;
                    }
                }
            }
        });

        // tcp_timeout = 2 s → hb_interval = max(1, 2×3/8) = 1 s,
        // hb_timeout = 2 s. With application echo answered each interval,
        // `last_rx` is refreshed ~every 1 s and the link survives. Without
        // the fix (control probe, no reply) `last_rx` would go stale and
        // the heartbeat declares the connection dead at ~3 s.
        let conn = ServerConn::connect(
            addr,
            "testuser",
            "testhost",
            ConnConfig {
                op_timeout: Duration::from_secs(2),
                tcp_timeout: Duration::from_secs(2),
                max_message_size: None,
            },
        )
        .await
        .expect("handshake must succeed");

        assert!(conn.is_alive(), "connection must be alive after handshake");

        // Past three heartbeat intervals (3 s): the pre-fix control probe
        // would have torn the link down by now.
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert!(
            conn.is_alive(),
            "application-echo heartbeat must keep the pvxs-shaped link alive"
        );
    }

    /// [`ServerConn::connect_blocking`] completes the same handshake as
    /// [`ServerConn::connect`], on an ordinary host build.
    ///
    /// This is deliberately **not** covered by the `--cfg pva_blocking_client`
    /// suite run, which proves a different thing: that run rebuilds the crate
    /// with `dial_pva` selecting the blocking transport, so what it exercises is
    /// `connect`. Nothing in it ever calls this constructor, and a `--cfg` no
    /// manifest can set is not something a default build can be argued from. So
    /// the third constructor is asserted here, where the crate is compiled
    /// exactly as it ships, and the assertion is that the two transports are
    /// interchangeable at runtime rather than one replacing the other at build
    /// time.
    ///
    /// `#[tokio::test]` — the default `CurrentThread` flavor, on purpose. The
    /// two pump threads must be able to park while the runtime's only thread
    /// drives the handshake future, which is the property
    /// `runtime::task::spawn_dedicated_thread` establishes by declining to
    /// inherit a current-thread ambient handle. Run this on a `multi_thread`
    /// flavor and it passes either way, testing nothing.
    #[tokio::test]
    async fn connect_blocking_completes_the_same_handshake_as_connect() {
        use crate::proto::encode_size_into;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let order = ByteOrder::Little;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");

        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            // SET_BYTE_ORDER + CONNECTION_VALIDATION.
            let mut hs = Vec::new();
            PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0)
                .write_into(&mut hs);
            let mut payload = Vec::new();
            payload.put_u32(0x10000, order);
            payload.put_u16(32_767, order);
            encode_size_into(1, order, &mut payload);
            encode_string_into("anonymous", order, &mut payload);
            PvaHeader::application(
                true,
                order,
                Command::ConnectionValidation.code(),
                payload.len() as u32,
            )
            .write_into(&mut hs);
            hs.extend_from_slice(&payload);
            let _ = sock.write_all(&hs).await;

            // Drain the client's CONNECTION_VALIDATION reply, then validate.
            let mut drain = [0u8; 512];
            let _ = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut drain)).await;
            let mut ok = Vec::new();
            PvaHeader::application(true, order, Command::ConnectionValidated.code(), 1)
                .write_into(&mut ok);
            ok.push(0xFF);
            let _ = sock.write_all(&ok).await;

            // Hold the circuit open past the assertions below, silent. With a
            // finite pump read timeout this is what tore the connection down.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let conn = ServerConn::connect_blocking(
            addr,
            "testuser",
            "testhost",
            ConnConfig {
                op_timeout: Duration::from_secs(2),
                tcp_timeout: Duration::from_secs(30),
                max_message_size: None,
            },
        )
        .await
        .expect("blocking handshake must succeed");

        assert!(
            conn.is_alive(),
            "connection must be alive after the blocking handshake"
        );

        // An idle circuit stays up. The reader pump's `SO_RCVTIMEO` ends the
        // connection when it expires, so this is what fails if it is ever given
        // a per-operation deadline (`op_timeout` above is 2 s) instead of
        // `PumpConfig`'s effectively-infinite default. `tcp_timeout` is 30 s, so
        // the heartbeat has no opinion yet either.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            conn.is_alive(),
            "an idle blocking circuit must outlive op_timeout: the reader pump's \
             receive timeout is not an idle-disconnect bound"
        );
    }

    /// The dial seam must never occupy a callback-band worker.
    ///
    /// Measured (gdb all-thread dump of the host-linux `realtime-pva-ioc`): the
    /// single `cbMedium` worker sat in `poll(timeout=39999)` under
    /// `TcpStream::connect_timeout` ← `dial_blocking` ← `ns_task` — a
    /// synchronous dial to a SYN-blackholed name server held the band for
    /// ~40 s per attempt, starving everything scheduled on Medium (the same
    /// executor failure class as `doc/qsrv-rtems-design.md` §9.15.1: one
    /// occupant on a band = broad delivery starvation).
    ///
    /// Exec-backend-only: the callback band being pinned is the exec
    /// executor's. The `--cfg pva_blocking_client` tokio build shares the
    /// same `dial_blocking` code path, so this coverage carries to it.
    /// Linux-only for the same reason as `epics-ca-rs`'s
    /// `connect_deadline_tests`: the blackhole below relies on Linux
    /// answering an accept-queue-overflowing SYN with silence
    /// (`tcp_abort_on_overflow = 0`, the default); macOS/BSD answer it with
    /// an RST and the dial resolves immediately.
    #[cfg(all(exec_backend, target_os = "linux"))]
    mod dial_band_tests {
        use super::*;
        use epics_base_rs::runtime::blocking_io::MAX_DIAL_WORKERS;
        use std::future::Future;

        /// A local address whose SYNs the kernel drops: a listening socket
        /// with a full accept queue (the `epics-ca-rs`
        /// `connect_deadline_tests::syn_blackhole` mechanism). Linux
        /// (`tcp_abort_on_overflow = 0`, the default) answers an overflowing
        /// SYN with silence, so the connecting peer sits in SYN-SENT and
        /// retries — a deterministic unanswered dial with no route off the
        /// box and no firewall.
        ///
        /// Saturation is *verified*, not assumed: a handshake completing
        /// through the SYN queue lands in the accept queue a beat after the
        /// filler's `connect()` returns, so a dial racing right behind a
        /// single unverified filler can still be admitted (observed: the
        /// late-dial test's connection beat the filler into the queue). The
        /// probe loop below keeps adding established fillers until a fresh
        /// nonblocking connect is still unanswered 300 ms in — on loopback a
        /// queued handshake completes in microseconds and a dropped SYN's
        /// first retransmit is at ~1 s, so an unanswered probe proves the
        /// queue is full. Closing the probe in SYN-SENT aborts the attempt
        /// (no further retransmits), so it cannot steal a slot later.
        ///
        /// Returns the blackhole address, the listener, and the fillers
        /// occupying the queue; the caller keeps them alive (and never
        /// accepts) for as long as the blackhole must hold.
        fn syn_blackhole() -> (SocketAddr, socket2::Socket, Vec<std::net::TcpStream>) {
            use socket2::{Domain, Socket, Type};

            let sock = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
            sock.bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
                .expect("bind");
            // Backlog 0 → the kernel rounds to a 1-slot accept queue.
            sock.listen(0).expect("listen");
            let addr: SocketAddr = sock.local_addr().expect("local_addr").as_socket().unwrap();

            let mut fillers = Vec::new();
            loop {
                let probe = Socket::new(Domain::IPV4, Type::STREAM, None).expect("probe");
                probe.set_nonblocking(true).expect("probe nonblocking");
                match probe.connect(&addr.into()) {
                    Ok(()) => {}
                    Err(e)
                        if e.raw_os_error() == Some(libc::EINPROGRESS)
                            || e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("probe connect: {e}"),
                }
                std::thread::sleep(Duration::from_millis(300));
                if probe.peer_addr().is_ok() {
                    // Admitted: the queue had room. Keep it as a filler.
                    probe.set_nonblocking(false).expect("filler blocking");
                    fillers.push(probe.into());
                } else {
                    // Still SYN-SENT after 300 ms: the queue is full.
                    return (addr, sock, fillers);
                }
            }
        }

        /// The invariant, pinned where it was measured broken: while a dial
        /// toward an unanswering server is in flight, other work spawned on
        /// the same callback band still runs. Pre-fix, `dial_blocking`'s
        /// synchronous `connect_timeout` parked the band's single worker for
        /// the whole bound and the canary starved.
        #[test]
        fn stalled_dial_releases_the_callback_band() {
            let (addr, _listener, _fillers) = syn_blackhole();

            // The dial, spawned exactly as `ns_task` is: `runtime::task::spawn`
            // → the default (Medium) callback band and its single worker.
            let (dial_tx, _dial_rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let _ = dial_tx.send(
                    dial_pva(addr, Duration::from_secs(10), Duration::from_secs(10))
                        .await
                        .map(|_| ()),
                );
            });
            // Let the band worker take the dial task and enter the connect.
            std::thread::sleep(Duration::from_millis(300));

            // Canary on the same band, behind the stalled dial.
            let (tx, rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let _ = tx.send(());
            });
            assert!(
                rx.recv_timeout(Duration::from_secs(2)).is_ok(),
                "a stalled dial must not occupy the callback band: the canary \
                 spawned behind it did not run within 2 s, so the band's \
                 single worker is parked inside the dial"
            );
        }

        /// Boundary: a dial that completes only after SYN retransmission is
        /// still usable. The accept queue is drained ~2 s in, the client's
        /// next SYN retry (Linux RTO ladder: ~1 s, ~3 s, …) completes the
        /// handshake, and the returned write half carries bytes.
        #[test]
        fn dial_completing_after_syn_retry_is_usable() {
            let (addr, listener, fillers) = syn_blackhole();
            let started = std::time::Instant::now();

            let (bytes_tx, bytes_rx) = std::sync::mpsc::channel();
            let acceptor = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                // Drain the fillers (queued first, FIFO): frees the accept
                // queue so the dial's retried SYN completes. The next accept
                // after them is the late dial.
                for _ in 0..fillers.len() {
                    let _ = listener.accept().expect("drain a filler");
                }
                drop(fillers);
                let (ours, _) = listener.accept().expect("accept the late dial");
                let ours: std::net::TcpStream = ours.into();
                ours.set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                use std::io::Read;
                let mut buf = [0u8; 9];
                (&ours).read_exact(&mut buf).expect("read the dialed bytes");
                let _ = bytes_tx.send(buf);
            });

            // Write through the returned half, then hand BOTH halves to the
            // test: dropping the reader adapter shuts the socket down, so
            // the halves must outlive the acceptor's read.
            let (dial_tx, dial_rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let res =
                    match dial_pva(addr, Duration::from_secs(15), Duration::from_secs(15)).await {
                        Ok((reader, mut writer)) => writer
                            .write_all(b"late-dial")
                            .await
                            .map(|()| (reader, writer))
                            .map_err(PvaError::Io),
                        Err(e) => Err(e),
                    };
                let _ = dial_tx.send(res);
            });

            let _halves = dial_rx
                .recv_timeout(Duration::from_secs(12))
                .expect("dial must resolve before its 15 s bound")
                .expect("a dial completing after SYN retry must succeed");
            assert!(
                started.elapsed() >= Duration::from_millis(1900),
                "the dial resolved in {:?}, i.e. before the accept queue was \
                 drained — the blackhole did not hold and the test proved \
                 nothing about a late-completing dial",
                started.elapsed()
            );
            let got = bytes_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the late-dialed connection must carry bytes");
            assert_eq!(&got, b"late-dial");
            acceptor.join().expect("acceptor thread");
        }

        /// Boundary: a target that answers with RST (nothing listening)
        /// fails the dial promptly — the off-band hop must not turn a fast
        /// refusal into a slow one.
        #[test]
        fn dial_to_closed_target_fails_fast() {
            // Bind, take the addr, drop: nothing listens → RST.
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = l.local_addr().expect("addr");
            drop(l);

            let started = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let _ = tx.send(
                    dial_pva(addr, Duration::from_secs(10), Duration::from_secs(10))
                        .await
                        .map(|_| ()),
                );
            });
            let res = rx
                .recv_timeout(Duration::from_secs(3))
                .expect("an RST-refused dial must resolve promptly");
            assert!(
                res.is_err(),
                "nothing listens at {addr}: the dial must fail"
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the refusal took {:?}; the off-band hop must not delay it",
                started.elapsed()
            );
        }

        /// Every thread this client creates for a dial, whatever shape the
        /// dial seam has.
        ///
        /// Both prefixes on purpose: `"PVAC-connect <target>"` is the
        /// per-attempt thread the pool replaced and `"PVAC-dial <n>"` is a
        /// pool worker, so a count taken through this function is comparable
        /// across the change and the bound below is asserted against the old
        /// shape as well as the new one. (`comm` is truncated to 15 bytes, so
        /// both are matched on their stable prefix.)
        fn dial_threads() -> usize {
            std::fs::read_dir("/proc/self/task")
                .expect("task dir")
                .filter(|e| {
                    let Ok(e) = e else { return false };
                    std::fs::read_to_string(e.path().join("comm"))
                        .is_ok_and(|c| c.starts_with("PVAC-connect") || c.starts_with("PVAC-dial"))
                })
                .count()
        }

        /// The bound the dial seam owes the target: **N failed dials must not
        /// cost N threads.**
        ///
        /// Every `std::thread` leaks 128 B permanently on RTEMS — its TLS key
        /// is freed before the key's destructor runs — so the cost that
        /// matters is thread *creations*, not threads alive. A search engine
        /// whose name server is unreachable redials roughly every 10 s for as
        /// long as the IOC runs, which under a per-attempt dial thread is a
        /// leak with no ceiling.
        ///
        /// The dials here are sequential — each is awaited to its failure
        /// before the next is issued, exactly the redial loop's shape — and
        /// they are aimed at a blackhole so that the threads serving them are
        /// still alive to be counted: a refusal would retire the old
        /// per-attempt thread before this test could see it and prove nothing.
        ///
        /// Fails on the per-attempt shape (12 live `PVAC-connect` threads, one
        /// per attempt, each pinned under the OS connect ladder); passes on the
        /// pool (at most `MAX_DIAL_WORKERS`, and past the first four every
        /// further dial queues and still fails at its own bound).
        #[test]
        fn sequential_failed_dials_do_not_grow_the_dial_thread_count() {
            // Long enough to outgrow the bound it asserts, whatever the bound
            // is set to.
            const DIALS: usize = MAX_DIAL_WORKERS * 3;

            let (addr, _listener, _fillers) = syn_blackhole();
            let before = dial_threads();

            for i in 0..DIALS {
                let (tx, rx) = std::sync::mpsc::channel();
                epics_base_rs::runtime::task::spawn(async move {
                    let _ = tx.send(
                        dial_pva(addr, Duration::from_millis(200), Duration::from_secs(10))
                            .await
                            .map(|_| ()),
                    );
                });
                let res = rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap_or_else(|e| panic!("dial {i} must resolve at its bound: {e}"));
                assert!(
                    matches!(res, Err(PvaError::Timeout)),
                    "dial {i} toward a blackhole must fail with Timeout, got {res:?}"
                );
            }

            let after = dial_threads();
            assert!(
                after <= before + MAX_DIAL_WORKERS,
                "{DIALS} sequential failed dials created {} dial threads \
                 (from {before} to {after}); the dial seam must borrow from a \
                 bounded set of at most {MAX_DIAL_WORKERS} permanent workers, \
                 not create one per attempt — on RTEMS each creation leaks \
                 128 B that is never returned",
                after - before
            );
        }

        /// Boundary: a caller that goes away mid-dial leaks nothing.
        ///
        /// An abandoned dial leaves the pool in one of exactly two states, and
        /// **both are correct** — which is why this test branches rather than
        /// assuming one of them:
        ///
        /// * the worker was already inside its `connect`, so a socket exists
        ///   and the worker must be its single finalizer — its send to the
        ///   dropped receiver fails and the fresh socket is dropped, and
        ///   closed, before it takes its next request;
        /// * the worker only reached the request *after* the caller had gone,
        ///   saw `Sender::is_closed()` and skipped the connect, so no socket
        ///   was ever opened.
        ///
        /// The first was asserted unconditionally until the CA twin of this
        /// test hung on it in 2 of 4 loaded runs: between the single `poll`
        /// and the `drop` there is a `/proc` scan, milliseconds of window for
        /// the worker not to have popped the request yet, and the blocking
        /// `accept` then waited forever for a connection that was correctly
        /// never made. The race was never observed here — ~14 loaded runs of
        /// this suite all took the first branch — but the sequence is
        /// identical, so the hazard is closed in both rather than left to
        /// timing.
        ///
        /// The worker itself deliberately does **not** retire; that is the
        /// bound. So the tail asserts the stronger property unconditionally:
        /// the same worker goes on to serve the next dial.
        #[test]
        fn dropped_dial_future_leaks_no_socket_and_returns_its_worker() {
            let (addr, listener, fillers) = syn_blackhole();
            let mut fut = Box::pin(dial_pva(
                addr,
                Duration::from_secs(30),
                Duration::from_secs(30),
            ));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            // The first poll spawns the dial thread and parks on the oneshot.
            assert!(
                fut.as_mut().poll(&mut cx).is_pending(),
                "a dial toward a blackhole must be pending, not resolved on \
                 the caller's thread"
            );
            assert_eq!(
                dial_threads(),
                1,
                "the first poll must have handed the connect to a dedicated \
                 dial thread"
            );

            // The caller goes away mid-dial.
            drop(fut);

            // Un-blackhole: drain the fillers so that, if the worker did enter
            // its connect, the abandoned dial's SYN retry (~1 s, ~3 s)
            // completes.
            for _ in 0..fillers.len() {
                let _ = listener.accept().expect("drain a filler");
            }
            drop(fillers);

            // `accept` honours `SO_RCVTIMEO` on Linux, so this is the branch
            // discriminator: a connection means the worker was mid-connect,
            // and `WouldBlock` past the SYN ladder's first two retries means
            // it skipped. Either way nothing may be left open.
            listener
                .set_read_timeout(Some(Duration::from_secs(6)))
                .expect("accept timeout");
            match listener.accept() {
                Ok((ours, _)) => {
                    // Single finalizer: the worker closed the socket it
                    // opened, so the accepted side reads EOF, not a half-open
                    // connection.
                    let ours: std::net::TcpStream = ours.into();
                    ours.set_read_timeout(Some(Duration::from_secs(10)))
                        .expect("read timeout");
                    use std::io::Read;
                    let mut buf = [0u8; 1];
                    let n = (&ours).read(&mut buf).expect("read on the abandoned dial");
                    assert_eq!(
                        n, 0,
                        "the worker was inside its connect, so it owns the \
                         socket it opened and must close it once its receiver \
                         is gone"
                    );
                }
                Err(e) if epics_base_rs::runtime::blocking_io::is_socket_timeout(e.kind()) => {
                    // The worker reached the request after the caller had gone
                    // and skipped the connect: no socket was opened, so there
                    // is none to finalise.
                }
                Err(e) => panic!("accept on the abandoned dial: {e}"),
            }

            // …and it is back in service rather than retired: the next dial
            // is served without a second thread being created.
            let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let live_addr = live.local_addr().expect("addr");
            let acceptor = std::thread::spawn(move || live.accept().expect("accept").0);
            let (tx, rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let _ = tx.send(
                    dial_pva(live_addr, Duration::from_secs(5), Duration::from_secs(10))
                        .await
                        .map(|_| ()),
                );
            });
            rx.recv_timeout(Duration::from_secs(6))
                .expect("the next dial must resolve")
                .expect("a dial to a live listener must succeed");
            assert_eq!(
                dial_threads(),
                1,
                "the worker that finalised the abandoned socket must serve the \
                 next dial, not be replaced by a fresh thread"
            );
            drop(acceptor.join().expect("acceptor"));
        }

        /// Boundary: the application-level bound (pvxs `operationTimeout`)
        /// still applies with the plain-blocking connect — it is enforced by
        /// the awaiting side's `timeout`, not by the (target-broken)
        /// `connect_timeout` poll path, so a blackholed dial fails the
        /// caller at the deadline even while the dial thread is still
        /// blocking under the OS connect ladder.
        #[test]
        fn dial_times_out_at_the_application_bound() {
            let (addr, _listener, _fillers) = syn_blackhole();

            let started = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let _ = tx.send(
                    dial_pva(addr, Duration::from_millis(500), Duration::from_secs(10))
                        .await
                        .map(|_| ()),
                );
            });
            let res = rx
                .recv_timeout(Duration::from_secs(3))
                .expect("the dial must resolve at its application bound");
            assert!(
                matches!(res, Err(PvaError::Timeout)),
                "a blackholed dial must fail with Timeout at the application \
                 bound, got {res:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the bound took {:?}; it must fire at ~500 ms, not wait out \
                 the OS connect ladder",
                started.elapsed()
            );
        }

        /// A dial against a live listener that answers succeeds and the
        /// returned halves carry bytes — the plain-blocking connect path,
        /// end to end. (SYN-ACK latency proper cannot be injected on
        /// loopback without privileges; delayed *completion* is covered by
        /// `dial_completing_after_syn_retry_is_usable`.)
        #[test]
        fn dial_to_live_listener_succeeds() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr");

            let (bytes_tx, bytes_rx) = std::sync::mpsc::channel();
            let acceptor = std::thread::spawn(move || {
                let (ours, _) = listener.accept().expect("accept the dial");
                ours.set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                use std::io::Read;
                let mut buf = [0u8; 4];
                (&ours).read_exact(&mut buf).expect("read the dialed bytes");
                let _ = bytes_tx.send(buf);
            });

            let (dial_tx, dial_rx) = std::sync::mpsc::channel();
            epics_base_rs::runtime::task::spawn(async move {
                let res =
                    match dial_pva(addr, Duration::from_secs(10), Duration::from_secs(10)).await {
                        Ok((reader, mut writer)) => writer
                            .write_all(b"live")
                            .await
                            .map(|()| (reader, writer))
                            .map_err(PvaError::Io),
                        Err(e) => Err(e),
                    };
                let _ = dial_tx.send(res);
            });

            let _halves = dial_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("dial must resolve")
                .expect("a dial to a live listener must succeed");
            let got = bytes_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the dialed connection must carry bytes");
            assert_eq!(&got, b"live");
            acceptor.join().expect("acceptor thread");
        }
    }
}
