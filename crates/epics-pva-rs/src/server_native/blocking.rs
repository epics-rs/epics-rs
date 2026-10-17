//! Blocking, thread-per-connection PVA server driver (RTEMS phase 6 item 5,
//! stage 3).
//!
//! The second driver, beside `super::accept`. It serves one connection with
//! three threads and **no reactor**:
//!
//! ```text
//!   socket --read--> reader pump --Vec<u8> chunks--> ChannelReader
//!                                                       |
//!                                  operation thread: block_on_sync(
//!                                      handle_connection_io(..))
//!                                                       |
//!   socket <--write-- writer pump <--framed bytes-- ChannelWriter
//! ```
//!
//! Both pumps and both adapters come from `runtime::blocking_io`; see "the seam
//! is the byte source" below.
//!
//! # The seam is the byte source, not the frame pipeline
//!
//! `handle_connection_io` already takes its reader and writer as
//! `Box<dyn AsyncRead/AsyncWrite>` (§1.2). So this driver adds *implementors*,
//! not a second protocol: every byte still reaches the same parser, the same
//! `select!`, the same handlers. Nothing in the 21,000-line protocol module is
//! `cfg`-ed, and the hosted driver is not touched.
//!
//! Those implementors are no longer defined here. `ChannelReader` /
//! `ChannelWriter`, the two pump bodies, the send-deadline loop and the two
//! thread-lifecycle guards live in
//! [`epics_base_rs::runtime::blocking_io`],
//! because the PVA *client* and the CA client need the identical primitive and
//! `epics-ca-rs` cannot depend on this crate. What remains in this file is
//! what is genuinely server-side: the accept loop, the [`ConnRegistry`], the
//! UDP search responder, and the assembly that gives one connection its two
//! pumps.
//!
//! Crucially the threads hand over **bytes, not frames** (§1.3). The inbound
//! type cache, the segment buffer and the channel map stay exactly where they
//! are — inside the one loop that dispatches frames in wire order — so a
//! client that defines a descriptor with `0xFD <slot>` in one frame and
//! references it with `0xFE <slot>` in a later one still resolves. That
//! invariant holds *by construction* here rather than by a new protocol
//! between two threads.
//!
//! # Where the async goes on each target
//!
//! `block_on_sync` (`epics_base_rs::runtime::task`) is the single bridge. On a
//! bare thread it parks; on a multi-thread runtime worker it hands the worker
//! off first. That difference matters, because on a **hosted** build the
//! operation future still needs a tokio runtime underneath it: `runtime::task`
//! aliases `spawn` and `interval` to tokio's, and tokio's need a reactor. So
//! `serve_connection_blocking` must be called from a multi-thread runtime
//! worker on the host. On RTEMS the `exec_backend` supplies both the spawn
//! pool and the timer, so the very same call runs on a bare thread. This
//! module is therefore host-compiled and host-tested — the only way to show
//! hosted behaviour is unchanged — and the reader/writer threads and both
//! adapters are exercised for real either way.
//!
//! # What "no reactor" is worth — at its measured strength, and no higher
//!
//! Not that a reactor cannot run on RTEMS. It can: a libevent reactor serves
//! PVA on RTEMS 6 once steered to `kqueue`, measured end to end. The claim
//! that survives measurement is narrower and still real — **the reference
//! implementation ships an RTEMS-5-era workaround that makes it unusable on
//! RTEMS 6 today**.
//! pvxs `src/evhelper.cpp:177-184` carries `#ifdef __rtems__
//! event_config_avoid_method(conf, "kqueue")`, written for "libbsd circa
//! RTEMS 5.1", and it steers libevent onto the `poll` backend, which never
//! blocks on this BSP: `poll()` returns `POLLERR` immediately on libevent's
//! internal notify FIFO, so one 4.000 s loop issues 148,081 `poll()` calls
//! against 1 for a raw `poll()`, with guest idle 33.6 % against 97.9 %.
//! Finding that took a `--wrap=poll` interposer and CPU-idle attribution.
//!
//! This driver does not depend on a reactor, so it never meets that class of
//! defect — which is a different statement from being the only thing that can
//! run there, and the weaker one is the true one.
//!
//! # Shutdown: one wake primitive, three triggers
//!
//! A thread cannot be aborted the way the hosted accept loop aborts a task in
//! its `JoinSet`, and PVA's `op_timeout` defaults to ~64,000 s so `SO_RCVTIMEO`
//! cannot stand in either (§1.6, §4.2c). So there is exactly one way a parked
//! connection thread is woken here — `ConnWake::wake`, i.e.
//! `shutdown(Shutdown::Both)` on that connection's socket — and all three
//! termination triggers go through it:
//!
//! | trigger | who wakes | the loop then sees |
//! |---|---|---|
//! | client EOF / read error (§4.2a) | nobody — the reader already returned | `Protocol("client closed")` |
//! | writer death: write error or send deadline (§4.2b) | the writer pump, on its way out | `Protocol("client closed")` |
//! | server stop (§4.2c) | [`ConnRegistry::stop`], walking every live connection | `Protocol("client closed")` |
//!
//! The first two are a connection retiring *itself*, and each is performed by
//! the pump guard that already owns that connection's socket
//! (`runtime::blocking_io`). The third is the *server-wide* transition, and
//! [`ConnRegistry`] is its **single owner**:
//!
//! > **MUST** every connection served here is registered before either of its
//! > pumps starts and stays registered until both have joined.
//! > **MUST NOT** any path take a connection out of the registry other than
//! > through the registration guard, whose `Drop` is the only remover; and no
//! > path outside a connection may reach that connection's socket other than
//! > through the `ConnWake` that only `ConnRegistry::register` constructs.
//!
//! Both halves hold by construction; see [`ConnRegistry`] for how.
//!
//! That third column is the point. §4.2b proposed closing the writer-death
//! window with a `oneshot` and a **seventh `select!` arm** in the shared
//! connection loop — which would have changed hosted teardown timing (a
//! sign-off item) or else `cfg`-ed an arm into the protocol module. Waking
//! through the socket needs neither: a dead writer shuts the socket, the
//! reader thread's `read` returns 0, and the connection unwinds down the
//! **existing** EOF path. So `tcp.rs` is untouched by stage 4, the hosted
//! `select!` is byte-identical, and the ≤15 s window is gone for this driver
//! anyway.
//!
//! # The accept side
//!
//! [`BlockingPvaServer`] owns the [`ConnRegistry`] and hands sockets to
//! `serve_connection_blocking`, assembling the arguments the hosted
//! `super::accept` assembles for its own driver. It creates **no thread per
//! connection**: each connection *borrows* its three threads — connection body,
//! reader pump, writer pump — as one set from a
//! [`epics_base_rs::runtime::worker_pool::WorkerPool`], because
//! every `std::thread` creation leaks 176–179 B permanently on RTEMS. All
//! three workers take `PVA_SERVER_PRIORITY` — see that constant for why they
//! share one number — and each is created once, with its stack class and band,
//! and reused. A full pool refuses admission with `EAGAIN`, which *is* the
//! `max_connections` limit.
//!
//! # Discovery
//!
//! [`BlockingPvaServer::serve_udp_search`] is the second loop, on its own
//! thread at `PVA_UDP_PRIORITY`: one `std::net::UdpSocket`, a 200 ms read
//! timeout as the stop seam, and [`super::search_engine`]'s decode driven
//! through `block_on_sync`. It sends what the decode returns and decides
//! nothing about the protocol itself, so its replies are byte-identical to the
//! hosted responder's.
//!
//! # Not here
//!
//! Beacons and the per-NIC send bundle need the interface enumerator, and the
//! loopback-multicast ORIGIN_TAG channel needs socket options, that do not
//! cross to RTEMS or VxWorks (no `socket2` there), so an embedded build has no
//! ORIGIN_TAG: every datagram is `Direct`. A client that unicasts or broadcasts
//! a SEARCH to this server's port is answered; one that relies on being told we
//! exist is not, yet. The hosted build joins the group on its one search socket
//! ([`bind_udp_search`]).

// RTEMS-EXEC-MODEL-ALLOW(22): checked - all 22 run and pass in the
// exec-backend suite. The three inactivity-deadline tests were added to the
// count after running them there: the deadline they exercise lives in the
// writer task, which is the same task on both backends. So was the
// read-stall watchdog test, whose ticker is on the same `LivenessClock`. `harness_reactor`'s `new_multi_thread` builder used to be
// counted as a 17th; it sits under `#[cfg(tokio_backend)]`, so the guard now
// accounts for it by its own gate and it must not be counted here too.
use std::collections::HashMap;
use std::io;
use std::net::{
    IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs,
    UdpSocket,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use epics_base_rs::net::{PortOrigin, bind_fanout};
use epics_base_rs::runtime::accept::AcceptBackoff;
use epics_base_rs::runtime::blocking_io::{
    DEFAULT_READ_CHUNK, is_socket_timeout, spawn_reader_pump, spawn_writer_pump,
};
use epics_base_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, enter_ioc_thread,
};
use epics_base_rs::runtime::worker_pool::{AcquireError, Worker, WorkerPool, WorkerRole};
use tracing::{debug, warn};

use super::config::PvaServerConfig;
use super::peers::{ConnSlot, PeerEntry, PeerRegistry};
use super::search_engine::{
    Origin, SearchOutput, filter_inbound, process_search_datagram, random_guid,
};
#[cfg(not(epics_embedded_target))]
use super::search_engine::{SearchListeners, ServerInterfaces, local_v4_addrs};
use super::serv_iface::ServListeners;
use super::source::{ChannelInvalidator, DynSource};
use super::tcp::{ConnInit, TX_LIMIT_FALLBACK, handle_connection_io};
use crate::error::{PvaError, PvaResult};
use crate::liveness_clock::LivenessClock;

/// The EPICS priority every PVA server thread runs at.
///
/// pvxs runs its TCP acceptor **and** the reactor that reads, decodes and
/// writes every connection on **one** thread, `PVXTCP`, at
/// `epicsThreadPriorityCAServerLow-2` (`pvxs/src/server.cpp:388`), where
/// `epicsThreadPriorityCAServerLow = 20` (`epicsThread.h:82`) — so 18. We
/// split that same body of work across an accept thread plus a reader,
/// operation and writer thread per connection, and all four take 18:
/// splitting how the work is scheduled *internally* must not change how it is
/// scheduled relative to everything else. Raising one of them (the writer,
/// say) would be a design decision needing its own justification, not a
/// default.
///
/// This sits below CA's per-client threads — `CaServerLow` = 20 for a client
/// connection and 19 for its event thread — which is the upstream ordering
/// for an IOC serving both protocols. It is level with CA's own accept loop,
/// which takes the same `CaServerLow-2` from `caservertask.c`'s ladder
/// (`epics_ca_rs::server::blocking`'s `CAS_TCP_PRIORITY`); pvxs puts its UDP
/// search collector lower still, at `CaServerLow-4` = 16
/// (`udp_collector.cpp:93`), where CA's name-search responder also sits, so a
/// SEARCH flood cannot starve established connections.
///
/// Applying it is best effort and gated on
/// `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING`, which defaults on as C's
/// does (`runtime::task::DEFAULT_POLICY`). On RTEMS the platform arm really does
/// call `pthread_setschedparam`, and the number is load-bearing there: RTEMS
/// pthreads inherit `POSIX_Init`'s priority, so a thread that does not take a
/// band runs one level above idle rather than "at the default".
pub(super) const PVA_SERVER_PRIORITY: ThreadPriority = ThreadPriority::Custom(18);

/// The executor a PVA server's spawned connection work runs on, or `None`
/// where there is no band to choose.
///
/// `Reactor::spawn` with nothing bound lands a future on the process-global
/// callback pool's middle band — `epicsThreadPriorityScanLow + 4` = 64, which
/// is C's band for deferred record processing. pvxs runs its connection
/// reactor at `CAServerLow-2` = 18, and the accept loop and poll thread here
/// already do; the connection futures they start did not, so the server's work
/// sat one ladder over from where the rest of it runs and shared its rings
/// with record tails.
///
/// One worker each: the reactor driver's whole claim is that connections cost
/// no thread, and its futures never block (every socket is a `ReadyStream`).
/// The blocking driver's per-connection threads are unaffected — this carries
/// the tasks those threads spawn.
///
/// A server starts two: [`PVA_TASK_EXECUTOR`] for the connection and monitor
/// tasks, [`PVA_WRITER_EXECUTOR`] for the connections' writer tasks. One scan
/// pass wakes a monitor task per subscription, and on one FIFO worker a writer
/// woken by the first frame waited behind all of them.
///
/// `None` on `tokio_backend`: there the runtime is the executor and its band
/// is the runtime's, so an executor here would be a thread nothing uses.
#[cfg(exec_backend)]
pub(super) fn pva_executor(
    name: &str,
) -> std::io::Result<Option<std::sync::Arc<epics_base_rs::runtime::background::DedicatedExecutor>>>
{
    epics_base_rs::runtime::background::DedicatedExecutor::new(name, PVA_SERVER_PRIORITY, 1)
        .map(|e| Some(std::sync::Arc::new(e)))
}

/// See the `exec_backend` twin: on this backend there is no band to choose.
#[cfg(tokio_backend)]
pub(super) fn pva_executor(
    _name: &str,
) -> std::io::Result<Option<std::sync::Arc<epics_base_rs::runtime::background::DedicatedExecutor>>>
{
    Ok(None)
}

/// [`pva_executor`]'s name for the connection and monitor tasks.
pub(super) const PVA_TASK_EXECUTOR: &str = "PVASTASK";
/// [`pva_executor`]'s name for the writer tasks (pvxs `PVXTCP`).
pub(super) const PVA_WRITER_EXECUTOR: &str = "PVASTX";

/// The EPICS priority the UDP SEARCH responder runs at — deliberately **not**
/// [`PVA_SERVER_PRIORITY`].
///
/// pvxs runs its UDP collector on `PVXUDP` at
/// `epicsThreadPriorityCAServerLow-4` (`pvxs/src/udp_collector.cpp:93`), two
/// steps below the `PVXTCP` acceptor/reactor's `CAServerLow-2`
/// (`pvxs/src/server.cpp:388`), where `epicsThreadPriorityCAServerLow = 20`
/// (`epicsThread.h:82`) — so 16 against 18.
///
/// The gap is the point, not an accident of upstream's numbering: SEARCH is
/// broadcast traffic from every client on the subnet, and an established
/// connection's reads, writes and operations must not be starved by a search
/// storm the server did not ask for. Unifying the two constants would silently
/// remove that protection, so `the_udp_responder_runs_below_the_connection_threads`
/// pins both numbers and their ordering.
const PVA_UDP_PRIORITY: ThreadPriority = ThreadPriority::Custom(16);

/// Depth of the reader → operation byte-chunk channel.
///
/// **One** (§1.4, resolved). The hosted read is strictly demand-driven —
/// `read_frame` issues one read per poll and the loop dispatches a frame fully
/// before reading again — so a depth of 1 reproduces it with at most one chunk
/// of read-ahead, which the kernel receive buffer already provides. A larger
/// depth would let a fast client queue chunks while a slow source blocks the
/// dispatcher: a behaviour change, not an optimisation.
const CHUNK_QUEUE_DEPTH: usize = 1;

/// Depth of the operation → writer frame channel. Same reasoning: the producer
/// (the connection's writer task) emits one frame at a time and waits for it,
/// so depth 1 gives the task the same backpressure a blocking socket write
/// would.
const FRAME_QUEUE_DEPTH: usize = 1;

// ---------------------------------------------------------------------------
// Shutdown: the wake handle and the registry that walks them
// ---------------------------------------------------------------------------

/// The one primitive that wakes a parked connection thread:
/// `shutdown(Shutdown::Both)` on that connection's socket.
///
/// **Only [`ConnRegistry::register`] constructs one.** That is what makes the
/// registry the single owner rather than merely the usual route: a wake handle
/// cannot exist for a connection the registry does not know about, so there is
/// no way to write a shutdown path that `stop` would miss.
///
/// It owns an `Arc` of the socket rather than borrowing a dup from one of the
/// connection's threads, and that is the load-bearing part. A connection's
/// three threads all exit and drop their own handles; a `stop` walking the
/// registry at that moment must still hold something that keeps the fd open,
/// or it would `shutdown` a fd number the OS has already handed to another
/// connection. With the `Arc` here, waking a connection that has already ended
/// is a no-op on a still-open fd.
#[derive(Clone)]
struct ConnWake(Arc<TcpStream>);

impl ConnWake {
    fn wake(&self) {
        // `ENOTCONN` when the peer has already gone: there was nothing to
        // wake, which is not a failure of anything.
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

#[derive(Default)]
struct RegistryState {
    stopped: bool,
    next_id: u64,
    conns: HashMap<u64, ConnWake>,
}

/// Server-wide registry of live blocking connections, and the single owner of
/// their shutdown transition (§4.2c).
///
/// The hosted accept loop keeps its connections in a `tokio::task::JoinSet`
/// and drops it to abort them all. Threads have no such handle, so the
/// blocking driver keeps this instead: every live connection's wake handle,
/// walked by [`stop`](Self::stop).
///
/// # Invariant
///
/// * **MUST** — every connection served by `serve_connection_blocking` is
///   registered here before either of its threads starts, and stays registered
///   until its operation thread has joined both of them.
/// * **MUST NOT** — no path shuts a connection's socket down, or takes it out
///   of the registry, except through a handle this registry issued: a
///   `ConnWake` (which only `ConnRegistry::register` constructs) or the
///   `ConnRegistration` guard (whose `Drop` is the only remover, and which
///   goes through `ConnRegistry::deregister` rather than reaching into
///   the map).
///
/// Both halves hold by construction, not by convention. Registration is not
/// something a caller can forget or double up — it happens inside
/// `serve_connection_blocking`, which cannot be called without a registry —
/// and the connection's own natural death removes its entry through the same
/// API a `stop` would, so there is one code path for "this connection is
/// gone", not two.
///
/// `stop` is a one-way latch: afterwards a connection that registers (an
/// accept already in flight when `stop` ran) is woken as it registers, so it
/// retires down the same path as everything else instead of needing a second
/// one. Calling `stop` twice is harmless.
///
/// Target-neutral, like the rest of this module: `std` sockets, `std` mutex,
/// no `cfg`. Item 7's RTEMS accept loop owns one and calls `stop`; on the host
/// that role belongs to `super::accept`, which drives its connections as
/// tasks and so has nothing to register here.
pub struct ConnRegistry {
    state: Mutex<RegistryState>,
}

impl Default for ConnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnRegistry {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
        }
    }

    /// The only way this type takes its lock, and the reason it never
    /// propagates poisoning.
    ///
    /// This mutex is **process-wide**: every connection registers and
    /// deregisters through it, and the accept loop takes it for every accept.
    /// So `.expect()` here does not fail one operation — one connection that
    /// panics anywhere between `register` and `deregister` poisons the mutex,
    /// and from then on *every* subsequent accept panics too. One client takes
    /// the server down. (Contrast the CA driver's send-lock `expect`s, which
    /// look identical but sit on a **per-client** mutex, so the blast radius
    /// is the one connection that already died. The difference is ownership
    /// scope, not care.)
    ///
    /// A poisoned registry is recoverable state, not corrupt state. The
    /// protected value is a `HashMap<u64, ConnWake>` and an id counter; a
    /// panic part-way through an insert or a remove leaves the map itself
    /// intact — at worst one entry is present that should not be, and the
    /// wake it holds is idempotent (`shutdown` on an already-shut socket is
    /// harmless). Losing every future connection is strictly worse than
    /// carrying on with that.
    fn state(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            // Deliberately not re-poisoning and not logging per-acquisition:
            // this is taken on every accept, and a poisoned mutex stays
            // poisoned, so a log line here would flood.
            poisoned.into_inner()
        })
    }

    /// Shut every live connection's socket, and every connection that
    /// registers from here on.
    ///
    /// Returns as soon as the wakes are issued. Each connection retires on its
    /// own operation thread, which is what joins its reader and writer; a
    /// caller that must know they are all gone joins whatever it spawned those
    /// operation threads with.
    pub fn stop(&self) {
        let wakes: Vec<ConnWake> = {
            let mut st = self.state();
            st.stopped = true;
            st.conns.values().cloned().collect()
        };
        // Outside the lock: `shutdown` is a syscall, and a connection retiring
        // concurrently takes this same lock in its registration guard.
        for wake in wakes {
            wake.wake();
        }
    }

    /// How many connections are currently registered. A connection that ended
    /// on its own has already removed itself.
    pub fn live_connections(&self) -> usize {
        self.state().conns.len()
    }

    /// Take ownership of a connection's socket, so [`stop`](Self::stop) can
    /// reach it.
    ///
    /// The registry's `Arc` is the *server-wide* route to this socket, and the
    /// only one: a connection's own two pump guards each hold an `Arc` of the
    /// same descriptor and retire their own thread with it, but neither is
    /// reachable from outside the connection. So `stop` walking `conns` remains
    /// the single owner of the server-wide shutdown transition.
    fn register(&self, socket: Arc<TcpStream>) -> ConnRegistration<'_> {
        let wake = ConnWake(socket);
        let (id, stopped) = {
            let mut st = self.state();
            let id = st.next_id;
            st.next_id += 1;
            st.conns.insert(id, wake.clone());
            (id, st.stopped)
        };
        if stopped {
            wake.wake();
        }
        ConnRegistration { registry: self, id }
    }

    /// The only remover. Private, and reached only from
    /// [`ConnRegistration::drop`].
    fn deregister(&self, id: u64) {
        self.state().conns.remove(&id);
    }
}

/// A connection's registration: its presence in the registry while it lives,
/// and its removal on the way out — every way out, including a panic unwinding
/// through the driver.
struct ConnRegistration<'a> {
    registry: &'a ConnRegistry,
    id: u64,
}

impl Drop for ConnRegistration<'_> {
    fn drop(&mut self) {
        self.registry.deregister(self.id);
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// The reader and writer slots of a connection's leased worker set, passed to
/// `serve_connection_blocking` as one value.
///
/// A pair, not two parameters, because the two are always leased and returned
/// together — `start_connection` takes them from one `acquire`, and neither
/// pump may run on a worker the connection did not lease. Passing them as a unit
/// is what makes "you cannot serve with a half-borrowed set" a property of the
/// signature rather than a convention.
pub(super) struct PumpWorkers {
    pub reader: Worker,
    pub writer: Worker,
}

/// Serve one PVA connection over a blocking [`TcpStream`], on this thread.
///
/// This call *is* the operation thread: it runs `handle_connection_io` to
/// completion and returns the connection's result, having joined both pump
/// jobs. The reader and writer are its children and neither decides the
/// connection is over — they can only report (§4.1).
///
/// `pumps` are two of the three workers this connection borrowed from the pool
/// (`start_connection`); the connection body itself runs on the third. The
/// pumps run on these two, and their guards join the jobs — returning the
/// workers to their set — before this function returns, on every exit path
/// including a panic. The pair is taken by value so a caller cannot run a pump
/// on a worker it did not lease.
///
/// `peer_entry` is this connection's row in the server report (`pvxsr`) and
/// `channel_invalidator` is the server-wide out-of-band disconnect channel;
/// both come from whatever accepts the socket. `registry` is that same
/// accepter's [`ConnRegistry`], and taking it by value rather than by option
/// is deliberate: a connection cannot be served outside a registry, so
/// `ConnRegistry::stop` reaching every live connection is a property of the
/// signature. There is no TLS parameter: the blocking driver is plain-TCP only
/// (§6), so the connection carries no x509 identity and authenticates through
/// CONNECTION_VALIDATION alone.
///
/// **Hosted callers must run this on a multi-thread runtime worker**; see the
/// module docs. On RTEMS a bare thread is correct.
#[allow(clippy::too_many_arguments)] // the `reactor` capability is the 8th
pub(super) fn serve_connection_blocking(
    reactor: epics_base_rs::runtime::task::Reactor,
    pumps: PumpWorkers,
    stream: TcpStream,
    peer: SocketAddr,
    source: DynSource,
    config: PvaServerConfig,
    init: ConnInit,
    registry: &ConnRegistry,
) -> PvaResult<()> {
    let PumpWorkers {
        reader: reader_worker,
        writer: writer_worker,
    } = pumps;
    let _ = stream.set_nodelay(true);
    // SO_RCVTIMEO (the same move CA's blocking driver makes), fatal: every
    // target that runs this accepts it, RTEMS included. Note the receive
    // timeout is NOT a shutdown mechanism here: `op_timeout` defaults to
    // ~64,000 s, so it is effectively infinite (§1.6). What ends a parked
    // reader is the `shutdown` below.
    stream
        .set_read_timeout(Some(config.op_timeout))
        .map_err(PvaError::Io)?;
    let send_timeout = config.send_timeout;
    // No SO_SNDTIMEO. It is not portable — VxWorks' socket stack does not
    // implement it and returns ENOPROTOOPT (errno 42) on an otherwise-good
    // accepted socket, which closed every VxWorks PVA connection before the
    // first byte (SET_BYTE_ORDER) went out when it was propagated fatally
    // (measured on target). It is also no longer needed on any target:
    // `blocking_io::write_frame_deadline` waits for writability against
    // `send_timeout` itself, so the per-frame bound is the same here as it is
    // for the blocking clients, and `handle_connection_io`'s
    // `runtime::task::timeout(send_timeout)` around every `write_all` sits
    // above it as a second, async-side bound rather than as the only one.

    // One socket, several handles: the SAME descriptor shared through an
    // `Arc` by both pump threads and the registry, which owns it from here
    // on. Registering *before* either thread starts is what closes the window
    // where a thread could be parked on a socket `stop` cannot reach.
    //
    // Shared, not duplicated: see `runtime::blocking_io`'s module docs for the
    // measured RTEMS/libbsd reason `try_clone` cannot be used here.
    let stream = Arc::new(stream);
    let read_sock = stream.clone();
    let write_sock = stream.clone();
    let registration = registry.register(stream);

    // Both pumps come from `runtime::blocking_io`, the workspace's one blocking
    // byte source — the same primitive the PVA *client*'s `connect_blocking`
    // uses, in `epics-base-rs` rather than here because `epics-ca-rs` cannot
    // reach this crate. They run on the two pump workers of this connection's
    // leased set, whose band is `PVA_SERVER_PRIORITY` — like every other
    // thread in this module; see that constant for why all of them share one
    // number.
    //
    // Pooled, so infallible: the two threads already exist (they are the leased
    // workers), so there is no per-connection creation left to fail. Admission
    // failure moved up to `pool.acquire` in `start_connection`, where a circuit
    // at capacity is refused before this function is ever entered.
    //
    // From here on the reader is owned by its guard. Every exit below — and a
    // panic unwinding out of the connection handler — runs
    // `ReaderPumpGuard::drop`, which wakes and joins it back into its pool.
    let label = format!("PVA connection {peer}");
    let (reader_adapter, reader) = spawn_reader_pump(
        reader_worker,
        read_sock,
        &label,
        DEFAULT_READ_CHUNK,
        CHUNK_QUEUE_DEPTH,
    );
    let (writer_adapter, writer) = spawn_writer_pump(
        writer_worker,
        write_sock,
        &label,
        send_timeout,
        FRAME_QUEUE_DEPTH,
    );

    let outcome = block_on_sync(handle_connection_io(
        reactor,
        source,
        Box::new(reader_adapter),
        Box::new(writer_adapter),
        peer,
        config,
        init,
    ));

    // Teardown, in §4.3's order and for its reason: the writer goes down
    // first so frames the connection emitted on its way out (MONITOR FINISH,
    // DESTROY_CHANNEL) reach the wire before the socket is torn down. Then the
    // reader, which needs the socket shut to return its parked `read`.
    //
    // These two drops are explicit only to pin that order at the point a
    // reader would look for it. They are not what makes the teardown happen —
    // the guards' own `Drop` is, which is why the `?` and panic paths above are
    // covered too. Their declaration order (reader, then writer) already gives
    // this same reverse-drop order if control never reaches here.
    drop(writer);
    drop(reader);
    // Both threads are joined; the connection is no longer reachable and its
    // entry goes. This is the only removal path there is.
    drop(registration);

    match outcome {
        Ok(result) => result,
        Err(_) => Err(PvaError::Protocol(
            "blocking PVA driver cannot run on a current-thread runtime".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Accept loop (item 7 stage C)
// ---------------------------------------------------------------------------

/// A blocking, thread-per-connection PVA TCP server — the accept side of the
/// driver in this module, and the RTEMS counterpart of `super::accept`.
///
/// `serve_connection_blocking` serves one connection on three threads; this
/// is what gives it sockets and the arguments the hosted accept loop assembles
/// in `accept.rs`. An N-client server therefore costs **3N+2** threads — this
/// accept loop, the UDP search responder, and three per connection — where the
/// hosted driver costs two tasks per connection. That is a stated RTEMS budget
/// item, not an accident.
///
/// It owns the [`ConnRegistry`], because `serve_connection_blocking` cannot
/// be called without one, and that makes [`shutdown`](Self::shutdown) able to
/// end live connections rather than only stop accepting new ones.
///
/// TLS is not served: the blocking driver is plain-TCP only (§6), so every
/// connection is registered with `tls = false` and authenticates through
/// CONNECTION_VALIDATION.
pub struct BlockingPvaServer {
    listeners: ServListeners,
    source: DynSource,
    config: PvaServerConfig,
    peers: Arc<PeerRegistry>,
    channel_invalidator: ChannelInvalidator,
    connections: Arc<ConnRegistry>,
    active: Arc<AtomicUsize>,
    /// The connection's three threads — connection body, reader pump, writer
    /// pump — are borrowed from this pool as one set, never created per accept.
    /// Its capacity is `max_connections`, so `acquire` refusing with
    /// `WouldBlock` *is* the connection limit; the old `active >= max` check is
    /// gone. Dropped after the listener and the connections are stopped, in
    /// [`shutdown`](Self::shutdown) order, so its `Stop`s never queue behind a
    /// live connection.
    conn_pool: WorkerPool<3>,
    /// The executors connection futures and writer tasks spawn onto — see
    /// [`pva_executor`]. `None` on `tokio_backend`, where the runtime is the
    /// executor.
    task_exec: Option<Arc<epics_base_rs::runtime::background::DedicatedExecutor>>,
    writer_exec: Option<Arc<epics_base_rs::runtime::background::DedicatedExecutor>>,
    shutdown: AtomicBool,
}

/// The three roles one PVA connection borrows together, in the order
/// `serve_connection_blocking` and `start_connection` destructure them:
/// `[conn, reader, writer]`. All three take `PVA_SERVER_PRIORITY` — see that
/// constant. The connection body is `Big` (it runs the whole protocol state
/// machine under `block_on_sync`, the counterpart of C's `epicsThreadStackBig`
/// `camsgtask`); the two pumps are `Small`, matching
/// [`circuit_roster`](epics_base_rs::runtime::blocking_io::circuit_roster).
fn connection_roster() -> [WorkerRole; 3] {
    [
        WorkerRole {
            suffix: "conn",
            stack: StackSizeClass::Big,
            priority: PVA_SERVER_PRIORITY,
        },
        WorkerRole {
            suffix: "reader",
            stack: StackSizeClass::Small,
            priority: PVA_SERVER_PRIORITY,
        },
        WorkerRole {
            suffix: "writer",
            stack: StackSizeClass::Small,
            priority: PVA_SERVER_PRIORITY,
        },
    ]
}

impl BlockingPvaServer {
    /// Bind the accept socket.
    ///
    /// Binding happens here and not in [`serve`](Self::serve), so a
    /// constructed server *is* a listening server: [`tcp_port`](Self::tcp_port)
    /// is answerable before any thread exists, and the port is never probed,
    /// released and re-bound.
    ///
    /// One listener per entry of `config.interfaces` after pvxs expansion
    /// (`ServListeners::bind`); `addr` is the listener when that list is
    /// empty and the port an entry without one takes.
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        source: DynSource,
        config: PvaServerConfig,
    ) -> io::Result<Self> {
        let listeners = ServListeners::bind(addr, &config)?;
        // One GUID per server, stamped here for the same reason the hosted
        // runtime stamps it in `start()` (`runtime.rs:224-229`): the
        // TCP-circuit SEARCH handler reads it out of its per-connection config
        // copy while the UDP responder reads it from ours, and a client that
        // discovers us over UDP and then connects must see one identity, not
        // two. Stamping at construction is what makes that true by
        // construction — there is no window in which a thread reads the
        // caller's default zeros.
        //
        // `?` and not a fallback: a server with no entropy source must fail
        // here, loudly, rather than start and advertise a guessable identity.
        // See `random_guid` for why every consumer of a GUID collision
        // degrades *silently*, which is what makes construction the last
        // moment the problem is visible at all.
        let mut config = config;
        config.guid = random_guid()?;
        // The bound port, after any fallback, as the hosted runtime stamps it:
        // `config` is the one record of it, read by the UDP responder.
        config.tcp_port = listeners.port();
        // The reserved `__server` composition, identical to the one
        // `PvaServer::start` performs — this driver used to bind the user
        // source directly, which left it with no server meta-channel and made
        // `pvxlist`/`pvlist-rs` fail against an otherwise healthy server.
        let source = crate::server_native::server_info::compose_with_server_info(source)
            .map_err(io::Error::other)?;
        // Same wiring the hosted single-listener path does at
        // `accept.rs:69-70`: the source needs the handle to force-disconnect
        // downstream channels out of band. Applied to the COMPOSITE, as
        // `start` does — `CompositeSource::set_channel_invalidator` fans the
        // handle out to every child, so a leaf that publishes invalidations
        // still receives it.
        let channel_invalidator = ChannelInvalidator::new();
        source.set_channel_invalidator(channel_invalidator.clone());
        // Read before `config` is moved into the struct; it is the pool's
        // capacity, and the pool's capacity is the connection limit.
        let max_connections = config.max_connections;
        // Started here for the reason the worker pool is: a server that cannot
        // run its connections' tasks is a `bind` failure.
        let task_exec = pva_executor(PVA_TASK_EXECUTOR)?;
        let writer_exec = pva_executor(PVA_WRITER_EXECUTOR)?;
        Ok(Self {
            listeners,
            source,
            config,
            peers: PeerRegistry::new(),
            channel_invalidator,
            connections: Arc::new(ConnRegistry::new()),
            active: Arc::new(AtomicUsize::new(0)),
            // `PVAS`: RTEMS truncates thread names at 16 bytes, and the pool
            // appends `-{suffix} {index}` (e.g. `PVAS-reader 3`). Capacity is
            // the connection limit — admission refuses past it.
            conn_pool: WorkerPool::new("PVAS", connection_roster(), max_connections),
            task_exec,
            writer_exec,
            shutdown: AtomicBool::new(false),
        })
    }

    /// The actual bound address — the value to use when the configured port
    /// was 0.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listeners.local_addr()
    }

    /// The bound TCP port, as SEARCH replies must advertise it.
    pub fn tcp_port(&self) -> u16 {
        self.config.tcp_port
    }

    /// This server's GUID, as both SEARCH paths advertise it.
    pub fn guid(&self) -> [u8; 12] {
        self.config.guid
    }

    /// Live per-connection accounting, for the server report (`pvxsr`).
    pub fn peers(&self) -> &Arc<PeerRegistry> {
        &self.peers
    }

    /// The served source, `__server` composed in.
    pub fn source(&self) -> &DynSource {
        &self.source
    }

    /// Connections currently being served.
    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Answer UDP SEARCHes on `socket` until [`shutdown`](Self::shutdown).
    ///
    /// Blocks the calling thread and takes it to `PVA_UDP_PRIORITY`, so give
    /// it a thread of its own — a *different* one from [`serve`](Self::serve),
    /// which runs higher.
    ///
    /// Bind the socket with [`bind_udp_search`] and pass it in, rather than
    /// having this bind it: the same reason [`bind`](Self::bind) binds the
    /// listener. The caller then knows the search port is owned before any
    /// thread exists, and for an ephemeral port can read the number back off
    /// the socket it still holds.
    pub fn serve_udp_search(&self, socket: SearchSocket) -> PvaResult<()> {
        handle_udp_search_blocking(
            socket,
            &self.source,
            &self.config,
            self.config.tcp_port,
            &self.shutdown,
        )
    }

    /// This server's beacons; see
    /// [`PvaServerDriver::beacon_sender`](super::driver::PvaServerDriver::beacon_sender).
    pub fn beacon_sender(&self, udp_port: u16) -> io::Result<super::driver::BeaconSender> {
        super::beacon::BeaconSender::bind(self.source.clone(), &self.config, udp_port)
    }

    /// `reactor` bound to this server's own executor, or `reactor` unchanged
    /// where there is none to bind (`tokio_backend`).
    ///
    /// Called once per `serve`, at the top, so nothing below it can reach the
    /// caller's reactor by accident: the band is a property of the server, not
    /// of the call site that happens to spawn.
    fn conn_reactor(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
    ) -> epics_base_rs::runtime::task::Reactor {
        match &self.task_exec {
            Some(exec) => reactor.with_executor(exec),
            None => reactor.clone(),
        }
    }

    /// `reactor` bound to this server's writer executor (see [`pva_executor`]).
    fn writer_reactor(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
    ) -> epics_base_rs::runtime::task::Reactor {
        match &self.writer_exec {
            Some(exec) => reactor.with_executor(exec),
            None => reactor.clone(),
        }
    }

    /// Accept until [`shutdown`](Self::shutdown).
    ///
    /// Blocks the calling thread and takes it to `PVA_SERVER_PRIORITY`, so
    /// give it a thread of its own rather than calling it on a thread that has
    /// other work.
    pub fn serve(&self, reactor: &epics_base_rs::runtime::task::Reactor) {
        // From here down `reactor` is the server's own — see the reactor
        // driver's `serve`. It carries the per-connection writer task, which
        // is this driver's one spawned future.
        let reactor = &self.conn_reactor(reactor);
        self.listeners
            .serve(|listener| self.accept_loop(listener, reactor));
    }

    /// One listener's accept loop, on its own thread.
    fn accept_loop(&self, listener: &TcpListener, reactor: &epics_base_rs::runtime::task::Reactor) {
        let _ = enter_ioc_thread(PVA_SERVER_PRIORITY);
        let mut backoff = AcceptBackoff::new();
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    backoff.accepted();
                    // `shutdown` wakes this parked `accept` by dialling our own
                    // socket; that throwaway connection arrives here and is
                    // dropped with the flag already set.
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let peer = match stream.peer_addr() {
                        Ok(peer) => peer,
                        Err(e) => {
                            warn!(error = %e, "blocking PVA server: peer_addr failed, dropping connection");
                            continue;
                        }
                    };
                    if let Err(e) = self.start_connection(reactor, stream, peer) {
                        warn!(?peer, error = %e, "blocking PVA server: connection not started");
                    }
                }
                Err(e) => {
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    warn!(error = %e, "blocking PVA server: accept failed");
                    // The failed connection is still queued, so an immediate
                    // retry spins at 100% CPU exactly when the machine is out
                    // of fds or memory. See `runtime::accept` for why the
                    // retry/give-up decision cannot be made from `e`.
                    thread::sleep(backoff.failed());
                    // `shutdown()` may have been asked for while we slept, and
                    // its self-dial cannot wake an `accept()` that is failing.
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
        }
    }

    /// Borrow a worker set, then hand the socket to its connection worker. The
    /// lease and the slot are both moved into the connection body, so both come
    /// back however that body ends — clean return, error, or a panic.
    ///
    /// Admission lives in [`WorkerPool::acquire`] now, not in an `active` check:
    /// the pool's capacity *is* `max_connections`, so a full pool refuses with
    /// [`AcquireError::AtCapacity`], which this maps to the same operator-visible
    /// warning the old gate produced. Any other `acquire` error is a target out
    /// of thread resources and propagates to the accept loop's "connection not
    /// started" path.
    ///
    /// That split used to be written as `e.kind() == io::ErrorKind::WouldBlock`,
    /// and it was wrong for the case that matters: a failed thread spawn is
    /// `EAGAIN`, `std` decodes `EAGAIN` as `WouldBlock`, so a target that had
    /// run out of thread resources was reported to the operator as
    /// `max_connections reached` — pointing at a config knob when the machine
    /// was out of memory. Matching the gate instead of an errno makes the two
    /// unmixable; the CA driver had the same defect on its own refusal path.
    fn start_connection(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
        stream: TcpStream,
        peer: SocketAddr,
    ) -> io::Result<()> {
        let local_port = stream.local_addr()?.port();
        // One atomic borrow of all three roles — connection body, reader pump,
        // writer pump — so a server at capacity can never hold a partial set and
        // block for the rest.
        let (lease, [conn_worker, reader_worker, writer_worker]) = match self.conn_pool.acquire() {
            Ok(set) => set,
            Err(AcquireError::AtCapacity { capacity }) => {
                // Dropping the stream closes it. Refusing costs more here than on
                // the host: each connection is three threads, not two tasks.
                //
                // `warn!`, not `debug!`: a client that cannot connect is an
                // operator-visible event, and at `debug!` this refusal was below
                // every default filter — including the IOC console subscriber
                // (`epics_base_rs::runtime::log::install_console_subscriber`),
                // which is the same silent-refusal defect the CA driver had at
                // its thread ceiling.
                //
                // `limit` is the bound the pool actually enforced, not the
                // configured number it was built from: they are the same today
                // and reporting the enforced one keeps them the same tomorrow.
                warn!(
                    ?peer,
                    limit = capacity,
                    "blocking PVA server: refusing connection, max_connections reached"
                );
                return Ok(());
            }
            Err(cause @ AcquireError::OutOfReservation { .. }) => {
                // The other refusal the process itself makes. It is not a
                // failure to start a connection — the server is healthy and
                // said no — so it takes the refusal path and reports the pool's
                // own words, which name the switch that raises the budget.
                warn!(?peer, "blocking PVA server: refusing connection, {cause}");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        self.active.fetch_add(1, Ordering::AcqRel);
        let peer_entry = PeerEntry::new(false);
        self.peers.insert(peer, peer_entry.clone());
        let slot = ConnSlot::new(self.peers.clone(), self.active.clone(), peer);

        let source = self.source.clone();
        let config = self.config.clone();
        let invalidator = self.channel_invalidator.clone();
        let connections = self.connections.clone();
        // The connection body runs on the set's `Big` `conn` worker — the deep
        // one, running the whole protocol state machine under `block_on_sync`
        // (channel create, GET/PUT/MONITOR, introspection, and every dispatch
        // into the database including record processing). It is the structural
        // counterpart of C's per-client `camsgtask`, which rsrv creates with
        // `epicsThreadStackBig` (`rsrv/caservertask.c:109-111`).
        //
        // `run_detached`: nobody joins this connection; the worker itself
        // announces a panic through `errlog`, and the two pumps' guards inside
        // `serve_connection_blocking` join *their* jobs before this body returns.
        // `_lease` and `_slot` are declared first so they drop last — the lease
        // returns the whole set to the pool only after the body has returned and
        // the pumps have been joined.
        // Plain TCP only (§6): no x509 identity, authentication is through
        // CONNECTION_VALIDATION. Assembled here so `serve_connection_blocking`
        // takes one connection-init value rather than its two loose halves.
        let init = ConnInit {
            peer_entry,
            x509_identity: None,
            channel_invalidator: invalidator,
            tx_limit_bytes: tx_limit_bytes(&stream),
            writer_reactor: self.writer_reactor(reactor),
            liveness_clock: LivenessClock::System,
            local_port,
        };
        let conn_reactor = reactor.clone();
        conn_worker.run_detached(format!("PVA connection {peer}"), move || {
            let _lease = lease;
            let _slot = slot;
            let outcome = serve_connection_blocking(
                conn_reactor,
                PumpWorkers {
                    reader: reader_worker,
                    writer: writer_worker,
                },
                stream,
                peer,
                source,
                config,
                init,
                &connections,
            );
            if let Err(e) = outcome {
                debug!(?peer, error = %e, "blocking PVA connection ended with error");
            }
        });
        Ok(())
    }

    /// Stop accepting, stop answering SEARCHes, and end the connections
    /// already running. Idempotent.
    ///
    /// Two operations because there are two things to stop, and neither
    /// implies the other: the flag plus a self-connect returns the parked
    /// `accept`, and [`ConnRegistry::stop`] shuts every live connection's
    /// socket. Without the second, a peer that never speaks and never hangs up
    /// would hold three threads until the process exits — the semantic CA's
    /// blocking server still has, and the reason the registry exists.
    ///
    /// [`serve_udp_search`](Self::serve_udp_search) needs no third operation:
    /// nothing parks it for longer than its 200 ms read timeout, so the flag
    /// alone retires it within one tick.
    ///
    /// Returns once the wakes are issued; each connection retires on its own
    /// pool worker. Nobody joins those connections, so a caller that must
    /// observe them gone watches [`active_connections`](Self::active_connections).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.listeners.wake();
        self.connections.stop();
    }
}

impl Drop for BlockingPvaServer {
    /// Stop the connections *before* the worker pool is dropped.
    ///
    /// [`WorkerPool`]'s own `Drop` sends one `Stop` per worker and joins every
    /// thread; a worker still inside a live connection body takes its `Stop`
    /// only after that body returns. So the pool must not be dropped while a
    /// connection's socket is still open, or the join would wait on a body that
    /// never ends. [`shutdown`](Self::shutdown) shuts every live connection's
    /// socket, which unwinds its body down the existing EOF path; running it
    /// here — before the `conn_pool` field drops — is what makes the pool's
    /// join terminate. Idempotent, so a caller that already called `shutdown`
    /// pays only a redundant self-dial.
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// UDP SEARCH responder (item 7 stage D)
// ---------------------------------------------------------------------------

/// How long a `recv_from` may park before the loop re-reads the shutdown flag.
/// Not a protocol timeout: it exists only so a stopped server does not keep a
/// thread until the next datagram happens to arrive. Same value and same role
/// as CA's blocking responder (`epics-ca-rs` `server/blocking.rs`).
const UDP_STOP_TICK: Duration = Duration::from_millis(200);

/// 64 KB — the IPv4 maximum datagram, so a recv never truncates.
const UDP_RECV_BUF: usize = 64 * 1024;

/// Bind a blocking UDP SEARCH socket.
///
/// On unix, raw `libc` for the socket options rather than `socket2`, because
/// `socket2` is one of the three crates that do not cross to RTEMS (`mod.rs`)
/// — the same reason CA's blocking driver hand-rolls its bind. The Windows
/// arm uses `socket2`.
///
/// `SO_REUSEADDR` + `SO_REUSEPORT` are set on every port, 0 included, as pvxs
/// does for every `UDPCollector` socket
/// (`epicsSocketEnableAddressUseForDatagramFanout`, `udp_collector.cpp:136`;
/// see `:140-151` for why the bind is wildcard rather than to the multicast
/// address) — after the bind on 0 on unix, before it on Windows
/// ([`epics_base_rs::net::bind_fanout`]). A pvxs client told an
/// ephemeral port as its broadcast port binds its beacon receiver there with
/// fanout, which a bare socket refuses. The cost pvxs accepts too: on a
/// configured number a collision does *not* fail, so there is no error to
/// detect afterwards.
///
/// Nor is there a message to read. CA announces the same situation — a
/// `cas WARNING: ... two or more servers share the same UDP port` from
/// `rsrv` — but PVA has no equivalent on either side of the wire, so a
/// silently shared search port is invisible to the process that shared it.
/// The only positive evidence of exclusive ownership is a client-side
/// listing (`pvxlist`) that sees exactly the expected server.
///
/// A wildcard socket also joins `config`'s beacon groups and reads the
/// interface map here, as pvxs's `UDPCollector` does in the `Server`
/// constructor, so a group SEARCH sent before the responder thread runs is
/// queued, and a failed interface read fails the bind.
pub fn bind_udp_search(addr: SocketAddrV4, config: &PvaServerConfig) -> io::Result<SearchSocket> {
    let socket = bind_udp_search_socket(addr)?;
    socket.set_read_timeout(Some(UDP_STOP_TICK))?;
    // pvxs's `UDPCollector` is one wildcard socket that is also the ORIGIN_TAG
    // channel (`udp_collector.cpp:124-151`). A second wildcard socket on this
    // port would share its reuse group and take about half the unicast SEARCHes,
    // so the search socket itself joins the group. A socket bound to one
    // address cannot receive the group's datagrams, so it does not.
    #[cfg(not(epics_embedded_target))]
    let role = if addr.ip().is_unspecified() {
        epics_base_rs::net::enable_loopback_mcast(socket2::SockRef::from(&socket));
        #[cfg(any(unix, windows))]
        epics_base_rs::net::udp_rx::enable(&socket, false)?;
        Role::Collector(Routing {
            listeners: search_listeners(&socket, config)?,
            local_v4: local_v4_addrs(),
        })
    } else {
        Role::Bound
    };
    #[cfg(epics_embedded_target)]
    let _ = config;
    Ok(SearchSocket {
        socket,
        #[cfg(not(epics_embedded_target))]
        role,
    })
}

/// pvxs's v6 collector on `port`: `[::]`, dual-stack because pvxs never sets
/// `IPV6_V6ONLY` (`udp_collector.cpp:124-151`), with the fanout flags and
/// `IPV6_RECVPKTINFO` pvxs sets on every collector socket, joined to the v6
/// groups of `interfaces`. Not the ORIGIN_TAG channel, which is v4, so it
/// answers what its listeners take and forwards nothing.
pub(crate) fn bind_udp_search_v6(
    port: u16,
    interfaces: &super::search_engine::ServerInterfaces,
    listen: super::search_engine::V6Listen,
) -> io::Result<SearchSocket> {
    #[cfg(not(epics_embedded_target))]
    {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(false)?;
        let any = SocketAddr::new(std::net::Ipv6Addr::UNSPECIFIED.into(), port);
        epics_base_rs::net::PortUse::Shared.bind(
            &socket,
            any,
            epics_base_rs::net::PortOrigin::Configured,
        )?;
        for &(group, index) in &interfaces.groups_v6 {
            if let Err(e) = socket.join_multicast_v6(&group, index) {
                debug!("join {group}%{index} failed: {e}");
            }
        }
        let socket = UdpSocket::from(socket);
        #[cfg(any(unix, windows))]
        epics_base_rs::net::udp_rx::enable(&socket, true)?;
        socket.set_read_timeout(Some(UDP_STOP_TICK))?;
        Ok(SearchSocket {
            socket,
            role: Role::V6(listen),
        })
    }
    #[cfg(epics_embedded_target)]
    {
        let _ = (port, interfaces, listen);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "an embedded build has no v6 UDP",
        ))
    }
}

/// A socket [`bind_udp_search`] or the v6 collector bind bound, with the
/// listeners that decide which datagrams it answers. Never routed on an
/// embedded target, whose build has no `socket2`.
pub struct SearchSocket {
    socket: UdpSocket,
    #[cfg(not(epics_embedded_target))]
    role: Role,
}

/// What a [`SearchSocket`] is to pvxs.
#[cfg(not(epics_embedded_target))]
enum Role {
    /// Bound to one address: answers what arrives.
    Bound,
    /// The v4 collector, also the ORIGIN_TAG channel: joined to
    /// `224.0.0.128` on loopback, so a unicast SEARCH is forwarded to the
    /// group and every local PVA server answers it, as in the tokio backend.
    Collector(Routing),
    /// The v6 collector.
    V6(super::search_engine::V6Listen),
}

/// The collector's listeners, and the origin-locality set of a tagged
/// datagram (pvxs `ifinfo.byAddr`).
#[cfg(not(epics_embedded_target))]
struct Routing {
    listeners: SearchListeners,
    local_v4: std::collections::HashSet<Ipv4Addr>,
}

impl SearchSocket {
    /// Whether a datagram's destination is read off the socket.
    fn pktinfo(&self) -> bool {
        #[cfg(not(epics_embedded_target))]
        return !matches!(self.role, Role::Bound);
        #[cfg(epics_embedded_target)]
        return false;
    }

    /// The bound address, the port to read back when 0 was asked for.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// `SO_SNDBUF × TCP_TX_LIMIT_MULT` for an accepted socket — the byte cap
/// pvxs applies to a connection's queued TX (`tcp_tx_limit`,
/// `serverconn.cpp:20,61`). Raw `libc` rather than `socket2` for the same
/// RTEMS reason as [`bind_udp_search`]; on a failed read the connection is
/// served under [`TX_LIMIT_FALLBACK`] instead of refused as pvxs does.
#[cfg(unix)]
pub(super) fn tx_limit_bytes(stream: &TcpStream) -> usize {
    use std::os::fd::AsRawFd;

    use super::tcp::TCP_TX_LIMIT_MULT;
    let mut val: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the fd is a valid open socket borrowed from `stream`; `val`
    // and `len` outlive the call and `len` matches `val`'s size.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &mut val as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && val > 0 {
        (val as usize).saturating_mul(TCP_TX_LIMIT_MULT)
    } else {
        TX_LIMIT_FALLBACK
    }
}

#[cfg(not(unix))]
pub(super) fn tx_limit_bytes(_stream: &TcpStream) -> usize {
    TX_LIMIT_FALLBACK
}

#[cfg(unix)]
fn set_reuse_opt(fd: std::os::fd::RawFd, opt: libc::c_int) -> io::Result<()> {
    let one: libc::c_int = 1;
    // SAFETY: `fd` is a valid open socket; `one` outlives the call; the size
    // matches a `c_int` option value.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &one as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn bind_udp_search_socket(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;
    // SAFETY: `socket()` returns a fresh owned fd or -1.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Take ownership immediately so every early return closes the fd via Drop.
    // SAFETY: `fd` is a valid, exclusively-owned socket fd just returned above.
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    // The wildcard socket is the ORIGIN_TAG channel: it takes only the group it
    // joins, from the first datagram on.
    #[cfg(not(epics_embedded_target))]
    if addr.ip().is_unspecified() {
        epics_base_rs::net::deny_unjoined_multicast(socket2::SockRef::from(&socket));
    }
    // Build sockaddr_in via zeroed to avoid touching platform-specific fields
    // (`sin_len`/`sin_zero`); s_addr and sin_port are network byte order.
    // SAFETY: `sockaddr_in` is a plain-old-data C struct; all-zero is a valid
    // initial value.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = addr.port().to_be();
    sin.sin_addr = libc::in_addr {
        s_addr: u32::from(*addr.ip()).to_be(),
    };
    bind_fanout(
        addr.port(),
        PortOrigin::Configured,
        || {
            set_reuse_opt(fd, libc::SO_REUSEADDR)?;
            set_reuse_opt(fd, libc::SO_REUSEPORT)
        },
        || {
            // SAFETY: `sin` is a fully-initialized sockaddr_in; the length is exact.
            let rc = unsafe {
                libc::bind(
                    fd,
                    &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        },
    )?;
    Ok(socket)
}

#[cfg(not(unix))]
fn bind_udp_search_socket(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    // The same fanout as the Unix arm: on WIN32
    // `epicsSocketEnableAddressUseForDatagramFanout` is `SO_REUSEADDR`
    // (`osdSockAddrReuse.cpp`), and a plain bind refuses a second server.
    use epics_base_rs::net::{PortOrigin, PortUse};
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    PortUse::Shared.bind(&socket, addr.into(), PortOrigin::Configured)?;
    Ok(socket.into())
}

/// Receive one datagram, with the `(destination, interface index)` of its
/// arrival (`udp_rx`) — what pvxs reads to tell a forward from a
/// direct datagram (`origin = Forwarded` at `udp_collector.cpp:278`; any other
/// `CMD_ORIGIN_TAG` is ignored, `:522`) and which listener takes it.
///
/// Only a collector asked for it ([`bind_udp_search`]); a datagram with none
/// is read as the group's, so a tag sent to the unicast port is honoured
/// there.
fn recv_search_datagram(
    socket: &UdpSocket,
    buf: &mut [u8],
    pktinfo: bool,
) -> io::Result<(usize, SocketAddr, Option<(IpAddr, u32)>)> {
    #[cfg(all(not(epics_embedded_target), any(unix, windows)))]
    if pktinfo {
        let d = epics_base_rs::net::udp_rx::recv(socket, buf)?;
        return Ok((d.len, d.src, d.dest.map(|dest| (dest, d.ifindex))));
    }
    let _ = pktinfo;
    let (len, src) = socket.recv_from(buf)?;
    Ok((len, src, None))
}

/// Send a SEARCH reply from `socket`, pinned as [`reply_pin`] says.
fn send_search_reply(
    socket: &UdpSocket,
    bytes: &[u8],
    dest: SocketAddr,
    iface_ip: Option<Ipv4Addr>,
) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    if let Some((dest, src)) = super::search_engine::reply_pin(dest, iface_ip) {
        return super::search_engine::send_from_v4(socket, bytes, dest, src);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = iface_ip;
    socket.send_to(bytes, dest)
}

/// Answer UDP SEARCHes on one socket until `shutdown` is set.
///
/// The decode is [`process_search_datagram`] — the same function the hosted
/// responder drives, moved onto neutral ground by stage D part 1 — so replies
/// are byte-identical between the two drivers, including the
/// `MustReply`-with-no-matches reply that lets `pvlist` see this server at all.
/// This loop owns only the socket: recv, filter, decode, send.
///
/// Two arguments the hosted responder computes per NIC are fixed here, because
/// there is one wildcard socket and not a bundle:
///
/// * `reply_iface_ip` — `UNSPECIFIED` for what arrives on the socket, so the
///   decode resolves `iface_hint` to `None` and the OS routes. For a datagram
///   peeled out of an ORIGIN_TAG prefix it is the tag's address, and the reply
///   leaves with that source address ([`send_search_reply`]).
/// * `origin` — `Direct` for what arrives on the socket, `FromOriginTag` for a
///   datagram peeled out of an ORIGIN_TAG prefix. A [`SearchSocket`] is joined
///   to the group, so it receives the tagged copies of its own forwards.
pub(super) fn handle_udp_search_blocking(
    socket: SearchSocket,
    source: &DynSource,
    config: &PvaServerConfig,
    tcp_port: u16,
    shutdown: &AtomicBool,
) -> PvaResult<()> {
    let want_pktinfo = socket.pktinfo();
    #[cfg(not(epics_embedded_target))]
    let SearchSocket { socket, role } = socket;
    #[cfg(epics_embedded_target)]
    let SearchSocket { socket } = socket;
    let _ = enter_ioc_thread(PVA_UDP_PRIORITY);
    let mut buf = vec![0u8; UDP_RECV_BUF];

    while !shutdown.load(Ordering::Acquire) {
        let (len, src, pktinfo) = match recv_search_datagram(&socket, &mut buf, want_pktinfo) {
            Ok(v) => v,
            // The stop tick fired; nothing arrived. Re-check the flag.
            Err(ref e) if is_socket_timeout(e.kind()) => continue,
            // Every other recv error is logged and skipped, with no
            // classification — byte-for-byte the hosted responder's rule
            // (`udp.rs`, `Err(e) => { debug!("udp recv error: {e}"); continue; }`).
            // A datagram socket's errors describe a *peer*, not us: an earlier
            // reply drawing an ICMP port-unreachable surfaces here as
            // ECONNREFUSED long after that peer is gone. Ending the loop on one
            // would let any client kill this server's discovery by closing its
            // port at the right moment, and would make the two drivers disagree
            // about what a recv error means.
            Err(e) => {
                debug!(error = %e, "blocking UDP responder: recv error");
                continue;
            }
        };
        if !filter_inbound(src, &config.ignore_addrs) {
            continue;
        }

        #[cfg(not(epics_embedded_target))]
        let routed = match &role {
            Role::Collector(r) => {
                let v4 = pktinfo.and_then(|(dest, index)| match dest {
                    IpAddr::V4(dest) => Some((dest, index)),
                    IpAddr::V6(_) => None,
                });
                r.listeners.route(&buf[..len], &r.local_v4, v4)
            }
            Role::V6(listen) if !listen.takes(src, pktinfo.map(|(dest, _)| dest)) => None,
            Role::Bound | Role::V6(_) => {
                Some((&buf[..len], Ipv4Addr::UNSPECIFIED, Origin::Direct, false))
            }
        };
        // An embedded build has no ORIGIN_TAG, so nothing asks where it arrived.
        #[cfg(epics_embedded_target)]
        let _ = pktinfo;
        #[cfg(epics_embedded_target)]
        let routed = Some((&buf[..len], Ipv4Addr::UNSPECIFIED, Origin::Direct, false));
        let Some((frame, reply_iface_ip, origin, forward)) = routed else {
            continue;
        };

        // The decode's only suspension point is the source name lookup. On
        // RTEMS `block_on_sync` parks this thread; on a host test it does the
        // same, since this loop owns its thread outright.
        let outputs = match block_on_sync(process_search_datagram(
            source,
            forward,
            config.udp_port,
            frame,
            src,
            reply_iface_ip,
            origin,
            tcp_port,
            config.guid,
            "tcp",
        )) {
            Ok(v) => v,
            Err(_) => {
                return Err(PvaError::Protocol(
                    "blocking UDP responder: decode future not blockable in this thread context"
                        .into(),
                ));
            }
        };

        for out in outputs {
            match out {
                SearchOutput::Reply {
                    dest,
                    bytes,
                    iface_hint,
                } => {
                    if let Err(e) = send_search_reply(&socket, &bytes, dest, iface_hint) {
                        debug!(%dest, error = %e, "blocking UDP SEARCH reply send failed");
                    }
                }
                // Produced only for the collector, and then the socket is a
                // member of the group, so the send loops back into this
                // responder and every local peer.
                SearchOutput::OriginTagForward { dest, bytes } => {
                    if let Err(e) = socket.send_to(&bytes, dest) {
                        debug!(%dest, error = %e, "blocking UDP ORIGIN_TAG forward failed");
                    }
                }
            }
        }
    }
    Ok(())
}

/// The collector's listeners for `config`, with its beacon groups joined on
/// `socket` — the tokio backend's `bind_udp` plan on one wildcard socket.
#[cfg(not(epics_embedded_target))]
fn search_listeners(socket: &UdpSocket, config: &PvaServerConfig) -> io::Result<SearchListeners> {
    let interfaces = ServerInterfaces::resolve(
        &config.interfaces,
        &config.beacon_destinations,
        config.auto_beacon,
    )?;
    let sock = socket2::SockRef::from(socket);
    for &(group, iface) in &interfaces.groups {
        if let Err(e) = sock.join_multicast_v4(&group, &iface) {
            debug!("join {group}@{iface} failed: {e}");
        }
    }
    Ok(SearchListeners::new(
        &interfaces,
        &epics_base_rs::net::IfaceMap::new()?.all(),
    ))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::codec::PvaCodec;
    use crate::proto::{
        ByteOrder, Command, PvaHeader, ReadExt, WriteExt, encode_string_into, ip_from_bytes,
        ip_to_bytes,
    };
    use crate::pvdata::encode::encode_type_desc;
    use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use crate::server_native::search::build_search_response_proto;
    use crate::server_native::{SharedPV, SharedSource};
    use epics_base_rs::runtime::blocking_io::circuit_roster;
    use epics_base_rs::runtime::task::spawn_dedicated_thread;
    use epics_base_rs::runtime::worker_pool::SetLease;
    use source_guard::{Comments, production};
    use std::io::{Cursor, Read, Write};
    use std::net::IpAddr;
    use std::sync::LazyLock;
    use std::task::{Context, Poll, Waker};
    use std::thread;
    use std::time::Instant;

    /// A process-lifetime pool the pump and connection tests borrow their
    /// workers from, so no test creates a raw thread. Its roster is the two pump
    /// roles `[reader, writer]` at `PVA_SERVER_PRIORITY`; the connection-body
    /// role is not here because these tests run that body on a tokio task
    /// (hosted) or drive the pump functions directly. Capacity is generous — a
    /// test borrows a few sets at once and returns them.
    static TEST_POOL: LazyLock<WorkerPool<2>> = LazyLock::new(|| {
        WorkerPool::new(
            "test-pvas",
            circuit_roster(PVA_SERVER_PRIORITY, PVA_SERVER_PRIORITY),
            64,
        )
    });

    /// Borrow one reader+writer set. The returned [`SetLease`] must outlive the
    /// jobs the two workers run — hold it until both pump guards have dropped.
    fn lease_pumps() -> (SetLease, Worker, Worker) {
        let (lease, [reader, writer]) = TEST_POOL.acquire().expect("test pool acquire");
        (lease, reader, writer)
    }

    /// Source guard for F2: both PVA accept loops back off through
    /// [`AcceptBackoff`], and the count is pinned so a *new* accept loop
    /// cannot be added without one.
    ///
    /// The blocking loop re-looped instantly on failure — a hot spin at 100%
    /// CPU while the log floods, because a failed accept leaves the connection
    /// queued. The hosted loop in `accept.rs` did have a sleep, but a flat
    /// 50 ms with no way out: a listener that could never accept again spun at
    /// 20 Hz for the life of the process.
    ///
    /// Whole-file scope and `concat!`-split needles: this guard lives in one
    /// of the two files it reads, so an unsplit needle would match its own
    /// source and pass vacuously.
    #[test]
    fn every_pva_accept_loop_backs_off_through_the_primitive() {
        for (file, src) in [
            ("server_native/blocking.rs", include_str!("blocking.rs")),
            ("server_native/accept.rs", include_str!("accept.rs")),
            ("server_native/reactor.rs", include_str!("reactor.rs")),
        ] {
            let loops = src.matches(concat!("listener.inco", "ming()")).count()
                + src
                    .matches(concat!("res = listener.acc", "ept() => res,"))
                    .count();
            let backoffs = src.matches(concat!("AcceptBack", "off::new()")).count();
            assert_eq!(
                loops, 1,
                "{file}: expected exactly one production accept loop; if one was \
                 added or removed, update this guard and give it a backoff"
            );
            assert_eq!(
                backoffs, loops,
                "{file}: an accept loop has no AcceptBackoff. A failed accept leaves \
                 the connection queued, so retrying with no delay spins at 100% CPU \
                 exactly when fds or memory have run out"
            );
            assert!(
                src.contains(concat!("backoff.fai", "led()")),
                "{file}: the accept-failure arm must ask the backoff what to do"
            );
            assert!(
                src.contains(concat!("backoff.acce", "pted()")),
                "{file}: a successful accept must reset the give-up budget, or a \
                 busy server under sustained EMFILE eventually stops accepting"
            );
            assert!(
                !src.contains(concat!("sleep(Duration::from_mil", "lis(50)).await")),
                "{file}: the hardcoded accept-error sleep is back; it has no \
                 give-up path, so a dead listener spins at 20 Hz forever"
            );
            assert_delay_is_actually_waited(file, src);
        }
    }

    /// The wait must actually happen.
    ///
    /// `failed()` is `#[must_use]`, but binding its result to `_` silences
    /// that and reinstates the hot spin while every other assertion above
    /// still passes. Verified by mutation: replacing the wait with `{}` was
    /// invisible until this check existed. Every call is therefore checked for
    /// an enclosing sleep, and — since the give-up path was removed for C
    /// parity — for the absence of any exit variant.
    ///
    /// Needles are `concat!`-split for the reason given above: this guard
    /// lives in one of the files it reads.
    fn assert_delay_is_actually_waited(file: &str, src: &str) {
        let call = concat!("backoff.fai", "led()");
        let mut calls = 0;
        for (i, _) in src.match_indices(call) {
            calls += 1;
            let before = &src[i.saturating_sub(40)..i];
            assert!(
                before.contains(concat!("sle", "ep(")),
                "{file}: an accept failure is not waited on — the accept loop is \
                 spinning again. Call context: {before:?}"
            );
        }
        assert!(calls > 0, "{file}: no accept-backoff call at all");
        assert!(
            !src.contains(concat!("AcceptRe", "try")),
            "{file}: the give-up path is back. C rsrv never leaves its accept \
             loop (caservertask.c:82-121); an IOC that stops accepting for the \
             life of the process is the failure that removal closed."
        );
    }

    /// The RTEMS constraint this whole driver exists to satisfy: it must not
    /// reach for tokio's async net/timer/spawn machinery, none of which builds
    /// for `armv7-rtems-eabihf`, and it must not suspend a future directly —
    /// every await goes through `block_on_sync`. `tokio::sync` and
    /// `tokio::io`'s traits ARE allowed and are what the two adapters are
    /// built from. Modelled on the CA blocking driver's guard
    /// (`epics-ca-rs` `server/blocking.rs`), and scoped to the production
    /// slice so the tests below may use the async machinery freely.
    #[test]
    fn blocking_driver_has_no_async_runtime_symbols() {
        let prod = production(include_str!("blocking.rs"), Comments::Strip);
        // Fail closed: if the driver is no longer in the slice, the slice is
        // wrong and every assertion below would pass vacuously.
        assert!(
            prod.contains("fn serve_connection_blocking"),
            "production slice no longer covers the driver"
        );
        // Assembled with `concat!` so the forbidden literals never appear
        // contiguously in this file — otherwise this test body would match
        // itself under `include_str!`.
        let forbidden = [
            concat!("tokio", "::net"),
            concat!("tokio", "::time"),
            concat!("tokio", "::", "spawn"),
            concat!("block", "_in_place"),
            concat!(".", "await"),
        ];
        for token in forbidden {
            assert_eq!(
                prod.matches(token).count(),
                0,
                "the blocking PVA driver must not reference `{token}`: it has no async \
                 net/timer/spawn on RTEMS, and every await goes through `block_on_sync`"
            );
        }
    }

    // ── adapter and pump tests live with the primitive ──────────────────
    //
    // Cancel-safety of `ChannelReader`, the `ChannelWriter` weak-sender rule,
    // the send-deadline loop and both pump guards are now tested in
    // `epics_base_rs::runtime::blocking_io`, beside the code they describe.
    // What stays here is what is server-side: the registry, the accept loop,
    // and this driver's own assembly and teardown.

    // ── end-to-end over a real loopback socket ──────────────────────────

    fn scalar_intro() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))].into(),
        }
    }

    fn scalar_value(v: f64) -> PvField {
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(v)))],
        })
    }

    pub(crate) fn test_source() -> DynSource {
        let pv = SharedPV::new();
        pv.open(scalar_intro(), scalar_value(1.5)).expect("open pv");
        let shared = SharedSource::new();
        shared.add("dut", pv);
        Arc::new(shared)
    }

    /// The client end of a connection, with just enough framing to drive the
    /// server. Shared by the per-connection tests and the accept-loop tests
    /// below, so neither grows its own frame reader.
    pub(crate) struct TestClient(pub(crate) TcpStream);

    impl TestClient {
        pub(crate) fn connect(addr: SocketAddr) -> TestClient {
            let sock = TcpStream::connect(addr).expect("connect");
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("client read timeout");
            TestClient(sock)
        }

        pub(crate) fn send(&mut self, bytes: &[u8]) {
            self.0.write_all(bytes).expect("client write");
        }

        /// Read exactly one PVA frame (header + declared body).
        fn read_frame(&mut self) -> (PvaHeader, Vec<u8>) {
            let mut head = [0u8; PvaHeader::SIZE];
            self.0.read_exact(&mut head).expect("frame header");
            let header =
                PvaHeader::decode(&mut Cursor::new(&head[..])).expect("decode frame header");
            let mut body = vec![0u8; header.payload_length as usize];
            if !body.is_empty() {
                self.0.read_exact(&mut body).expect("frame body");
            }
            (header, body)
        }

        /// Read frames until one carries `command` as an application message.
        pub(crate) fn read_until(&mut self, command: Command) -> Vec<u8> {
            for _ in 0..32 {
                let (header, body) = self.read_frame();
                if !header.flags.is_control() && header.command == command.code() {
                    return body;
                }
            }
            panic!("no {command:?} frame arrived within 32 frames");
        }

        /// Read frames until one carries `command` as a control message.
        fn read_until_control(&mut self, command: crate::proto::ControlCommand) {
            for _ in 0..32 {
                let (header, _) = self.read_frame();
                if header.flags.is_control() && header.command == command.code() {
                    return;
                }
            }
            panic!("no {command:?} control frame arrived within 32 frames");
        }

        pub(crate) fn close(&self) {
            let _ = self.0.shutdown(Shutdown::Both);
        }
    }

    /// A connection served by the blocking driver, plus the client's end of
    /// the socket. Dropping the client end ends the connection.
    struct Harness {
        client: TestClient,
        conn: Option<tokio::task::JoinHandle<PvaResult<()>>>,
    }

    impl Harness {
        /// Bind loopback on an ephemeral port (never 5075), connect, and hand
        /// the accepted socket to the blocking driver.
        ///
        /// The driver runs inside a spawned task rather than a bare thread
        /// because on a hosted build the connection future needs tokio's
        /// timer and spawner underneath it (module docs); `block_on_sync`
        /// then takes its worker-handoff arm, which is the arm a hosted
        /// caller is supposed to take.
        ///
        /// `registry` stands in for the accept loop stage C brings: these
        /// tests drive it as that loop does — one registry, N connections,
        /// `stop`.
        fn start(config: PvaServerConfig, registry: Arc<ConnRegistry>) -> Harness {
            Self::start_with_clock(
                config,
                registry,
                LivenessClock::System,
                test_source(),
                PeerEntry::new(false),
            )
        }

        fn start_with_clock(
            config: PvaServerConfig,
            registry: Arc<ConnRegistry>,
            liveness_clock: LivenessClock,
            source: DynSource,
            peer_entry: Arc<PeerEntry>,
        ) -> Harness {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let client = TestClient::connect(addr);
            let (server_sock, peer) = listener.accept().expect("accept");
            let (lease, reader_worker, writer_worker) = lease_pumps();
            let conn = tokio::spawn(async move {
                // Hold the set's lease until the connection — and the two pump
                // jobs its guards join before `serve_connection_blocking`
                // returns — is over; dropping it returns the set to the pool.
                let _lease = lease;
                serve_connection_blocking(
                    crate::test_reactor(),
                    PumpWorkers {
                        reader: reader_worker,
                        writer: writer_worker,
                    },
                    server_sock,
                    peer,
                    source,
                    config,
                    ConnInit {
                        peer_entry,
                        x509_identity: None,
                        channel_invalidator: ChannelInvalidator::new(),
                        tx_limit_bytes: TX_LIMIT_FALLBACK,
                        writer_reactor: crate::test_reactor(),
                        liveness_clock,
                        local_port: 5075,
                    },
                    &registry,
                )
            });
            Harness {
                client,
                conn: Some(conn),
            }
        }

        /// Collect the connection's result under a bound, so a regression that
        /// leaves the connection parked fails this test instead of hanging the
        /// run. Every shutdown-path assertion below is "it retired", and this
        /// is what makes that assertion falsifiable.
        async fn wait_retired(mut self, within: Duration) -> PvaResult<()> {
            let conn = self.conn.take().expect("connection task present");
            match tokio::time::timeout(within, conn).await {
                Ok(joined) => joined.expect("connection task joined"),
                Err(_) => {
                    // Release the connection *before* failing. The driver is
                    // parked inside `block_in_place`, and unwinding straight
                    // into the runtime's drop would wait on that worker — the
                    // test would hang instead of failing, which is exactly
                    // what a mutation of the wake path must not be able to do.
                    self.client.close();
                    panic!("the connection must retire within {within:?}, not stay parked");
                }
            }
        }

        /// Close the client end and collect the connection's result.
        async fn finish(self) -> PvaResult<()> {
            self.client.close();
            self.wait_retired(RETIRE_BOUND).await
        }
    }

    /// How long a connection may take to notice it is over. Generous next to
    /// the microseconds a socket shutdown actually needs, and far below the
    /// ≤15 s heartbeat window a `select!`-arm-less design would have left.
    const RETIRE_BOUND: Duration = Duration::from_secs(5);

    pub(crate) fn isolated_config() -> PvaServerConfig {
        PvaServerConfig {
            wire_byte_order: ByteOrder::Little,
            ..PvaServerConfig::isolated()
        }
    }

    /// The port a TCP SEARCH for `dut`, sent over a connection to `addr`, is
    /// told to connect to.
    pub(crate) fn tcp_search_port(addr: SocketAddr) -> u16 {
        let mut client = TestClient::connect(addr);
        let codec = crate::codec::PvaCodec { big_endian: false };
        client.send(&codec.build_search(1, 7, "dut", [0; 4], 0, true));
        let body = client.read_until(Command::SearchResponse);
        client.close();
        // GUID (12), seq (4), server address (16), then the port.
        u16::from_le_bytes([body[32], body[33]])
    }

    /// Two interfaces on two ports: the first ephemeral, the second the
    /// named `other`.
    pub(crate) fn two_port_config(other: u16) -> PvaServerConfig {
        PvaServerConfig {
            interfaces: ["127.0.0.1:0".to_string(), format!("127.0.0.2:{other}")]
                .map(|a| a.parse::<SocketAddr>().unwrap().into())
                .to_vec(),
            ..isolated_config()
        }
    }

    pub(crate) fn app_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Vec<u8> {
        // Client → server: the Server direction bit stays clear.
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        let mut out = Vec::new();
        header.write_into(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    pub(crate) fn create_channel_payload(cid: u32, name: &str, order: ByteOrder) -> Vec<u8> {
        let mut payload = Vec::new();
        // `count` is a plain u16, not a Size (pvxs serverchan.cpp:269).
        payload.put_u16(1, order);
        payload.put_u32(cid, order);
        encode_string_into(name, order, &mut payload);
        payload
    }

    /// The header and the body each arrive split across several TCP segments,
    /// with a pause in between so the kernel really does deliver them as
    /// separate chunks. The chunk boundaries land inside the 8-byte header and
    /// inside the payload — the two places a byte-level seam could corrupt a
    /// frame — and the server must still dispatch one whole CREATE_CHANNEL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_header_and_partial_body_frames_reassemble() {
        let order = ByteOrder::Little;
        let mut h = Harness::start(isolated_config(), Arc::new(ConnRegistry::new()));

        let frame = app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(7, "dut", order),
        );
        // 3 bytes of header, then 5 more (completing it), then the body in
        // two pieces.
        let split = [3usize, PvaHeader::SIZE, PvaHeader::SIZE + 2, frame.len()];
        let mut from = 0;
        for to in split {
            h.client.send(&frame[from..to]);
            from = to;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let body = h.client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 7, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        let status = cur.get_u8().expect("status");
        assert_eq!(status, 0xFF, "CREATE_CHANNEL must succeed (status OK)");

        let _ = h.finish().await;
    }

    /// A segmented application message whose FIRST and LAST segments are
    /// written in separate TCP chunks. Segment reassembly lives in the
    /// operation loop's `seg_buf`, on the far side of the thread boundary —
    /// this proves handing over *bytes* rather than frames left it intact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn segmented_message_reassembles_across_a_chunk_boundary() {
        let order = ByteOrder::Little;
        let mut h = Harness::start(isolated_config(), Arc::new(ConnRegistry::new()));

        let payload = create_channel_payload(11, "dut", order);
        let cut = payload.len() / 2;
        // Segment flags live in the header's flag byte: bit 4 = FIRST,
        // bit 5 = LAST (an unsegmented frame has neither).
        let mut first =
            PvaHeader::application(false, order, Command::CreateChannel.code(), cut as u32);
        first.flags.0 |= 0x10;
        let mut last = PvaHeader::application(
            false,
            order,
            Command::CreateChannel.code(),
            (payload.len() - cut) as u32,
        );
        last.flags.0 |= 0x20;

        let mut chunk_a = Vec::new();
        first.write_into(&mut chunk_a);
        chunk_a.extend_from_slice(&payload[..cut]);
        h.client.send(&chunk_a);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut chunk_b = Vec::new();
        last.write_into(&mut chunk_b);
        chunk_b.extend_from_slice(&payload[cut..]);
        h.client.send(&chunk_b);

        let body = h.client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 11, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a segmented CREATE_CHANNEL must reassemble and succeed"
        );

        let _ = h.finish().await;
    }

    /// Reassembly is capped, and the cap is on the *accumulated* size — not
    /// on any one segment. Each segment below is comfortably under the
    /// ceiling, so `read_frame`'s per-frame check cannot be what refuses
    /// them; only the accumulator can. Without that check a peer streams
    /// SegFirst → SegMiddle … forever and `seg_buf` grows until the
    /// allocator says no.
    ///
    /// The retire assertion rests on `runtime::blocking_io`'s wait, not on
    /// the park: a local `shutdown(Shutdown::Both)` returns a connection
    /// parked in `read` behind the ~64,000 s `op_timeout` (§1.6) at once on
    /// unix (`POLLHUP`), and within the wait's `WAKE_POLL_PERIOD` on Windows,
    /// which does not return the parked `recv` itself (measured, PR #56 CI
    /// 2026-07-24: all eight of these tests timed out on their retire bounds
    /// on both Windows runners while the park carried the bound).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reassembly_is_capped_on_the_accumulated_size_not_the_segment() {
        let order = ByteOrder::Little;
        const CAP: usize = 24;
        const SEG: usize = 16;
        let config = PvaServerConfig {
            max_message_size: Some(CAP),
            ..isolated_config()
        };
        let mut h = Harness::start(config, Arc::new(ConnRegistry::new()));

        let mut first =
            PvaHeader::application(false, order, Command::CreateChannel.code(), SEG as u32);
        first.flags.0 |= 0x10; // SegFirst
        let mut middle =
            PvaHeader::application(false, order, Command::CreateChannel.code(), SEG as u32);
        middle.flags.0 |= 0x30; // SegMiddle (FIRST|LAST bits both set)

        let mut wire = Vec::new();
        first.write_into(&mut wire);
        wire.extend_from_slice(&[0u8; SEG]);
        middle.write_into(&mut wire);
        wire.extend_from_slice(&[0u8; SEG]);
        h.client.send(&wire);

        let err = h
            .wait_retired(RETIRE_BOUND)
            .await
            .expect_err("accumulating past the cap must end the connection");
        let msg = err.to_string();
        assert!(
            msg.contains("segmented PVA message exceeds max_message_size"),
            "the refusal must name the reassembly cap, got: {msg}"
        );
        // 16 + 16 = 32 against a 24-byte ceiling: neither segment alone
        // exceeded it, which is what makes this the accumulator's refusal.
        assert!(
            msg.contains("32") && msg.contains("24"),
            "the refusal must report accumulated vs cap, got: {msg}"
        );
    }

    /// The TypeCache invariant (§1.3), end to end through the blocking driver.
    ///
    /// A client may define a descriptor with `0xFD <slot> <desc>` in one frame
    /// and reference it with `0xFE <slot>` in a **different** frame on the
    /// same connection; pvxs keeps one connection-scoped `rxRegistry`
    /// (`conn.h:23`). This is the invariant that decided the seam: had the
    /// reader thread parsed frames and shipped them across a channel, the
    /// cache would have needed a protocol between two threads. Handing bytes
    /// keeps it a plain local of the one loop that dispatches in wire order —
    /// and this test is what proves the thread boundary did not disturb it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn type_cache_define_and_reference_survive_in_different_frames() {
        let order = ByteOrder::Little;
        let mut h = Harness::start(isolated_config(), Arc::new(ConnRegistry::new()));

        h.client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(1, "dut", order),
        ));
        let body = h.client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        let _cid = cur.get_u32(order).expect("cid");
        let sid = cur.get_u32(order).expect("sid");
        assert_eq!(cur.get_u8().expect("status"), 0xFF, "channel created");

        // The empty pvRequest structure: a real, cacheable descriptor whose
        // only job here is to be defined once and referenced later.
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new().into(),
        };
        const SLOT: u16 = 0x0001;
        const INIT: u8 = 0x08;

        // GET INIT #1 — pvRequest carried as `0xFD <slot> <inline desc>`.
        let mut define = Vec::new();
        define.put_u32(sid, order);
        define.put_u32(801, order);
        define.put_u8(INIT);
        define.put_u8(0xFD);
        define.put_u16(SLOT, order);
        encode_type_desc(&req_desc, order, &mut define);
        h.client.send(&app_frame(Command::Get, order, define));
        let reply1 = h.client.read_until(Command::Get);
        let mut cur = Cursor::new(&reply1[..]);
        assert_eq!(cur.get_u32(order).expect("ioid"), 801);
        let _sub = cur.get_u8().expect("subcommand");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "the defining GET INIT must succeed"
        );

        // GET INIT #2 — a bare `0xFE <slot>` reference, in its own frame,
        // resolved only by the connection-scoped cache the first frame filled.
        let mut reference = Vec::new();
        reference.put_u32(sid, order);
        reference.put_u32(802, order);
        reference.put_u8(INIT);
        reference.put_u8(0xFE);
        reference.put_u16(SLOT, order);
        h.client.send(&app_frame(Command::Get, order, reference));
        let reply2 = h.client.read_until(Command::Get);
        let mut cur = Cursor::new(&reply2[..]);
        assert_eq!(cur.get_u32(order).expect("ioid"), 802);
        let _sub = cur.get_u8().expect("subcommand");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a 0xFE reference in a later frame must resolve against the cache the \
             earlier frame filled — the invariant the byte-level seam preserves"
        );

        let _ = h.finish().await;
    }

    /// The hosted precondition, stated as a boundary rather than left to the
    /// module docs: a **current-thread** runtime cannot host this driver,
    /// because parking its only thread would stop the very tasks that must
    /// wake the connection. `block_on_sync` reports that instead of
    /// deadlocking, and the driver must surface it as an error and tear down
    /// rather than pass it off as a normal connection close.
    #[tokio::test]
    async fn a_current_thread_runtime_is_refused_rather_than_deadlocked() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let _client = TcpStream::connect(addr).expect("connect");
        let (server_sock, peer) = listener.accept().expect("accept");

        // `#[tokio::test]` without a flavor IS a current-thread runtime, so
        // this call is already on the thread the guard is about.
        let (_lease, reader_worker, writer_worker) = lease_pumps();
        let err = serve_connection_blocking(
            crate::test_reactor(),
            PumpWorkers {
                reader: reader_worker,
                writer: writer_worker,
            },
            server_sock,
            peer,
            test_source(),
            isolated_config(),
            ConnInit {
                peer_entry: PeerEntry::new(false),
                x509_identity: None,
                channel_invalidator: ChannelInvalidator::new(),
                tx_limit_bytes: TX_LIMIT_FALLBACK,
                writer_reactor: crate::test_reactor(),
                liveness_clock: LivenessClock::System,
                local_port: 5075,
            },
            &ConnRegistry::new(),
        )
        .expect_err("a current-thread runtime must be refused");
        match err {
            PvaError::Protocol(msg) => assert!(
                msg.contains("current-thread"),
                "the error must name the reason, got: {msg}"
            ),
            other => panic!("expected a Protocol error naming the runtime, got {other:?}"),
        }
    }

    // ── shutdown: the registry and the wake ─────────────────────────────
    //
    // One case per boundary of the stop transition, not one per story. The
    // registry has no in-tree production caller until item 7's RTEMS accept
    // loop, so these tests are its driver, and they drive it the way that
    // loop will: one registry, connections registering and retiring under it,
    // `stop` walking whatever is live.

    /// Wait until `registry` holds `n` connections.
    ///
    /// `Harness::start` returning does not mean the connection is registered:
    /// the driver registers on its own thread, once the spawned task is first
    /// polled. Asserting the count directly is a race, and a `stop` issued
    /// before registration would exercise the post-stop latch instead of the
    /// case under test.
    async fn await_registered(registry: &ConnRegistry, n: usize) {
        for _ in 0..500 {
            if registry.live_connections() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "registry never reached {n} live connections (stuck at {})",
            registry.live_connections()
        );
    }

    /// A pair of connected loopback sockets, with the client end returned so
    /// the caller can keep it open — a dropped client would retire the
    /// connection on its own and make every assertion below vacuous.
    fn socket_pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, peer) = listener.accept().expect("accept");
        (client, server, peer)
    }

    /// One non-blocking `poll_write` on a writer adapter, from a plain `#[test]`
    /// with no runtime.
    ///
    /// The tests below need to ask "has the writer pump ended?" without
    /// dropping the pump guard — dropping it would shut the socket and so
    /// answer its own question. A single poll against a no-op waker gives
    /// `Ready(Err)` exactly when the pump has dropped the frame receiver.
    fn poll_write_once(
        adapter: &mut (impl tokio::io::AsyncWrite + Unpin),
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut cx = Context::from_waker(Waker::noop());
        std::pin::Pin::new(adapter).poll_write(&mut cx, buf)
    }

    /// Boundary: nothing registered. `stop` must be a no-op rather than
    /// something with an empty-map special case, and must not invent state.
    #[test]
    fn stop_with_no_connections_is_harmless() {
        let registry = ConnRegistry::new();
        assert_eq!(registry.live_connections(), 0);
        registry.stop();
        assert_eq!(
            registry.live_connections(),
            0,
            "stopping an empty registry must not leave an entry behind"
        );
    }

    /// Boundary: the reader is parked in `read` — the common state, and the
    /// one nothing else can end. `op_timeout` is ~64,000 s by default (§1.6),
    /// so if the socket shutdown does not return that `read`, nothing does.
    ///
    /// Mutation-checked: make `ConnWake::wake` a no-op and this test fails on
    /// its retire bound instead of hanging, because `wait_retired` is bounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_wakes_a_connection_parked_in_its_read() {
        let order = ByteOrder::Little;
        let registry = Arc::new(ConnRegistry::new());
        let mut h = Harness::start(isolated_config(), registry.clone());

        // One complete exchange first, so the connection is provably past
        // setup and parked waiting for a frame that will never come.
        h.client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(3, "dut", order),
        ));
        let _ = h.client.read_until(Command::CreateChannel);
        await_registered(&registry, 1).await;

        registry.stop();
        let _ = h.wait_retired(RETIRE_BOUND).await;
        assert_eq!(
            registry.live_connections(),
            0,
            "a retired connection must have removed its own entry"
        );
    }

    /// Boundary: the writer is parked inside `write_frame_deadline` with a
    /// deadline far longer than the test, on a peer that is not reading. The
    /// deadline would eventually free it — that is §3.3 — but `stop` must not
    /// have to wait for it.
    ///
    /// Mutation-checked: `ConnWake::wake` as a no-op leaves the writer parked
    /// until its 30 s deadline, well past the 5 s bound here.
    ///
    /// Driven through the real pump primitive rather than a hand-built thread,
    /// so what is under test is the registry's reach into a connection assembled
    /// the way `serve_connection_blocking` assembles one.
    #[test]
    fn stop_wakes_a_writer_parked_in_the_deadline_loop() {
        // Kept alive and deliberately never read from: this is the stuck-peer
        // case, so the socket buffers fill and the writer blocks.
        let (_client, server, peer) = socket_pair();

        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        let write_sock = server.clone();
        let _registration = registry.register(server.clone());
        // Far longer than this test may take, so only the wake can explain a
        // prompt exit.
        const DEADLINE: Duration = Duration::from_secs(30);

        let label = format!("PVA connection {peer}");
        // `_lease` is declared before `_guard`, so it drops last — after the
        // guard has joined the pump job and returned the worker.
        let (_lease, _reader_worker, writer_worker) = lease_pumps();
        let (mut adapter, _guard) = spawn_writer_pump(
            writer_worker,
            write_sock,
            &label,
            DEADLINE,
            FRAME_QUEUE_DEPTH,
        );

        // Bigger than any socket buffer, so the writer really is parked mid
        // frame rather than done before `stop` runs. The depth-1 queue takes
        // the whole buffer in one `poll_write`.
        assert!(
            matches!(
                poll_write_once(&mut adapter, &vec![0xA5u8; 8 * 1024 * 1024]),
                Poll::Ready(Ok(_))
            ),
            "the first frame must fit the empty queue without parking"
        );
        thread::sleep(Duration::from_millis(300));

        // The pump's exit is observed through the adapter, NOT by dropping the
        // guard: the guard shuts the very same descriptor, so joining it here
        // would release the writer whether or not `stop` did anything, and the
        // assertion would pass vacuously. Once the pump ends it drops the frame
        // receiver, and the adapter's weak sender reports the channel closed.
        let started = Instant::now();
        registry.stop();
        let mut ended = false;
        while started.elapsed() < RETIRE_BOUND {
            if matches!(poll_write_once(&mut adapter, b"probe"), Poll::Ready(Err(_))) {
                ended = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let elapsed = started.elapsed();
        assert!(
            ended && elapsed < DEADLINE / 2,
            "the writer must be woken by the shutdown, not released by its own \
             {DEADLINE:?} deadline; ended={ended} after {elapsed:?}"
        );
    }

    /// Boundary: an operation is registered and not destroyed when `stop`
    /// arrives. The connection must still retire — teardown drains `channels`,
    /// which drops each `OpState` and fires its guards — rather than the
    /// in-flight op holding the loop open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_retires_a_connection_with_an_operation_in_flight() {
        let order = ByteOrder::Little;
        let registry = Arc::new(ConnRegistry::new());
        let mut h = Harness::start(isolated_config(), registry.clone());

        h.client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(5, "dut", order),
        ));
        let body = h.client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        let _cid = cur.get_u32(order).expect("cid");
        let sid = cur.get_u32(order).expect("sid");
        assert_eq!(cur.get_u8().expect("status"), 0xFF, "channel created");

        // A GET INIT that is answered but never destroyed: the ioid stays
        // registered on the channel, so teardown has real state to unwind.
        let mut init = Vec::new();
        init.put_u32(sid, order);
        init.put_u32(900, order);
        init.put_u8(0x08);
        init.put_u8(0xFD);
        init.put_u16(0x0001, order);
        encode_type_desc(
            &FieldDesc::Structure {
                struct_id: String::new(),
                fields: Vec::new().into(),
            },
            order,
            &mut init,
        );
        h.client.send(&app_frame(Command::Get, order, init));
        let reply = h.client.read_until(Command::Get);
        let mut cur = Cursor::new(&reply[..]);
        assert_eq!(cur.get_u32(order).expect("ioid"), 900);
        let _sub = cur.get_u8().expect("subcommand");
        assert_eq!(cur.get_u8().expect("status"), 0xFF, "GET INIT succeeded");

        await_registered(&registry, 1).await;
        registry.stop();
        let _ = h.wait_retired(RETIRE_BOUND).await;
        assert_eq!(registry.live_connections(), 0);
    }

    /// Boundary: `stop` called more than once, including after everything has
    /// already gone. It is a latch, not a toggle, and the second call must not
    /// find a half-removed entry or panic on one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_is_idempotent() {
        let registry = Arc::new(ConnRegistry::new());
        let h = Harness::start(isolated_config(), registry.clone());
        await_registered(&registry, 1).await;

        registry.stop();
        registry.stop();
        let _ = h.wait_retired(RETIRE_BOUND).await;
        assert_eq!(registry.live_connections(), 0);
        // And once more with nothing left to walk.
        registry.stop();
        assert_eq!(registry.live_connections(), 0);
    }

    /// Boundary: the connection ends by itself, then `stop` runs. Two things
    /// must hold — the entry is gone (no leak), and walking a registry that
    /// raced a retiring connection cannot touch a recycled fd, which is why
    /// the registry owns an `Arc` of the socket rather than a bare fd.
    ///
    /// Mutation-checked: make `ConnRegistration::drop` skip
    /// `ConnRegistry::deregister` and the leak assertion fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_that_ended_on_its_own_leaves_no_entry() {
        let registry = Arc::new(ConnRegistry::new());
        let h = Harness::start(isolated_config(), registry.clone());
        await_registered(&registry, 1).await;

        // The client hangs up; nothing on the server side was told to stop.
        let _ = h.finish().await;
        assert_eq!(
            registry.live_connections(),
            0,
            "a connection that ended on its own must not leave an entry for \
             `stop` to walk"
        );
        // A later stop has nothing to do and must say so quietly.
        registry.stop();
    }

    /// Boundary: registration *after* `stop`. An accept already in flight when
    /// the server stopped must not produce a connection that outlives it, and
    /// it retires down the same path as every other connection rather than a
    /// second one — the latch is checked as part of registering, not by a
    /// separate pre-flight the caller could skip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_registered_after_stop_is_shut_down_at_once() {
        let registry = Arc::new(ConnRegistry::new());
        registry.stop();

        // The client stays connected and sends nothing: without the latch this
        // connection would sit in `read` until `op_timeout`, i.e. ~64,000 s.
        let h = Harness::start(isolated_config(), registry.clone());
        let _ = h.wait_retired(RETIRE_BOUND).await;
        assert_eq!(registry.live_connections(), 0);
    }

    /// The inactivity deadline as the peer experiences it: a client that
    /// connects and then says nothing is dropped when `idle_timeout`
    /// expires, whatever the read loop happens to be parked on.
    ///
    /// The bound is the point. Before the deadline moved to the writer task
    /// it was evaluated in the read loop's own 15 s heartbeat arm, so it
    /// could only be noticed at a tick: a probe against a live server
    /// measured 60.0 s for the 45 s default, and this test with its 200 ms
    /// timeout would have taken 15 s. pvxs closes at its `tcpTimeout`
    /// (40.0 s measured against the 40 s default) because there the timeout
    /// is libevent's on the bufferevent, which owns the fd.
    ///
    /// Mutation-checked: drop the `tx.closed()` arm from the `tcp.rs`
    /// `select!` and the connection stays parked past `RETIRE_BOUND`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_peer_is_dropped_when_the_inactivity_deadline_expires() {
        let idle_timeout = Duration::from_millis(200);
        let config = PvaServerConfig {
            idle_timeout,
            ..isolated_config()
        };
        let started = Instant::now();
        // Nothing is ever sent. The server's own CONNECTION_VALIDATION is
        // outbound and must not stand in for the peer's liveness.
        let h = Harness::start(config, Arc::new(ConnRegistry::new()));
        let result = h.wait_retired(RETIRE_BOUND).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= idle_timeout,
            "a connection reaped before its own deadline: {elapsed:?}"
        );
        assert!(
            result.is_ok(),
            "reaping a silent peer is a deliberate close, not an I/O failure: {result:?}"
        );
    }

    /// The other half of that deadline: it is re-armed for whatever silence
    /// is still owed, so a peer that keeps talking is never reaped. The
    /// re-arm is what a fixed ticker got wrong in both directions — late on
    /// a dead peer, and, with the period set anywhere near the timeout,
    /// early on a live one.
    ///
    /// On a [`ManualClock`]: the peer talks every half period, each frame
    /// is known to be stamped once its echo comes back, and each expiry is
    /// known to be evaluated once its re-arm is recorded. Nothing here
    /// waits on the wall clock, so load cannot stretch the peer's silence.
    /// Every re-arm must land one period after the last frame, which is
    /// what a deadline that re-armed for the full period would miss.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_that_keeps_sending_outlives_the_re_armed_deadline() {
        use crate::liveness_clock::ManualClock;
        const IDLE: Duration = Duration::from_millis(200);
        let order = ByteOrder::Little;
        let config = PvaServerConfig {
            idle_timeout: IDLE,
            ..isolated_config()
        };
        let clock = Arc::new(ManualClock::default());
        let mut h = Harness::start_with_clock(
            config,
            Arc::new(ConnRegistry::new()),
            LivenessClock::Manual(clock.clone()),
            test_source(),
            PeerEntry::new(false),
        );
        let armed = |n: usize| {
            let clock = clock.clone();
            async move {
                tokio::time::timeout(RETIRE_BOUND, async {
                    loop {
                        let armed = clock.armed();
                        if armed.len() >= n {
                            return armed[n - 1];
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("deadline #{n} was never armed"))
            }
        };

        let mut echo = Vec::new();
        PvaHeader::control(
            false,
            order,
            crate::proto::ControlCommand::EchoRequest.code(),
            0,
        )
        .write_into(&mut echo);
        // Each expiry arms exactly one deadline, so the count is known
        // without reading it back while the writer may be re-arming.
        let mut deadlines = 1;
        let mut due = armed(deadlines).await;
        assert_eq!(due, IDLE, "the first deadline is one period from the start");
        let mut now = Duration::ZERO;
        let mut last_frame = Duration::ZERO;
        for _ in 0..10 {
            now += IDLE / 2;
            clock.advance(IDLE / 2);
            if now >= due {
                deadlines += 1;
                due = armed(deadlines).await;
                assert_eq!(
                    due,
                    last_frame + IDLE,
                    "at {now:?} the deadline re-armed for the full period instead of \
                     what was owed since the frame at {last_frame:?}"
                );
            }
            h.client.send(&echo);
            h.client
                .read_until_control(crate::proto::ControlCommand::EchoResponse);
            last_frame = now;
        }
        assert!(
            !h.conn
                .as_ref()
                .expect("connection task present")
                .is_finished(),
            "a peer sending every half period must not be reaped"
        );

        // A full period of silence after the last frame is reaped at the
        // deadline the last re-arm set.
        clock.advance(IDLE);
        let result = h.wait_retired(RETIRE_BOUND).await;
        assert!(
            result.is_ok(),
            "reaping a silent peer is a deliberate close, not an I/O failure: {result:?}"
        );
    }

    /// Every `has_pv` waits until the gate opens.
    struct GatedSource(tokio::sync::watch::Sender<bool>);

    impl crate::server_native::ChannelSource for GatedSource {
        async fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
            Vec::new()
        }
        async fn has_pv(&self, _: &[u8]) -> bool {
            let _ = self.0.subscribe().wait_for(|open| *open).await;
            true
        }
        async fn get_introspection(&self, _: &[u8]) -> Option<FieldDesc> {
            Some(scalar_intro())
        }
        async fn get_value(&self, _: &[u8]) -> Option<PvField> {
            None
        }
        async fn put_value(
            &self,
            _: &[u8],
            _: PvField,
        ) -> Result<(), crate::server_native::OpError> {
            Err("read-only".into())
        }
        async fn is_writable(&self, _: &[u8]) -> bool {
            false
        }
        async fn subscribe(
            &self,
            _: &[u8],
        ) -> Option<crate::server_native::MonitorStream<PvField>> {
            None
        }
    }

    /// A CREATE_CHANNEL resolver pause is the read loop's own silence, so
    /// when reads resume `last_rx` restarts: the deadline that falls after
    /// the resume is owed one period from the resume, not from the last
    /// frame before a pause longer than the period. On a [`ManualClock`],
    /// as above; the pause is known to hold once the server has consumed
    /// the one frame that cannot be queued, and known over once that
    /// frame's reply arrives, which the read loop writes after the resume.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resolver_pause_restarts_the_idle_deadline_on_resume() {
        use super::super::tcp::{CREATE_CHANNEL_QUEUE_DEPTH, CREATE_CHANNEL_RESOLVE_CONCURRENCY};
        use crate::liveness_clock::ManualClock;
        const IDLE: Duration = Duration::from_millis(200);
        // Every worker held in `has_pv` and the queue full: the next frame
        // decoded waits for queue space, and no frame after it is read.
        const FRAMES: usize = CREATE_CHANNEL_RESOLVE_CONCURRENCY + CREATE_CHANNEL_QUEUE_DEPTH + 1;
        let order = ByteOrder::Little;
        let config = PvaServerConfig {
            idle_timeout: IDLE,
            ..isolated_config()
        };
        let clock = Arc::new(ManualClock::default());
        let gate = tokio::sync::watch::Sender::new(false);
        let peer = PeerEntry::new(false);
        let mut h = Harness::start_with_clock(
            config,
            Arc::new(ConnRegistry::new()),
            LivenessClock::Manual(clock.clone()),
            Arc::new(GatedSource(gate.clone())),
            peer.clone(),
        );
        let within = |what: String, done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + RETIRE_BOUND;
            while !done() {
                assert!(Instant::now() < deadline, "{what}");
                thread::sleep(Duration::from_millis(1));
            }
        };
        let armed = |n: usize| {
            within(format!("deadline #{n} was never armed"), &|| {
                clock.armed().len() >= n
            });
            clock.armed()[n - 1]
        };

        assert_eq!(armed(1), IDLE);
        let frames: Vec<Vec<u8>> = (0..FRAMES as u32)
            .map(|cid| {
                app_frame(
                    Command::CreateChannel,
                    order,
                    create_channel_payload(cid, &format!("G:{cid:03}"), order),
                )
            })
            .collect();
        h.client.send(&frames.concat());
        let consumed = frames.iter().map(Vec::len).sum::<usize>() as u64;
        within("the pausing frame was never read".into(), &|| {
            peer.bytes_in.load(Ordering::Relaxed) == consumed
        });

        // A pause three periods long: the deadline that falls inside it
        // does not reap, and re-arms.
        let resume = 3 * IDLE;
        clock.advance(resume);
        let in_pause = armed(2);
        assert!(in_pause > resume, "{in_pause:?}");
        gate.send_replace(true);
        for _ in 0..FRAMES {
            h.client.read_until(Command::CreateChannel);
        }

        clock.advance(in_pause - resume);
        assert_eq!(
            armed(3),
            resume + IDLE,
            "the deadline after the resume counted the pause as peer silence"
        );
        let _ = h.finish().await;
    }

    /// The read-stall watchdog MUST NOT close a connection while its reads
    /// are paused for a resolver pause, and MUST close a silent peer
    /// `op_timeout` after reads resume — not before, and not a tick later
    /// than that. On a [`ManualClock`], as above; a tick is known evaluated
    /// once the ticker arms a deadline past the clock, and `idle_timeout`
    /// is set past every advance so only this watchdog can close. `op_timeout`
    /// is long in wall time because it is also the socket's `SO_RCVTIMEO`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_read_stall_watchdog_skips_a_resolver_pause_and_closes_at_op_timeout() {
        use super::super::tcp::{CREATE_CHANNEL_QUEUE_DEPTH, CREATE_CHANNEL_RESOLVE_CONCURRENCY};
        use crate::liveness_clock::ManualClock;
        // Ticks every 15 s, so the close falls exactly on a tick.
        const OP: Duration = Duration::from_secs(3600);
        const FRAMES: usize = CREATE_CHANNEL_RESOLVE_CONCURRENCY + CREATE_CHANNEL_QUEUE_DEPTH + 1;
        let order = ByteOrder::Little;
        let config = PvaServerConfig {
            op_timeout: OP,
            idle_timeout: 100 * OP,
            ..isolated_config()
        };
        let clock = Arc::new(ManualClock::default());
        let gate = tokio::sync::watch::Sender::new(false);
        let peer = PeerEntry::new(false);
        let mut h = Harness::start_with_clock(
            config,
            Arc::new(ConnRegistry::new()),
            LivenessClock::Manual(clock.clone()),
            Arc::new(GatedSource(gate.clone())),
            peer.clone(),
        );
        let within = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + RETIRE_BOUND;
            while !done() {
                assert!(Instant::now() < deadline, "{what}");
                thread::sleep(Duration::from_millis(1));
            }
        };
        let open = |h: &Harness| !h.conn.as_ref().expect("connection task").is_finished();
        // Advance to `to` and wait until every tick up to it is evaluated,
        // or the connection has closed on one of them.
        let advance_to = |h: &Harness, now: &mut Duration, to: Duration| {
            clock.advance(to - *now);
            *now = to;
            within("the watchdog never caught up with the clock", &|| {
                !open(h) || clock.ticks().iter().any(|&at| at > to)
            });
        };

        let frames: Vec<Vec<u8>> = (0..FRAMES as u32)
            .map(|cid| {
                app_frame(
                    Command::CreateChannel,
                    order,
                    create_channel_payload(cid, &format!("G:{cid:03}"), order),
                )
            })
            .collect();
        h.client.send(&frames.concat());
        let consumed = frames.iter().map(Vec::len).sum::<usize>() as u64;
        within("the pausing frame was never read", &|| {
            peer.bytes_in.load(Ordering::Relaxed) == consumed
        });

        let mut now = Duration::ZERO;
        let resume = 50 * OP;
        advance_to(&h, &mut now, resume);
        assert!(
            open(&h),
            "the watchdog closed the connection during the resolver pause"
        );

        gate.send_replace(true);
        for _ in 0..FRAMES {
            h.client.read_until(Command::CreateChannel);
        }
        advance_to(&h, &mut now, resume + OP - Duration::from_secs(1));
        assert!(
            open(&h),
            "the watchdog closed the connection before op_timeout of silence after the resume"
        );

        clock.advance(Duration::from_secs(1));
        let result = h.wait_retired(RETIRE_BOUND).await;
        assert!(
            matches!(result, Err(PvaError::Timeout)),
            "the watchdog did not close the connection at op_timeout: {result:?}"
        );
    }

    /// §4.2b without a seventh `select!` arm: a writer that ends wakes the
    /// connection through the socket, so the reader pump's parked `read`
    /// returns and its adapter reports end-of-stream, down which the loop
    /// unwinds on its existing EOF path. This is what lets `tcp.rs` stay
    /// untouched and the hosted `select!` stay byte-identical.
    ///
    /// Through the real reader pump, not a bare `read` on the descriptor: the
    /// wake is the pump's contract, not the socket's. On Windows the parked
    /// `recv` is never returned by the shutdown at all; it is the pump's wait
    /// cap that sees it (`runtime::blocking_io`), and a bare read asserted the
    /// one thing that target does not do. The reader guard is held until the
    /// assertion has been made, because its drop shuts the same socket and
    /// would otherwise answer the question this test is asking.
    ///
    /// Mutation-checked: delete the socket shutdown at the end of
    /// `runtime::blocking_io`'s writer pump and the parked reader here is never
    /// released.
    #[test]
    fn a_writer_that_ends_wakes_a_reader_parked_on_the_socket() {
        // The client stays connected and silent, so the only thing that can
        // return the reader's `read` is a shutdown from this side.
        let (_client, server, peer) = socket_pair();
        let server = Arc::new(server);
        // An effectively-infinite receive bound: only the shutdown can end the
        // pump.
        server
            .set_read_timeout(Some(Duration::from_secs(64_000)))
            .expect("SO_RCVTIMEO");

        let label = format!("PVA connection {peer}");
        // Held past both guards below, so the set returns only after the pump
        // jobs are joined.
        let (_lease, reader_worker, writer_worker) = lease_pumps();
        let (mut reader_adapter, reader_guard) = spawn_reader_pump(
            reader_worker,
            server.clone(),
            &label,
            DEFAULT_READ_CHUNK,
            CHUNK_QUEUE_DEPTH,
        );
        // The peer sends nothing, so the pump is parked in its read.
        thread::sleep(Duration::from_millis(200));

        let (writer_adapter, writer_guard) = spawn_writer_pump(
            writer_worker,
            server.clone(),
            &label,
            Duration::from_secs(5),
            FRAME_QUEUE_DEPTH,
        );

        // The pump's last strong sender goes with the guard: it ends, and on the
        // way out it shuts the socket, waking the reader.
        drop(writer_adapter);
        drop(writer_guard);
        let started = Instant::now();
        let mut buf = [0u8; 64];
        let n = loop {
            let mut read_buf = tokio::io::ReadBuf::new(&mut buf);
            let mut cx = Context::from_waker(Waker::noop());
            match tokio::io::AsyncRead::poll_read(
                std::pin::Pin::new(&mut reader_adapter),
                &mut cx,
                &mut read_buf,
            ) {
                Poll::Ready(Ok(())) => break read_buf.filled().len(),
                Poll::Ready(Err(e)) => panic!("the woken read surfaces as end-of-stream, not {e}"),
                Poll::Pending => {
                    assert!(
                        started.elapsed() < RETIRE_BOUND,
                        "a parked reader must be released when the writer ends"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        assert_eq!(n, 0, "the woken read must report end-of-stream");

        drop(reader_guard);
    }

    /// F6: a connection that panics while holding the registry lock must not
    /// take every later connection with it.
    ///
    /// The registry mutex is process-wide, so `.expect()` on it turned one
    /// client's panic into a panic on every subsequent accept. This is the
    /// formerly-fatal path: poison the mutex, then use the registry normally.
    #[test]
    fn a_poisoned_registry_still_registers_and_stops() {
        let registry = ConnRegistry::new();
        let (_client, server, _peer) = socket_pair();
        let server = Arc::new(server);

        // Poison it exactly the way a connection would: panic while holding
        // the lock.
        let poisoner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.state();
            panic!("a connection blew up holding the registry lock");
        }));
        assert!(poisoner.is_err(), "precondition: the closure must panic");
        assert!(
            registry.state.is_poisoned(),
            "precondition: the mutex must actually be poisoned, or this test \
             proves nothing"
        );

        // Every entry point must still work. Before the fix each of these
        // panicked.
        assert_eq!(registry.live_connections(), 0);
        {
            let _registration = registry.register(server.clone());
            assert_eq!(registry.live_connections(), 1);
            registry.stop();
        }
        assert_eq!(
            registry.live_connections(),
            0,
            "deregistration on drop must still run through a poisoned lock"
        );
    }

    /// The state a poisoned registry carries forward is the state it had: the
    /// recovery must not silently reset the map or the id counter.
    #[test]
    fn poison_recovery_preserves_the_registry_contents() {
        let registry = ConnRegistry::new();
        let (_c1, s1, _p1) = socket_pair();
        let (_c2, s2, _p2) = socket_pair();
        let first = registry.register(Arc::new(s1));

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.state();
            panic!("boom");
        }));
        assert!(poisoned.is_err());

        assert_eq!(
            registry.live_connections(),
            1,
            "the connection registered before the panic must still be there"
        );
        let second = registry.register(Arc::new(s2));
        assert_ne!(first.id, second.id, "the id counter must not have reset");
        assert_eq!(registry.live_connections(), 2);
    }

    /// F3, the formerly-bypassing path: the reader is spawned, connection
    /// setup then fails before the writer exists, and the function `?`s out.
    ///
    /// Before the guard this leaked the reader permanently.
    /// `ConnRegistration::drop` deregisters without waking, so
    /// `ConnRegistry::stop` could no longer reach it and it stayed parked in
    /// `read` behind an `op_timeout` of ~64,000 s, holding its socket and
    /// descriptor for the life of the IOC — while `live_connections()` read 0
    /// and the `max_connections` slot came back, so nothing looked wrong.
    #[test]
    fn a_reader_is_released_when_connection_setup_fails_after_it_is_spawned() {
        // The client stays connected and silent, so nothing but a wake from
        // this side can return the reader's `read`.
        let (_client, server, peer) = socket_pair();
        let registry = ConnRegistry::new();
        let server = Arc::new(server);

        let started = Instant::now();
        {
            let _registration = registry.register(server.clone());
            let label = format!("PVA connection {peer}");
            // An effectively-infinite receive timeout, exactly as `op_timeout`
            // gives the real driver: only the guard's shutdown can end this
            // pump.
            server
                .set_read_timeout(Some(Duration::from_secs(64_000)))
                .expect("SO_RCVTIMEO");
            // `_lease` drops after `_reader`'s guard has joined the pump job.
            let (_lease, reader_worker, _writer_worker) = lease_pumps();
            let (_adapter, _reader) = spawn_reader_pump(
                reader_worker,
                server.clone(),
                &label,
                DEFAULT_READ_CHUNK,
                CHUNK_QUEUE_DEPTH,
            );
            assert_eq!(registry.live_connections(), 1);
            thread::sleep(Duration::from_millis(200));
            // Stand-in for the writer-spawn `?`: leave the scope with the
            // reader running and the writer never created. Leaving it is what
            // must wake and join the pump — if it did not, this scope exit
            // would block until the 64,000 s timeout.
        }
        assert!(
            started.elapsed() < RETIRE_BOUND,
            "a reader spawned before a failed setup must still be woken and joined"
        );
        assert_eq!(
            registry.live_connections(),
            0,
            "the registration must still be removed"
        );
    }

    /// The same guard on the panic path, which no cleanup on the error branch
    /// could have covered — `catch_unwind` stands in for a panic unwinding out
    /// of `handle_connection_io`.
    #[test]
    fn a_reader_is_released_when_the_connection_handler_panics() {
        let (_client, server, peer) = socket_pair();
        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        server
            .set_read_timeout(Some(Duration::from_secs(64_000)))
            .expect("SO_RCVTIMEO");

        let started = Instant::now();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _registration = registry.register(server.clone());
            let label = format!("PVA connection {peer}");
            let (_lease, reader_worker, _writer_worker) = lease_pumps();
            let (_adapter, _reader) = spawn_reader_pump(
                reader_worker,
                server.clone(),
                &label,
                DEFAULT_READ_CHUNK,
                CHUNK_QUEUE_DEPTH,
            );
            panic!("connection handler blew up");
        }));

        assert!(panicked.is_err(), "precondition: the closure must panic");
        assert!(
            started.elapsed() < RETIRE_BOUND,
            "a panic unwinding past the reader must still wake and join it"
        );
        assert_eq!(registry.live_connections(), 0);
    }

    /// Structural closure, as source: both connection threads are owned by a
    /// guard. A bare `let reader = spawn_dedicated_thread(..)` is the shape
    /// that leaked, and it must not come back.
    #[test]
    fn both_connection_threads_are_owned_by_a_joining_guard() {
        // Scoped to production: a *test* below deliberately spawns a bare
        // reader thread to stand in for a pump it must not own (owning it
        // would shut the socket and answer that test's own question). Scanning
        // the whole file made this guard fire on that, which is a guard
        // punishing the test that proves the property it is guarding.
        // `Strip`: the doc comment above names the banned shape on purpose,
        // and this guard is about CODE. (Needles are split for the same
        // reason — this guard lives in the file it reads.)
        let src = production(include_str!("blocking.rs"), Comments::Strip);
        let code: Vec<&str> = src.lines().collect();
        for banned in [
            concat!("let reader = spawn_dedi", "cated_thread("),
            concat!("let writer = spawn_dedi", "cated_thread("),
            concat!("let reader = thread::", "spawn("),
            concat!("let writer = thread::", "spawn("),
        ] {
            let offenders: Vec<&&str> = code.iter().filter(|l| l.contains(banned)).collect();
            assert!(
                offenders.is_empty(),
                "a connection thread is held in a bare handle again: `{banned}`. \
                 An exit path that does not join it leaks the thread, its socket \
                 and its descriptor for the life of the IOC"
            );
        }
        // The guards themselves now live in `epics_base_rs::runtime::blocking_io`
        // and are tested there; what this file must still get right is binding
        // BOTH of them, so the `?` and panic paths run their `Drop`. A pump
        // spawned into `_` would compile and leak.
        for required in [
            concat!("let (reader_adapter, reader) = spawn_reader_", "pump("),
            concat!("let (writer_adapter, writer) = spawn_writer_", "pump("),
        ] {
            assert!(
                code.iter().any(|l| l.contains(required)),
                "connection setup no longer binds `{required}` — both pumps \
                 must be owned by a guard that joins them on every exit path"
            );
        }
    }

    // ── the accept loop (stage C) ───────────────────────────────────────

    /// Stand a server up and give it an accept thread.
    ///
    /// The accept thread goes through the seam, not `thread::spawn`, and that
    /// is not decoration: on a hosted build `spawn_dedicated_thread` captures
    /// the calling thread's runtime, so the per-connection threads the accept
    /// loop spawns capture it in turn. The ambient context propagates down the
    /// thread tree because every level was spawned the same way. On RTEMS
    /// there is nothing to propagate and the same code is a plain thread.
    /// The executor this driver's hosted connections spawn on.
    ///
    /// Not `crate::test_reactor()`, and the difference is the module's own
    /// claim: these bodies are deliberately synchronous `#[test]`s, because
    /// the driver serves a connection with three threads and no reactor *of
    /// its own*. On a hosted build `serve_connection_blocking` still runs
    /// `handle_connection_io`, whose writer task needs a tokio runtime — see
    /// "Where the async goes on each target" above, which requires every
    /// hosted caller to be on a multi-thread runtime worker. So the harness
    /// supplies exactly that runtime, once for the whole test binary, rather
    /// than each body pretending to be async. On RTEMS the same call is the
    /// process-global background executor and no runtime is built.
    #[cfg(tokio_backend)]
    pub(crate) fn harness_reactor() -> epics_base_rs::runtime::task::Reactor {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let rt = RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("harness multi-thread runtime")
        });
        // The runtime is a `static`, so the handle this mints outlives every
        // thread that spawns through it.
        let _entered = rt.enter();
        epics_base_rs::runtime::task::Reactor::current()
            .expect("the harness runtime was just entered on this thread")
    }

    /// `exec_backend` twin: there is no runtime to build — see the
    /// `tokio_backend` copy.
    #[cfg(exec_backend)]
    pub(crate) fn harness_reactor() -> epics_base_rs::runtime::task::Reactor {
        crate::test_reactor()
    }

    /// The band a bound server hands its connection work, measured through the
    /// same accessor `serve` uses.
    ///
    /// pvxs runs its TCP acceptor and connection reactor at `CAServerLow-2`;
    /// before `pva_executor` the accept loop honoured that and every
    /// future it spawned went to the callback pool's middle band instead.
    #[cfg(exec_backend)]
    #[test]
    fn a_bound_server_spawns_its_connection_work_on_its_own_band() {
        use std::sync::mpsc;
        let server =
            BlockingPvaServer::bind((Ipv4Addr::LOCALHOST, 0), test_source(), isolated_config())
                .expect("bind the blocking PVA server");
        let reactor = server.conn_reactor(&harness_reactor());
        let (tx, rx) = mpsc::channel();
        reactor.spawn(async move {
            tx.send(
                std::thread::current()
                    .name()
                    .unwrap_or_default()
                    .to_string(),
            )
            .expect("send");
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("the spawned future ran"),
            "PVASTASK",
        );
        server.shutdown();
    }

    /// A queued frame reaches the socket while the task executor is busy.
    /// One scan pass wakes a monitor task per subscription, and with the
    /// writer on the same FIFO worker the first frame of the burst waited
    /// for every one of them. Here the task worker is held outright: the
    /// connection's first frame must still arrive.
    #[cfg(exec_backend)]
    #[test]
    fn a_frame_reaches_the_peer_while_the_task_executor_is_busy() {
        use std::io::Read;
        use std::sync::mpsc;
        let (server, accept) = start_server(isolated_config());
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        server.conn_reactor(&harness_reactor()).spawn(async move {
            held_tx.send(()).expect("held");
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        });
        held_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the task worker is held");

        let mut client = TcpStream::connect(server.local_addr().expect("addr")).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("read timeout");
        let mut header = [0u8; crate::proto::PvaHeader::SIZE];
        let read = client.read_exact(&mut header);
        release_tx.send(()).expect("release the task worker");
        read.expect("the first frame arrives while PVASTASK is held");
        assert_eq!(header[0], 0xCA, "a PVA frame");

        drop(client);
        stop_and_join(&server, accept);
    }

    fn start_server(config: PvaServerConfig) -> (Arc<BlockingPvaServer>, thread::JoinHandle<()>) {
        let server = Arc::new(
            BlockingPvaServer::bind((Ipv4Addr::LOCALHOST, 0), test_source(), config)
                .expect("bind the blocking PVA server"),
        );
        let serving = server.clone();
        let accept = spawn_dedicated_thread(
            "test-PVAS-accept".into(),
            PVA_SERVER_PRIORITY,
            StackSizeClass::Medium,
            move || serving.serve(&harness_reactor()),
        )
        .expect("accept thread spawned");
        (server, accept)
    }

    /// Poll `f` until it holds, or fail. Connection setup and teardown both
    /// finish on threads this test does not join, so every assertion about
    /// them is "eventually", and it must be a bounded eventually.
    pub(crate) fn eventually(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{what} did not happen within 5s");
    }

    /// Shut the server down and collect its accept thread under a bound.
    ///
    /// `JoinHandle::join` cannot time out, so a regression in either half of
    /// `shutdown` would hang the run rather than fail it — the same trap
    /// `Harness::wait_retired` exists to avoid on the connection side.
    fn stop_and_join(server: &BlockingPvaServer, accept: thread::JoinHandle<()>) {
        server.shutdown();
        eventually("shutdown returns the accept loop", || accept.is_finished());
        accept.join().expect("accept thread joined");
    }

    /// The priority is derived from upstream, not typed in. pvxs runs PVXTCP
    /// at `epicsThreadPriorityCAServerLow-2` (`src/server.cpp:388`) and
    /// `epicsThreadPriorityCAServerLow` is 20 (`epicsThread.h:82`), so an edit
    /// that "rounds" this to CaServerLow or to 19 has to fail something.
    #[test]
    fn the_server_priority_is_the_pvxs_value() {
        assert_eq!(
            PVA_SERVER_PRIORITY.value(),
            ThreadPriority::CaServerLow.value() - 2,
            "pvxs runs its TCP acceptor and connection reactor at CAServerLow-2"
        );
        assert_eq!(PVA_SERVER_PRIORITY.value(), 18);
    }

    /// One rule for every thread this module puts a connection on: it comes
    /// from the worker pool, never a fresh creation. A raw `thread::Builder` in
    /// production would be a thread with no priority and, on the host, no
    /// ambient runtime — and, per connection, the very creation whose RTEMS
    /// residue the pool exists to remove.
    ///
    /// `thread::spawn` is banned for a second reason on top of that one: it
    /// cannot express a stack size at all, so on RTEMS it takes std's generic
    /// 2 MiB `DEFAULT_MIN_STACK_SIZE` instead of C's class. This module puts
    /// three threads on each connection, so that is the difference between 6 MiB
    /// and 2 MiB of the target's fixed pool per client. The `Builder` ban above
    /// does not cover it — `thread::spawn` is invisible to a check that keys on
    /// `Builder` — which is why it is named separately here, as
    /// `epics-base-rs`'s `every_thread_in_this_crate_states_a_stack_size` does.
    ///
    /// And `spawn_dedicated_thread` itself is now banned in production: the
    /// per-connection operation thread it used to create is exactly the leak
    /// this change closed. The connection body runs on a *borrowed* worker
    /// (`conn_pool.acquire` → `run_detached`), and the pumps on the other two
    /// workers of the same set, so this module creates no thread per accept at
    /// all — the pool's fixed set of workers is created once and reused.
    #[test]
    fn every_server_thread_goes_through_the_seam() {
        let prod = production(include_str!("blocking.rs"), Comments::Strip);
        assert_eq!(
            prod.matches(concat!("thread", "::Builder")).count(),
            0,
            "spawn server threads through the worker pool, not directly: the \
             pool is what applies the priority, the stack class, and carries the \
             ambient runtime — and what stops a per-connection creation"
        );
        let bare = concat!("thread", "::spawn(");
        let offenders: Vec<String> = prod
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains(bare) && !t.contains("Builder")
            })
            .map(|(n, _)| format!("blocking.rs:{}", n + 1))
            .collect();
        assert!(
            offenders.is_empty(),
            "these threads state no stack class and take std's 2 MiB RTEMS \
             default — three per connection: {offenders:?}"
        );
        assert_eq!(
            prod.matches(concat!("spawn_dedi", "cated_thread(")).count(),
            0,
            "this module no longer creates a thread per connection: the \
             connection body runs on a borrowed pool worker via `run_detached`, \
             not `spawn_dedicated_thread`, which is the per-accept creation the \
             worker pool was built to remove"
        );
        // The connection body is dispatched onto its borrowed worker exactly
        // once — the counterpart of the single thread this module used to spawn.
        assert_eq!(
            prod.matches("run_detached(").count(),
            1,
            "the connection body must be dispatched onto its `conn` worker with \
             exactly one `run_detached` — the pooled replacement for the old \
             per-connection `spawn_dedicated_thread`"
        );
        // And the pumps really do go through the seam: a pump spawned any other
        // way would not be reachable from these two calls.
        for pump in ["spawn_reader_pump(", "spawn_writer_pump("] {
            assert_eq!(
                prod.matches(pump).count(),
                1,
                "each connection takes exactly one `{pump}` — the seam that \
                 applies the priority and the stack class"
            );
        }
    }

    /// Binding *is* listening: the port is answerable and connectable before
    /// any thread exists, so nothing ever probes a port, releases it and
    /// re-binds.
    #[test]
    fn bind_owns_the_port_before_any_thread_exists() {
        let server =
            BlockingPvaServer::bind((Ipv4Addr::LOCALHOST, 0), test_source(), isolated_config())
                .expect("bind");
        assert_ne!(server.tcp_port(), 0, "an ephemeral bind resolves the port");
        assert_eq!(
            server.local_addr().expect("local addr").port(),
            server.tcp_port()
        );
        // No `serve()` yet: the listen backlog takes this.
        let _client = TcpStream::connect(server.local_addr().expect("addr")).expect("connect");
        assert_eq!(server.active_connections(), 0);
    }

    /// The whole arg assembly, end to end: the accept loop must build what
    /// `accept.rs` builds on the host — source, config, peer entry, channel
    /// invalidator, registry — or the connection cannot answer a
    /// CREATE_CHANNEL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_accept_loop_serves_a_real_channel() {
        let order = ByteOrder::Little;
        let (server, accept) = start_server(isolated_config());
        let mut client = TestClient::connect(server.local_addr().expect("addr"));

        client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(21, "dut", order),
        ));
        let body = client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 21, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a connection accepted by the blocking accept loop must serve channels"
        );

        stop_and_join(&server, accept);
    }

    /// The reserved server meta-channel must exist on a blocking server too.
    ///
    /// This is what `pvxlist -i`, `pvxlist <address>` and our own `pvlist-rs`
    /// create a channel for. Without it the server boots, serves its user PVs
    /// and looks healthy, while every attempt to ask it what it is fails with
    /// "Refused to create Channel" — an IOC that cannot be diagnosed from a
    /// client is the exact failure mode the RTEMS path can least afford,
    /// because there is no shell on the target to ask instead.
    ///
    /// Asserted through the accept loop rather than on the source, so it
    /// covers what `bind` actually composed rather than what a test composed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_reserved_server_channel_is_servable() {
        let order = ByteOrder::Little;
        let (server, accept) = start_server(isolated_config());
        let mut client = TestClient::connect(server.local_addr().expect("addr"));

        client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(42, crate::server_native::SERVER_PV_NAME, order),
        ));
        let body = client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 42, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a blocking PVA server must serve the reserved `{}` channel, or \
             pvxlist/pvlist cannot introspect it at all",
            crate::server_native::SERVER_PV_NAME,
        );

        stop_and_join(&server, accept);
    }

    /// The peer registry is the accept loop's job, and it is what the server
    /// report reads. Present while the connection lives, gone when it ends —
    /// and gone via the slot guard, so an error or a panic returns it too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_connection_is_tracked_in_the_peer_registry_and_released() {
        let (server, accept) = start_server(isolated_config());
        let client = TestClient::connect(server.local_addr().expect("addr"));

        eventually("the peer registry gains the connection", || {
            server.peers().snapshot().len() == 1
        });
        assert_eq!(server.active_connections(), 1);
        let (_peer, snap) = server.peers().snapshot().remove(0);
        assert!(!snap.tls, "the blocking driver serves plain TCP only");

        client.close();
        eventually("the peer registry releases the connection", || {
            server.peers().snapshot().is_empty()
        });
        eventually("the connection slot is returned", || {
            server.active_connections() == 0
        });

        stop_and_join(&server, accept);
    }

    /// Boundary: shutdown with nothing connected. The accept loop is parked
    /// inside `accept()`, which no flag can return on its own — the
    /// self-connect is what wakes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_returns_a_parked_accept_loop() {
        let (server, accept) = start_server(isolated_config());

        // Park the loop *provably*, and with an empty backlog. Merely
        // connecting is not enough: a connection still queued in the listen
        // backlog is itself a pending wake, and an accept loop that exits on
        // it would pass this test with no self-connect at all. So drive one
        // connection all the way through — served, then gone — and only then
        // is the next `accept()` a park with nothing to return it.
        let probe = TestClient::connect(server.local_addr().expect("addr"));
        eventually("the probe connection is served", || {
            server.active_connections() == 1
        });
        probe.close();
        eventually("the probe connection is gone", || {
            server.active_connections() == 0
        });

        stop_and_join(&server, accept);
    }

    /// pvxs binds one listener per expanded interface and never the bind
    /// address beside them (`server.cpp:476-486`): each listed address
    /// serves, the wildcard stays free, and shutdown wakes every accept loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_listed_interface_gets_a_listener_and_nothing_else() {
        let config = PvaServerConfig {
            interfaces: ["127.0.0.1:0", "127.0.0.2:0"]
                .map(|a| a.parse::<SocketAddr>().unwrap().into())
                .to_vec(),
            ..isolated_config()
        };
        let server = Arc::new(
            BlockingPvaServer::bind((Ipv4Addr::UNSPECIFIED, 0), test_source(), config)
                .expect("bind the blocking PVA server"),
        );
        let port = server.tcp_port();
        // A connect, not a bind: Darwin and Windows bind a specific address
        // beside a wildcard listener. 127.0.0.4: Darwin's lo0 carries only
        // the addresses CI aliases.
        TcpStream::connect((Ipv4Addr::new(127, 0, 0, 4), port))
            .expect_err("the wildcard must not be bound beside the listed interfaces");
        let serving = server.clone();
        let accept = spawn_dedicated_thread(
            "test-PVAS-accept".into(),
            PVA_SERVER_PRIORITY,
            StackSizeClass::Medium,
            move || serving.serve(&harness_reactor()),
        )
        .expect("accept thread spawned");
        for ip in [Ipv4Addr::LOCALHOST, Ipv4Addr::new(127, 0, 0, 2)] {
            let client = TestClient::connect(SocketAddr::from((ip, port)));
            eventually("the connection is served", || {
                server.active_connections() == 1
            });
            client.close();
            eventually("the connection is gone", || {
                server.active_connections() == 0
            });
        }
        stop_and_join(&server, accept);
    }

    /// pvxs `serverchan.cpp:241`: a TCP SEARCH reply names the port of the
    /// listener the connection came in on, not one server-wide number.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_tcp_search_reply_names_the_port_it_arrived_on() {
        let (server, other) = named_port::on_a_named_port(|other| {
            match BlockingPvaServer::bind(
                (Ipv4Addr::UNSPECIFIED, 0),
                test_source(),
                two_port_config(other),
            ) {
                Ok(server) => Some((Arc::new(server), other)),
                Err(e) if epics_base_rs::net::port_held(&e) => None,
                Err(e) => panic!("bind the blocking PVA server: {e}"),
            }
        });
        let first = server.tcp_port();
        assert_ne!(first, other);
        let serving = server.clone();
        let accept = spawn_dedicated_thread(
            "test-PVAS-accept".into(),
            PVA_SERVER_PRIORITY,
            StackSizeClass::Medium,
            move || serving.serve(&harness_reactor()),
        )
        .expect("accept thread spawned");
        for (ip, port) in [
            (Ipv4Addr::LOCALHOST, first),
            (Ipv4Addr::new(127, 0, 0, 2), other),
        ] {
            assert_eq!(tcp_search_port(SocketAddr::from((ip, port))), port);
        }
        stop_and_join(&server, accept);
    }

    /// Boundary: shutdown with a live connection that is not going anywhere by
    /// itself. This is the half CA's blocking server still lacks — stopping
    /// the accept loop leaves its clients running — and the reason the server
    /// owns a `ConnRegistry`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_also_ends_a_live_connection() {
        let order = ByteOrder::Little;
        let (server, accept) = start_server(isolated_config());
        let mut client = TestClient::connect(server.local_addr().expect("addr"));

        // Drive one exchange so the connection is provably established and
        // then silent — nothing but `shutdown` can end it.
        client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(31, "dut", order),
        ));
        let _ = client.read_until(Command::CreateChannel);
        eventually("the connection is tracked", || {
            server.active_connections() == 1
        });

        server.shutdown();
        eventually("shutdown ends the live connection", || {
            server.active_connections() == 0
        });
        assert!(
            server.peers().snapshot().is_empty(),
            "its peer entry goes with it"
        );
        eventually("shutdown returns the accept loop", || accept.is_finished());
        accept.join().expect("accept thread joined");
    }

    /// Boundary: at the connection limit. Each PVA connection is three
    /// threads, so an unbounded accept loop is a worse hazard here than on the
    /// host; the refusal must close the socket rather than queue it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn max_connections_refuses_the_next_client() {
        let config = PvaServerConfig {
            max_connections: 1,
            ..isolated_config()
        };
        let (server, accept) = start_server(config);
        let first = TestClient::connect(server.local_addr().expect("addr"));
        eventually("the first connection is served", || {
            server.active_connections() == 1
        });

        let mut refused = TestClient::connect(server.local_addr().expect("addr"));
        let mut byte = [0u8; 1];
        assert_eq!(
            refused
                .0
                .read(&mut byte)
                .expect("read on the refused socket"),
            0,
            "over the limit the server must close the socket, not hold it open"
        );
        assert_eq!(
            server.active_connections(),
            1,
            "a refused connection must not take a slot"
        );

        // And the slot frees up, so the limit is a limit and not a latch.
        first.close();
        eventually("the first connection releases its slot", || {
            server.active_connections() == 0
        });

        stop_and_join(&server, accept);
    }

    // -----------------------------------------------------------------------
    // UDP SEARCH responder (item 7 stage D)
    // -----------------------------------------------------------------------

    /// A SEARCH requester: its own UDP socket, and the two frames a real
    /// client sends — a named search and a `pvlist` discovery probe.
    struct SearchClient(UdpSocket);

    impl SearchClient {
        fn new() -> SearchClient {
            let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("requester bind");
            sock.set_read_timeout(Some(RETIRE_BOUND))
                .expect("requester read timeout");
            SearchClient(sock)
        }

        fn port(&self) -> u16 {
            self.0.local_addr().expect("requester addr").port()
        }

        fn send(&self, frame: &[u8], to: SocketAddr) {
            self.0.send_to(frame, to).expect("requester send");
        }

        fn recv(&self) -> Vec<u8> {
            let mut buf = [0u8; 1500];
            let (n, _) = self.0.recv_from(&mut buf).expect("SEARCH_RESPONSE");
            buf[..n].to_vec()
        }

        /// No reply within `RETIRE_BOUND`, distinguished from a socket error.
        fn expect_silence(&self) {
            let mut buf = [0u8; 1500];
            match self.0.recv_from(&mut buf) {
                Ok((n, from)) => panic!("expected no reply, got {n} bytes from {from}"),
                Err(e) if is_socket_timeout(e.kind()) => {}
                Err(e) => panic!("requester recv failed: {e}"),
            }
        }
    }

    /// Bind a search socket, start the responder on its own thread, and hand
    /// back the address a requester sends to. The responder thread goes
    /// through the same seam the accept thread does, for the same reason.
    #[allow(clippy::type_complexity)]
    fn start_udp_responder(
        config: PvaServerConfig,
    ) -> (
        Arc<BlockingPvaServer>,
        SocketAddr,
        thread::JoinHandle<()>,
        thread::JoinHandle<()>,
    ) {
        let socket = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &config)
            .expect("bind the blocking search socket");
        let (server, accept) = start_server(config);
        let search_addr = socket.local_addr().expect("search addr");
        let responding = server.clone();
        let udp = spawn_dedicated_thread(
            "test-PVAS-udp".into(),
            PVA_UDP_PRIORITY,
            StackSizeClass::Medium,
            move || {
                responding.serve_udp_search(socket).expect("UDP responder");
            },
        )
        .expect("UDP responder thread spawned");
        (server, search_addr, accept, udp)
    }

    /// Start the responder on a socket the test bound, with `config` stamped
    /// with the port that socket holds — the number a forward is addressed to.
    #[allow(clippy::type_complexity)]
    fn start_udp_responder_on(
        socket: SearchSocket,
        mut config: PvaServerConfig,
    ) -> (
        Arc<BlockingPvaServer>,
        thread::JoinHandle<()>,
        thread::JoinHandle<()>,
    ) {
        config.udp_port = socket.local_addr().expect("search addr").port();
        let (server, accept) = start_server(config);
        let responding = server.clone();
        let udp = spawn_dedicated_thread(
            "test-PVAS-udp".into(),
            PVA_UDP_PRIORITY,
            StackSizeClass::Medium,
            move || {
                responding.serve_udp_search(socket).expect("UDP responder");
            },
        )
        .expect("UDP responder thread spawned");
        (server, accept, udp)
    }

    /// [`start_udp_responder_on`] with the local address set the responder
    /// checks a tag against given, not enumerated.
    #[cfg(target_os = "linux")]
    #[allow(clippy::type_complexity)]
    fn start_udp_responder_with_locals(
        mut socket: SearchSocket,
        config: PvaServerConfig,
        locals: &[Ipv4Addr],
    ) -> (
        Arc<BlockingPvaServer>,
        thread::JoinHandle<()>,
        thread::JoinHandle<()>,
    ) {
        let Role::Collector(routing) = &mut socket.role else {
            panic!("a wildcard socket routes");
        };
        routing.local_v4 = locals.iter().copied().collect();
        start_udp_responder_on(socket, config)
    }

    fn wildcard_search_socket(config: &PvaServerConfig) -> (SearchSocket, u16) {
        let socket = bind_udp_search(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0), config)
            .expect("bind the wildcard search socket");
        let port = socket.local_addr().expect("search addr").port();
        (socket, port)
    }

    /// The ORIGIN_TAG channel is joined when `bind_udp_search` returns, as
    /// pvxs's `UDPCollector` is in the `Server` constructor: a SEARCH sent to
    /// the group before the responder thread first runs is still answered.
    #[test]
    fn the_origin_tag_group_is_joined_when_the_bind_returns() {
        let (socket, port) = wildcard_search_socket(&isolated_config());
        // Another process on the host may hold the group and make a wildcard
        // socket receive it anyway; this socket's own membership is what the
        // kernel option below shows.
        #[cfg(target_os = "linux")]
        assert!(
            !socket2::SockRef::from(&socket.socket)
                .multicast_all_v4()
                .expect("IP_MULTICAST_ALL"),
            "the search socket must take only the groups it joined"
        );
        let client = SearchClient::new();
        socket2::SockRef::from(&client.0)
            .set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
            .expect("requester multicast interface");
        let codec = PvaCodec { big_endian: false };
        client.send(
            &codec.build_search(3, 9, "dut", [127, 0, 0, 1], client.port(), false),
            SocketAddr::from((epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP, port)),
        );

        let (server, accept, udp) = start_udp_responder_on(socket, isolated_config());
        let reply = client.recv();
        let header = PvaHeader::decode(&mut Cursor::new(&reply)).expect("reply header");
        assert_eq!(header.command, Command::SearchResponse.code());
        stop_all(&server, accept, udp);
    }

    /// A unicast-flagged SEARCH is forwarded to the group and answered from
    /// the copy that loops back, as the hosted responder's is.
    #[test]
    fn a_unicast_search_is_forwarded_through_the_group_and_answered() {
        let (socket, port) = wildcard_search_socket(&isolated_config());
        let (server, accept, udp) = start_udp_responder_on(socket, isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };
        client.send(
            &codec.build_search(5, 11, "dut", [127, 0, 0, 1], client.port(), true),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        );
        let reply = client.recv();
        let golden = build_search_response_proto(
            server.guid(),
            5,
            server.tcp_port(),
            &[11],
            ByteOrder::Little,
            "tcp",
        );
        assert_eq!(reply, golden);
        stop_all(&server, accept, udp);
    }

    /// Two servers on one port: the one the kernel hands the unicast datagram
    /// forwards it to the group, so both answer. Without the forward only the
    /// recipient replies, and a client of the other server never hears it.
    #[test]
    fn a_unicast_search_reaches_every_server_sharing_the_port() {
        let (first, port) = wildcard_search_socket(&isolated_config());
        let second = bind_udp_search(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
            &isolated_config(),
        )
        .expect("bind the second search socket on the shared port");
        let (server_a, accept_a, udp_a) = start_udp_responder_on(first, isolated_config());
        let (server_b, accept_b, udp_b) = start_udp_responder_on(second, isolated_config());
        assert_ne!(server_a.guid(), server_b.guid());

        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };
        client.send(
            &codec.build_search(7, 13, "dut", [127, 0, 0, 1], client.port(), true),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        );
        let golden = |server: &BlockingPvaServer| {
            build_search_response_proto(
                server.guid(),
                7,
                server.tcp_port(),
                &[13],
                ByteOrder::Little,
                "tcp",
            )
        };
        let mut replies = [client.recv(), client.recv()];
        let mut expected = [golden(&server_a), golden(&server_b)];
        replies.sort();
        expected.sort();
        assert_eq!(replies, expected);
        stop_all(&server_a, accept_a, udp_a);
        stop_all(&server_b, accept_b, udp_b);
    }

    /// Bind a wildcard responder on `config`, send one unicast-flagged
    /// SEARCH to `dest` on its port out of loopback, then start the
    /// responder; whether it answered. The send precedes the responder
    /// thread, so the groups must be joined by the bind.
    #[cfg(target_os = "linux")]
    fn answers_search_to(config: PvaServerConfig, dest: Ipv4Addr) -> bool {
        let (socket, port) = wildcard_search_socket(&config);
        let client = group_client();
        client
            .0
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("requester read timeout");
        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(3, 9, "dut", [127, 0, 0, 1], client.port(), true);
        client.send(&frame, SocketAddr::from((dest, port)));
        let (server, accept, udp) = start_udp_responder_on(socket, config);
        let answered = client.0.recv_from(&mut [0u8; 1500]).is_ok();
        stop_all(&server, accept, udp);
        answered
    }

    /// pvxs joins a beacon group on the collector and listens on it only
    /// under `auto_beacon` (`config.cpp:512-519`). A SEARCH sent to it is
    /// answered from the arrival interface, never forwarded, so a server not
    /// listing that interface answers it too.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_group_search_is_answered_only_with_auto_beacon() {
        let group = Ipv4Addr::new(224, 0, 9, 17);
        let config = |auto_beacon, iface: Ipv4Addr| PvaServerConfig {
            auto_beacon,
            beacon_destinations: vec![
                crate::config::env::Endpoint::parse("224.0.9.17@127.0.0.1", 5076).unwrap(),
            ],
            interfaces: vec![SocketAddr::from((iface, 0)).into()],
            ..isolated_config()
        };
        assert!(answers_search_to(config(true, Ipv4Addr::LOCALHOST), group));
        assert!(!answers_search_to(
            config(false, Ipv4Addr::LOCALHOST),
            group
        ));
        assert!(answers_search_to(
            config(true, Ipv4Addr::new(127, 0, 0, 2)),
            group
        ));
    }

    /// A unicast SEARCH to a local address outside `EPICS_PVAS_INTF_ADDR_LIST`
    /// finds no pvxs listener.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_search_to_an_unlisted_address_is_not_answered() {
        let config = || PvaServerConfig {
            interfaces: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into()],
            ..isolated_config()
        };
        assert!(answers_search_to(config(), Ipv4Addr::LOCALHOST));
        assert!(!answers_search_to(config(), Ipv4Addr::new(127, 0, 0, 2)));
    }

    /// A requester that sends to the group over loopback, as a forwarding
    /// peer does.
    fn group_client() -> SearchClient {
        let client = SearchClient::new();
        socket2::SockRef::from(&client.0)
            .set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
            .expect("requester multicast interface");
        client
    }

    /// Send a SEARCH carrying `reply_addr`, behind a tag naming `origin`, to
    /// the group of a fresh wildcard responder; the reply, if any, goes to
    /// the returned client.
    fn search_via_group(
        origin: Option<Ipv4Addr>,
        reply_addr: [u8; 4],
        seq: u32,
        id: u32,
    ) -> (
        SearchClient,
        Arc<BlockingPvaServer>,
        thread::JoinHandle<()>,
        thread::JoinHandle<()>,
    ) {
        let (socket, port) = wildcard_search_socket(&isolated_config());
        let (server, accept, udp) = start_udp_responder_on(socket, isolated_config());
        let client = group_client();
        let codec = PvaCodec { big_endian: false };
        let mut datagram = origin
            .map(|o| PvaCodec::build_origin_tag_prefix(o).to_vec())
            .unwrap_or_default();
        datagram.extend_from_slice(&codec.build_search(
            seq,
            id,
            "dut",
            reply_addr,
            client.port(),
            false,
        ));
        client.send(
            &datagram,
            SocketAddr::from((epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP, port)),
        );
        (client, server, accept, udp)
    }

    /// A tag naming a local address is peeled and the SEARCH answered.
    #[test]
    fn a_tag_naming_a_local_address_is_answered() {
        let (client, server, accept, udp) =
            search_via_group(Some(Ipv4Addr::LOCALHOST), [127, 0, 0, 1], 8, 14);
        let reply = client.recv();
        let header = PvaHeader::decode(&mut Cursor::new(&reply)).expect("reply header");
        assert_eq!(header.command, Command::SearchResponse.code());
        stop_all(&server, accept, udp);
    }

    /// A forwarded SEARCH that asks for the reply to go to its sender can
    /// never be answered, and pvxs ignores it (`udp_collector.cpp:367-371`).
    #[test]
    fn a_tagged_search_with_a_wildcard_reply_address_gets_no_reply() {
        let (client, server, accept, udp) =
            search_via_group(Some(Ipv4Addr::LOCALHOST), [0, 0, 0, 0], 9, 15);
        client.expect_silence();
        stop_all(&server, accept, udp);
    }

    /// An untagged SEARCH through the group is pvxs `Forwarded`
    /// (`udp_collector.cpp:278-284`): answered, and refused like a tagged one
    /// when it asks for the reply to go to its sender (`:367-371`).
    #[test]
    fn an_untagged_search_through_the_group_is_forwarded() {
        let (client, server, accept, udp) = search_via_group(None, [127, 0, 0, 1], 11, 17);
        let reply = client.recv();
        let header = PvaHeader::decode(&mut Cursor::new(&reply)).expect("reply header");
        assert_eq!(header.command, Command::SearchResponse.code());
        stop_all(&server, accept, udp);
        let (client, server, accept, udp) = search_via_group(None, [0, 0, 0, 0], 12, 18);
        client.expect_silence();
        stop_all(&server, accept, udp);
    }

    /// pvxs honours a tag only on a packet that arrived on loopback addressed
    /// to the group (`udp_collector.cpp:278`, `:522`); the same tagged SEARCH
    /// sent to the unicast port is ignored.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_tagged_search_sent_to_the_unicast_port_gets_no_reply() {
        let (socket, port) = wildcard_search_socket(&isolated_config());
        let (server, accept, udp) = start_udp_responder_on(socket, isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };
        let mut datagram = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        datagram.extend_from_slice(&codec.build_search(
            11,
            17,
            "dut",
            [127, 0, 0, 1],
            client.port(),
            false,
        ));
        client.send(&datagram, SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        client.expect_silence();
        stop_all(&server, accept, udp);
    }

    /// The reply to a tagged SEARCH leaves with the tag's address as its
    /// source (`udp_collector.cpp:514-518`). `127.0.0.2` is local to every
    /// Linux host and is not what the route to `127.0.0.1` would pick.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_reply_to_a_tagged_search_comes_from_the_tag_address() {
        let tag = Ipv4Addr::new(127, 0, 0, 2);
        let (socket, port) = wildcard_search_socket(&isolated_config());
        let (server, accept, udp) =
            start_udp_responder_with_locals(socket, isolated_config(), &[tag]);
        let client = group_client();
        let codec = PvaCodec { big_endian: false };
        let mut datagram = PvaCodec::build_origin_tag_prefix(tag).to_vec();
        datagram.extend_from_slice(&codec.build_search(
            12,
            18,
            "dut",
            [127, 0, 0, 1],
            client.port(),
            false,
        ));
        client.send(
            &datagram,
            SocketAddr::from((epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP, port)),
        );
        let mut buf = [0u8; 1500];
        let (_, from) = client.0.recv_from(&mut buf).expect("SEARCH_RESPONSE");
        assert_eq!(from.ip(), IpAddr::from(tag));
        stop_all(&server, accept, udp);
    }

    /// A tag naming an address that is not one of ours was forwarded by
    /// something else (`udp_collector.cpp:527-533`).
    #[test]
    fn a_tagged_search_naming_a_non_local_origin_gets_no_reply() {
        let (client, server, accept, udp) =
            search_via_group(Some(Ipv4Addr::new(203, 0, 113, 7)), [127, 0, 0, 1], 10, 16);
        client.expect_silence();
        stop_all(&server, accept, udp);
    }

    /// A pinned reply leaves with the pinned source address, an unpinned one
    /// with whatever the route gives; `127.0.0.2` is not what the route to
    /// `127.0.0.1` picks.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pinned_search_reply_carries_the_pinned_source_address() {
        let sender = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("sender bind");
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("client bind");
        client
            .set_read_timeout(Some(RETIRE_BOUND))
            .expect("client read timeout");
        let to = client.local_addr().expect("client addr");
        let mut buf = [0u8; 8];

        send_search_reply(&sender, b"pinned", to, Some(Ipv4Addr::new(127, 0, 0, 2)))
            .expect("pinned send");
        let (n, from) = client.recv_from(&mut buf).expect("pinned reply");
        assert_eq!(&buf[..n], b"pinned");
        assert_eq!(from.ip(), IpAddr::from(Ipv4Addr::new(127, 0, 0, 2)));

        send_search_reply(&sender, b"routed", to, None).expect("unpinned send");
        let (n, from) = client.recv_from(&mut buf).expect("unpinned reply");
        assert_eq!(&buf[..n], b"routed");
        assert_eq!(from.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
    }

    /// A socket bound to one address cannot receive the group, so it does not
    /// forward: a unicast-flagged SEARCH is answered directly, not lost to a
    /// forward nobody receives.
    #[test]
    fn a_socket_bound_to_one_address_answers_a_unicast_search_directly() {
        let (server, search_addr, accept, udp) = start_udp_responder(isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };
        client.send(
            &codec.build_search(6, 12, "dut", [127, 0, 0, 1], client.port(), true),
            search_addr,
        );
        let reply = client.recv();
        let header = PvaHeader::decode(&mut Cursor::new(&reply)).expect("reply header");
        assert_eq!(header.command, Command::SearchResponse.code());
        stop_all(&server, accept, udp);
    }

    /// pvxs throws out of the `Server` constructor when the collector cannot
    /// bind (`udp_collector.cpp:151`). A stranger on 127.0.0.2:P without reuse
    /// flags refuses the wildcard bind on P.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_origin_tag_bind_fails_the_start() {
        let holder = UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 2), 0)).expect("holder");
        let port = holder.local_addr().expect("holder addr").port();
        let err = bind_udp_search(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
            &isolated_config(),
        )
        .err();
        assert!(
            matches!(&err, Some(e) if epics_base_rs::net::port_held(e)),
            "{err:?}"
        );
    }

    fn stop_all(
        server: &BlockingPvaServer,
        accept: thread::JoinHandle<()>,
        udp: thread::JoinHandle<()>,
    ) {
        server.shutdown();
        eventually("shutdown returns the UDP responder", || udp.is_finished());
        udp.join().expect("UDP thread joined");
        stop_and_join(server, accept);
    }

    /// pvxs deliberately runs discovery *below* the connections it serves:
    /// `PVXUDP` is `epicsThreadPriorityCAServerLow-4`
    /// (`udp_collector.cpp:93`), `PVXTCP` is `CAServerLow-2`
    /// (`src/server.cpp:388`). Both numbers are pinned, and so is the ordering —
    /// an edit that unifies the two constants (the natural "simplification")
    /// removes the protection an established connection has against a SEARCH
    /// storm, and must fail here rather than in the field.
    #[test]
    fn the_udp_responder_runs_below_the_connection_threads() {
        assert_eq!(
            PVA_UDP_PRIORITY.value(),
            ThreadPriority::CaServerLow.value() - 4,
            "pvxs runs its UDP search collector at CAServerLow-4"
        );
        assert_eq!(PVA_UDP_PRIORITY.value(), 16);
        assert!(
            PVA_UDP_PRIORITY.value() < PVA_SERVER_PRIORITY.value(),
            "SEARCH traffic must never be able to starve an established connection"
        );
    }

    /// Both loops enter their thread role at the top of the thread they run
    /// on, and there are exactly two such loops. A third `enter_ioc_thread`
    /// would mean a loop nobody assigned a number to, or a number assigned
    /// twice.
    ///
    /// The prologue is `enter_ioc_thread`, not `apply_to_current_thread`:
    /// the former also publishes the thread's name to the OS, which on RTEMS
    /// is the only way a name set with `Builder::name` reaches a task
    /// listing. A loop that took only its priority would run anonymous
    /// there, so the bare form is asserted absent rather than merely
    /// uncounted.
    #[test]
    fn both_serve_loops_take_their_priority_at_the_top() {
        let prod = production(include_str!("blocking.rs"), Comments::Strip);
        assert_eq!(
            prod.matches("enter_ioc_thread(").count(),
            2,
            "the accept loop and the UDP responder, each on the thread it blocks"
        );
        assert_eq!(
            prod.matches("enter_ioc_thread(PVA_SERVER_PRIORITY)")
                .count(),
            1,
            "the accept loop runs at 18"
        );
        assert_eq!(
            prod.matches("enter_ioc_thread(PVA_UDP_PRIORITY)").count(),
            1,
            "the UDP responder runs at 16"
        );
        assert_eq!(
            prod.matches("apply_to_current_thread(").count(),
            0,
            "a thread entry that skips the naming half of the prologue is \
             invisible in an RTEMS task listing"
        );
    }

    /// The whole point of the stage: a client that does not know the TCP port
    /// can find it. And the reply must be the bytes the *hosted* responder
    /// would have produced — the extraction in part 1 is what makes that a
    /// consequence rather than a coincidence, so the golden is built from the
    /// same shared wire builder.
    #[test]
    fn a_udp_search_is_answered_with_the_hosted_response_bytes() {
        let (server, search_addr, accept, udp) = start_udp_responder(isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };

        let frame = codec.build_search(7, 42, "dut", [127, 0, 0, 1], client.port(), false);
        client.send(&frame, search_addr);
        let reply = client.recv();

        let golden = build_search_response_proto(
            server.guid(),
            7,
            server.tcp_port(),
            &[42],
            ByteOrder::Little,
            "tcp",
        );
        assert_eq!(
            reply, golden,
            "the blocking responder must emit the same SEARCH_RESPONSE bytes as the hosted one"
        );

        stop_all(&server, accept, udp);
    }

    /// The SEARCH reply advertises the server address as the **unspecified**
    /// sentinel (v4-mapped `0.0.0.0`), never a concrete local address, and the
    /// client uses the UDP source address instead.
    ///
    /// This is not cosmetic. It is what makes a NAT'd guest reachable: under
    /// QEMU `hostfwd` the guest's own interface address is meaningless on the
    /// host, so a responder that helpfully substituted its local IP would send
    /// every client to an address that does not route — and the failure would
    /// look like "the server never answered", not like a wrong byte. Pinned
    /// here rather than left to a comment on `build_search_response_proto`.
    #[test]
    fn the_search_reply_advertises_the_unspecified_server_address() {
        let (server, search_addr, accept, udp) = start_udp_responder(isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };

        client.send(
            &codec.build_search(1, 5, "dut", [127, 0, 0, 1], client.port(), false),
            search_addr,
        );
        let reply = client.recv();

        // SEARCH_RESPONSE payload: guid[12], seq[4], addr[16], port[2], ...
        let addr_off = PvaHeader::SIZE + 12 + 4;
        let addr = &reply[addr_off..addr_off + 16];
        assert_eq!(
            addr,
            &ip_to_bytes(IpAddr::V4(Ipv4Addr::UNSPECIFIED))[..],
            "the reply must carry the unspecified sentinel, not this host's address"
        );
        assert_eq!(
            ip_from_bytes(&addr.try_into().expect("16 bytes")),
            Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            "and it must decode as unspecified, so the client falls back to the UDP source"
        );

        stop_all(&server, accept, udp);
    }

    /// `pvlist` discovery: an empty protocol list matches nothing, so `found`
    /// is 0 — but the `MustReply` bit still obliges a reply, and without it
    /// this server is invisible to discovery even though it serves PVs.
    #[test]
    fn a_must_reply_discovery_probe_is_answered_with_found_zero() {
        let (server, search_addr, accept, udp) = start_udp_responder(isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };

        client.send(&codec.build_discover_search(9, client.port()), search_addr);
        let reply = client.recv();

        let golden = build_search_response_proto(
            server.guid(),
            9,
            server.tcp_port(),
            &[],
            ByteOrder::Little,
            "tcp",
        );
        assert_eq!(reply, golden, "a MustReply probe must be answered");
        // found byte: guid[12] seq[4] addr[16] port[2] + "tcp" as a size-
        // prefixed string (1 + 3).
        let found_off = PvaHeader::SIZE + 12 + 4 + 16 + 2 + 4;
        assert_eq!(reply[found_off], 0, "no names matched, so found = 0");

        stop_all(&server, accept, udp);
    }

    /// `ignore_addrs` is consulted on the UDP path and only there (pvxs
    /// `src/server.cpp:654-670` vs `serverconn.cpp:461-467`): a noisy host's
    /// discovery traffic is dropped while its direct TCP clients still
    /// connect. The blocking responder must apply the same filter, so a
    /// gateway config that suppresses a peer is not silently ignored on RTEMS.
    #[test]
    fn an_ignored_peer_gets_no_search_reply() {
        let client = SearchClient::new();
        let config = PvaServerConfig {
            ignore_addrs: vec![(IpAddr::V4(Ipv4Addr::LOCALHOST), client.port())],
            ..isolated_config()
        };
        let (server, search_addr, accept, udp) = start_udp_responder(config);
        let codec = PvaCodec { big_endian: false };

        client.send(
            &codec.build_search(3, 11, "dut", [127, 0, 0, 1], client.port(), false),
            search_addr,
        );
        client.expect_silence();

        stop_all(&server, accept, udp);
    }

    /// One server, one identity. `bind` stamps the GUID so the UDP responder
    /// and the TCP-circuit SEARCH handler read the same field — a client that
    /// discovers this server over UDP and then connects must not be told it
    /// found two different servers.
    #[test]
    fn both_search_paths_advertise_one_guid() {
        let (server, search_addr, accept, udp) = start_udp_responder(isolated_config());
        let client = SearchClient::new();
        let codec = PvaCodec { big_endian: false };

        assert_ne!(
            server.guid(),
            [0u8; 12],
            "bind must stamp a GUID, not leave the config default"
        );

        client.send(
            &codec.build_search(2, 8, "dut", [127, 0, 0, 1], client.port(), false),
            search_addr,
        );
        let reply = client.recv();
        assert_eq!(
            &reply[PvaHeader::SIZE..PvaHeader::SIZE + 12],
            &server.guid()[..],
            "the UDP reply must carry the same GUID the TCP-circuit handler reads from the config"
        );

        stop_all(&server, accept, udp);
    }

    /// Two servers in one process are two identities, and neither is the
    /// config's zeros.
    ///
    /// The interesting half is that both are built from the *same*
    /// `isolated_config()` value: identity comes from `bind`, not from the
    /// config a caller hands in, so a caller cannot accidentally clone one
    /// server's identity by cloning its config. This is also the assertion
    /// the removed time+PID fallback would have failed on RTEMS, where one
    /// process and a fixed boot wall-clock made both of its inputs constant.
    #[test]
    fn two_servers_in_one_process_have_distinct_guids() {
        let a = BlockingPvaServer::bind((Ipv4Addr::LOCALHOST, 0), test_source(), isolated_config())
            .expect("bind a");
        let b = BlockingPvaServer::bind((Ipv4Addr::LOCALHOST, 0), test_source(), isolated_config())
            .expect("bind b");

        assert_ne!(a.guid(), [0u8; 12], "a bound server never serves the zeros");
        assert_ne!(b.guid(), [0u8; 12], "a bound server never serves the zeros");
        assert_ne!(
            a.guid(),
            b.guid(),
            "two servers built from one config value must not share an identity"
        );
        assert_eq!(
            isolated_config().guid,
            [0u8; 12],
            "and the config they were built from still carries the default zeros, \
             which is why `bind` stamping is what makes the identity real"
        );
    }

    /// The stop seam. Nothing wakes the responder — it is parked in
    /// `recv_from` — so the read timeout is the only thing that returns it to
    /// the flag. A regression that drops or lengthens the timeout leaves a
    /// thread alive after `shutdown`, and this is where that surfaces.
    ///
    /// Both durations below are absolute, deliberately **not** expressed in
    /// [`UDP_STOP_TICK`]. Written as `UDP_STOP_TICK * 2` the settle sleep moves
    /// with the constant under test, so lengthening the tick to 30 s lands the
    /// shutdown exactly on a timeout boundary and the test still passes — it
    /// pins the loop's shape but not its latency. Absolute numbers make the
    /// mutation visible.
    #[test]
    fn shutdown_retires_the_udp_responder_without_a_datagram() {
        /// Long enough that the responder is provably parked mid-`recv_from`,
        /// short enough to be nowhere near a lengthened tick's boundary.
        const SETTLE: Duration = Duration::from_millis(500);
        /// Generous against scheduler noise, still an order of magnitude below
        /// any tick a regression would plausibly introduce.
        const RETIRE_WITHIN: Duration = Duration::from_secs(2);

        let (server, _search_addr, accept, udp) = start_udp_responder(isolated_config());
        // Deliberately send nothing: the loop must be parked in `recv_from`,
        // not returning through the top on its own.
        thread::sleep(SETTLE);
        assert!(!udp.is_finished(), "the responder must still be running");

        let t0 = Instant::now();
        server.shutdown();
        eventually("shutdown returns the parked UDP responder", || {
            udp.is_finished()
        });
        let elapsed = t0.elapsed();
        assert!(
            elapsed < RETIRE_WITHIN,
            "the responder must retire within one stop tick, not on the next datagram \
             (took {elapsed:?})"
        );
        udp.join().expect("UDP thread joined");
        stop_and_join(&server, accept);
    }

    /// An ephemeral search port takes a fanout co-bind, as a pvxs client's
    /// beacon receiver on that port needs (`client.cpp:641-642`).
    #[test]
    fn an_ephemeral_search_port_takes_a_fanout_co_bind() {
        let first = bind_udp_search(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            &isolated_config(),
        )
        .expect("first bind");
        let port = first.local_addr().expect("addr").port();
        assert_ne!(port, 0, "an ephemeral bind resolves the port");

        bind_udp_search(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            &isolated_config(),
        )
        .expect("a fanout bind on the ephemeral port");
    }

    /// The RTEMS driver's socket handles must be SHARED, never duplicated.
    ///
    /// The banned call is `std`'s clone-the-descriptor method, i.e.
    /// `fcntl(F_DUPFD*)`. **The ban rests on the measurement**: on target
    /// `dup`, `F_DUPFD` and `F_DUPFD_CLOEXEC` all fail on an accepted socket
    /// with ENXIO, while `F_DUPFD` on `/dev/console` succeeds — so the
    /// failure is specific to a libbsd socket and is not a general fcntl
    /// gap. The `F_DUPFD_CLOEXEC` variant additionally cannot work on
    /// **RTEMS 6** whatever the file is: `cpukit/libcsupport/src/fcntl.c`
    /// has no case for it (`:146-220`) and falls to `default: EINVAL`.
    /// That is a series-6 statement (`rtems-kernel_6` `51f962fb3`) and only
    /// that: the series-7 kernel (`181e86a19`) DOES handle
    /// `F_DUPFD_CLOEXEC`, at `:173-175`, so the ban rests on the series we
    /// ship rather than on RTEMS in general.
    ///
    /// **Withdrawn:** this comment used to explain the plain-`F_DUPFD`
    /// failure by saying `duplicate_iop` (`fcntl.c:47-79`) calls the file's
    /// `open_h` and that `rtems_bsd_sysgen_nodeops.open_h` is
    /// `rtems_bsd_sysgen_open_error`. The `duplicate_iop` half is right; the
    /// libbsd half is FALSE at **both** declared pins (`7-freebsd-14`
    /// `fdd9fa734`, `6-freebsd-14` `6d027f0bb`), where `.open_h` is
    /// `rtems_bsd_sysgen_dup` — a real dup through `fget`/`finstall` — and
    /// `rtems_bsd_sysgen_open_error` is defined but named by no handler
    /// table at all. `c86cbc57` and `08d8e275` (both 2026-07-28) made it so,
    /// and both are ancestors of both pins, so the ENXIO measurement
    /// predates the change and the current tree does not explain it. Do not
    /// go read the handler table expecting to confirm the ban: it says the
    /// opposite. The rule itself does not move — the measurement stands and
    /// `Arc<TcpStream>` stays.
    ///
    /// Host CI cannot catch a reintroduced duplicate here — it works
    /// perfectly on Linux and fails only after a target boot. This guard is
    /// the only thing standing in for that.
    #[test]
    fn the_rtems_socket_handles_are_shared_never_duplicated() {
        // Comment lines are skipped: the call-site notes name the banned call
        // on purpose, and the guard is about CODE. A real call cannot live on
        // a line that starts with `//`. The needle is also split, because
        // this guard lives in the file it reads.
        let needle = concat!("try", "_clone");
        let offenders: Vec<usize> = include_str!("blocking.rs")
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim_start().starts_with("//"))
            .filter(|(_, l)| l.contains(needle))
            .map(|(i, _)| i + 1)
            .collect();
        assert!(
            offenders.is_empty(),
            "server_native/blocking.rs: duplicated descriptors cannot work on RTEMS/libbsd (lines {offenders:?}); \
             share one `Arc<TcpStream>` and use `impl Read/Write for &TcpStream`"
        );
    }
}
