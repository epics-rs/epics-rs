//! Blocking, thread-per-connection PVA server driver (RTEMS phase 6 item 5,
//! stage 3 — `doc/pva-rtems-item5-design.md` §1, §3).
//!
//! The second driver, beside [`super::accept`]. It serves one connection with
//! three threads and **no reactor**:
//!
//! ```text
//!   socket --read--> reader thread --Vec<u8> chunks--> ChannelReader
//!                                                         |
//!                                    operation thread: block_on_sync(
//!                                        handle_connection_io(..))
//!                                                         |
//!   socket <--write-- writer thread <--framed bytes-- ChannelWriter
//! ```
//!
//! # The seam is the byte source, not the frame pipeline
//!
//! `handle_connection_io` already takes its reader and writer as
//! `Box<dyn AsyncRead/AsyncWrite>` (§1.2). So this driver adds *implementors*,
//! not a second protocol: every byte still reaches the same parser, the same
//! `select!`, the same handlers. Nothing in the 21,000-line protocol module is
//! `cfg`-ed, and the hosted driver is not touched.
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
//! [`serve_connection_blocking`] must be called from a multi-thread runtime
//! worker on the host. On RTEMS the `exec_backend` supplies both the spawn
//! pool and the timer, so the very same call runs on a bare thread. This
//! module is therefore host-compiled and host-tested — the only way to show
//! hosted behaviour is unchanged — and the reader/writer threads and both
//! adapters are exercised for real either way.
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
//! | writer death: write error or send deadline (§4.2b) | the writer thread, on its way out | `Protocol("client closed")` |
//! | server stop (§4.2c) | [`ConnRegistry::stop`], walking every live connection | `Protocol("client closed")` |
//!
//! [`ConnRegistry`] is that primitive's **single owner**, and the invariant it
//! enforces is:
//!
//! > **MUST** every connection served here is registered before either of its
//! > threads starts and stays registered until both have joined.
//! > **MUST NOT** any path shut a connection's socket down, or take it out of
//! > the registry, other than through a handle the registry issued — a
//! > `ConnWake`, which only `ConnRegistry::register` can construct, or the
//! > registration guard, whose `Drop` is the only remover.
//!
//! Both halves hold by construction; see [`ConnRegistry`] for how.
//!
//! That third column is the point. §4.2b proposed closing the writer-death
//! window with a `oneshot` and a **seventh `select!` arm** in the shared
//! connection loop — which would have changed hosted teardown timing (a
//! sign-off item) or else `cfg`-ed an arm into the protocol module, the one
//! thing `doc/pva-rtems-item7-design.md` §6 forbids. Waking through the socket
//! needs neither: a dead writer shuts the socket, the reader thread's `read`
//! returns 0, and the connection unwinds down the **existing** EOF path. So
//! `tcp.rs` is untouched by stage 4, the hosted `select!` is byte-identical,
//! and the ≤15 s window is gone for this driver anyway.
//!
//! # The accept side
//!
//! [`BlockingPvaServer`] owns the [`ConnRegistry`] and hands sockets to
//! [`serve_connection_blocking`], assembling the arguments the hosted
//! [`super::accept`] assembles for its own driver. Every thread it and this
//! module create goes through `runtime::task::spawn_dedicated_thread` at
//! `PVA_SERVER_PRIORITY` — see that constant for why they all share one
//! number, and the seam function for why a plain `std::thread` is not enough
//! on a hosted build.
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
//! Beacons, the per-NIC send bundle, and the loopback-multicast ORIGIN_TAG
//! forwarding channel: all three need the interface enumerator and the socket
//! options that do not cross to RTEMS. A client that unicasts or broadcasts a
//! SEARCH to this server's port is answered; one that relies on being told we
//! exist is not, yet.

// RTEMS-EXEC-MODEL-ALLOW(18): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{
    Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::accept::AcceptBackoff;
use epics_base_rs::runtime::task::spawn_dedicated_thread;
use epics_base_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, enter_ioc_thread,
};
use tokio::io::ReadBuf;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::config::PvaServerConfig;
use super::peers::{PeerEntry, PeerRegistry};
use super::search_engine::{
    Origin, SearchOutput, filter_inbound, process_search_datagram, random_guid,
};
use super::source::{ChannelInvalidator, DynSource};
use super::tcp::{ConnInit, handle_connection_io};
use crate::error::{PvaError, PvaResult};

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
/// Applying it is best effort and gated on `EPICS_RS_ALLOW_RT_PRIORITY`,
/// which defaults **on** for RTEMS and off for hosted targets
/// (`runtime::task::DEFAULT_POLICY`). On RTEMS the platform arm really does
/// call `pthread_setschedparam`, and the number is load-bearing there: RTEMS
/// pthreads inherit `POSIX_Init`'s priority, so a thread that does not take a
/// band runs one level above idle rather than "at the default".
const PVA_SERVER_PRIORITY: ThreadPriority = ThreadPriority::Custom(18);

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

/// One blocking read, sized to match `read_frame`'s own chunk so the byte
/// arrival pattern is the hosted one.
const READ_CHUNK: usize = 4096;

/// How many SO_SNDTIMEO ticks fit inside one send deadline. The socket
/// timeout only exists to return control to the deadline loop; the deadline
/// is the real bound (§3.3).
const SEND_TICKS_PER_DEADLINE: u32 = 4;

/// A blocking socket op hit its `SO_RCVTIMEO`/`SO_SNDTIMEO`.
///
/// Same classification CA's blocking driver uses (`epics-ca-rs`
/// `server/blocking.rs`, `is_read_timeout`): Unix reports the expiry as
/// `WouldBlock`, some platforms as `TimedOut`.
fn is_socket_timeout(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

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
/// * **MUST** — every connection served by [`serve_connection_blocking`] is
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
/// that role belongs to [`super::accept`], which drives its connections as
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

    /// Take ownership of a connection's socket and issue its wake handle.
    ///
    /// Consumes the `Arc` so the caller cannot keep a second route to the
    /// socket: from here on the only way to shut this connection down is the
    /// handle on the returned guard.
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
        ConnRegistration {
            registry: self,
            id,
            wake,
        }
    }

    /// The only remover. Private, and reached only from
    /// [`ConnRegistration::drop`].
    fn deregister(&self, id: u64) {
        self.state().conns.remove(&id);
    }
}

/// A connection's registration: its wake handle while it lives, and its
/// removal from the registry on the way out — every way out, including a panic
/// unwinding through the driver.
struct ConnRegistration<'a> {
    registry: &'a ConnRegistry,
    id: u64,
    wake: ConnWake,
}

impl ConnRegistration<'_> {
    /// A clone of the wake handle, for a thread that outlives this borrow, or
    /// for the guard that owns one — [`WriterGuard`]'s thread wakes the
    /// connection on its way out (§4.2b), and [`ReaderGuard`] wakes it to
    /// return a parked `read`.
    ///
    /// This is the *only* way to obtain a wake from a registration. There used
    /// to be a `wake(&self)` beside it, which meant the retiring path could
    /// wake without holding anything that joins — the shape that let the
    /// reader leak. Now a wake is only reachable through a guard that joins.
    fn wake_handle(&self) -> ConnWake {
        self.wake.clone()
    }
}

impl Drop for ConnRegistration<'_> {
    fn drop(&mut self) {
        self.registry.deregister(self.id);
    }
}

/// Announce a per-connection child thread that did not end normally.
///
/// [`ReaderGuard`] and [`WriterGuard`] made both losses below *survivable* —
/// the connection is torn down and its slot returned however a child ends.
/// They did not make either loss *visible*, and an IOC that has lost a thread
/// but reads exactly like a healthy one is what closes here. Two losses reach
/// this function:
///
/// * the thread could not be created at all ([`spawn_child`]) — the
///   per-connection thread ceiling, and the very condition [`ReaderGuard`]
///   exists to survive;
/// * the thread panicked, which the guards' `join()` used to discard with
///   `let _ =`.
///
/// Through `errlog` and not `tracing` alone, for the reason the CA driver's
/// client refusal is: `errlog_sev_printf` reaches the console whatever the log
/// configuration — including an RTEMS console whose subscriber is the in-tree
/// one — and printing it is what a C IOC does. The `tracing` event beside it is
/// what a hosted operator with a subscriber already reads.
fn child_thread_lost(role: &str, peer: SocketAddr, what: &str) {
    epics_base_rs::runtime::log::errlog_sev_printf(
        epics_base_rs::runtime::log::ErrlogSevEnum::Major,
        &format!(
            "PVA connection {peer}: the {role} thread {what}; this connection is \
             being torn down. Other connections are unaffected."
        ),
    );
    warn!(
        ?peer,
        role, what, "blocking PVA server: a per-connection thread was lost"
    );
}

/// Spawn one of a connection's two child threads, announcing a failure to
/// create it before the error propagates.
///
/// Both children take the same priority and the same stack class. `Small`:
/// neither builds anything — the reader's whole frame is a [`READ_CHUNK`]
/// buffer and a `read`/`send` loop (`reader_thread`), and the writer drains
/// already-encoded frames from a queue onto the socket (`writer_thread`). The
/// protocol state machine runs on the connection thread, which is `Big`.
///
/// One function rather than two spawn sites so that the failure cannot be
/// propagated silently from either. This is the path that leaves a reader
/// running with no writer — exactly what [`ReaderGuard`] was built to survive —
/// and until now it reached the operator only through the connection thread's
/// `debug!`, which is below every default filter and below the IOC console
/// subscriber's threshold.
fn spawn_child(
    role: &str,
    peer: SocketAddr,
    body: impl FnOnce() + Send + 'static,
) -> PvaResult<thread::JoinHandle<()>> {
    spawn_dedicated_thread(
        format!("PVAS-{role} {peer}"),
        PVA_SERVER_PRIORITY,
        StackSizeClass::Small,
        body,
    )
    .map_err(|e| {
        child_thread_lost(role, peer, &format!("could not be created ({e})"));
        PvaError::Io(e)
    })
}

/// The spawned reader thread, woken and joined on **every** exit path.
///
/// # Invariant
///
/// MUST: once `reader_thread` has been spawned, it is woken and joined before
/// [`serve_connection_blocking`] returns — clean return, `?`, or a panic
/// unwinding out of the connection handler.
///
/// # The defect this closes
///
/// A writer-spawn failure used to `?` out with the reader already running.
/// [`ConnRegistration::drop`] deregisters WITHOUT waking — which is right, the
/// wake belongs to whoever retires the connection — so [`ConnRegistry::stop`]
/// could no longer reach that reader. It sat parked in `read` behind an
/// `SO_RCVTIMEO` of `op_timeout` (~64,000 s by default), holding its socket
/// and its descriptor for the life of the IOC. The `max_connections` slot was
/// returned correctly, which is exactly what made the leak invisible: the
/// connection count looked healthy while descriptors drained away.
///
/// Owning the handle in a guard, rather than calling cleanup on the error
/// branch, is what makes the leak unexpressible: there is no way to have
/// spawned the reader without also holding the value that joins it. The same
/// applies to the panic path, which no error-branch cleanup could have covered.
struct ReaderGuard {
    wake: ConnWake,
    peer: SocketAddr,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // The reader's `read` is parked behind an effectively-infinite
            // timeout, so the socket has to be shut to return it. This is the
            // module's single wake primitive, through the registry-issued
            // handle — the same one `ConnRegistry::stop` would use.
            self.wake.wake();
            // The join result is the only place a panicked reader is ever
            // reported: `reader_thread` returns `()`, so an `Err` here means it
            // unwound, and the connection's own error will be a bland
            // channel-closed rather than the cause. Discarding it left the two
            // unlinkable.
            if handle.join().is_err() {
                child_thread_lost("reader", self.peer, "panicked");
            }
        }
    }
}

/// The spawned writer thread and the only strong frame sender, retired
/// together on **every** exit path.
///
/// The sender lives here rather than beside the guard because the writer parks
/// on `frame_rx.recv()` and leaves only when the last strong sender drops.
/// A guard that joined without dropping the sender would hang; keeping the two
/// in one value means the order cannot be got wrong, and does not depend on
/// the declaration order of two separate locals.
struct WriterGuard {
    frames: Option<mpsc::Sender<Vec<u8>>>,
    peer: SocketAddr,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        // Decisive because it is the only strong sender — the `ChannelWriter`
        // adapter holds a weak handle. The writer drains what is queued, sees
        // `None`, and exits; on its way out it wakes the connection (§4.2b).
        drop(self.frames.take());
        if let Some(handle) = self.handle.take() {
            // Same reading as `ReaderGuard`'s: an `Err` is a panicked writer,
            // and a writer that unwound with frames still queued dropped them.
            if handle.join().is_err() {
                child_thread_lost("writer", self.peer, "panicked");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reader side
// ---------------------------------------------------------------------------

/// `AsyncRead` over a channel of byte chunks — the blocking driver's stand-in
/// for the tokio socket read half.
///
/// **Cancel-safety** is the whole point of the `cur`/`pos` pair and is why
/// this type exists at all rather than a channel being read inline. The
/// hosted `read_frame` is used directly as a `select!` arm and survives losing
/// that race because its accumulated bytes live *outside* it. This adapter has
/// the same property: a chunk leaves the channel only when `poll_recv` returns
/// `Ready`, and a partially-copied chunk stays in `cur`/`pos` across as many
/// dropped `poll_read` futures as the caller likes. A lost race consumes
/// nothing.
pub(super) struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    /// The chunk currently being handed out, and how much of it has gone.
    cur: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            cur: Vec::new(),
            pos: 0,
        }
    }
}

impl tokio::io::AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // No room offered: report "nothing filled" without taking anything
        // out of the channel. Consuming here would be the one way this
        // adapter could lose bytes.
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let me = &mut *self;
        loop {
            if me.pos < me.cur.len() {
                let n = (me.cur.len() - me.pos).min(buf.remaining());
                buf.put_slice(&me.cur[me.pos..me.pos + n]);
                me.pos += n;
                if me.pos == me.cur.len() {
                    me.cur.clear();
                    me.pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match me.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    // An empty chunk is not an EOF marker; skip it rather
                    // than letting it read as one.
                    if chunk.is_empty() {
                        continue;
                    }
                    me.cur = chunk;
                    me.pos = 0;
                }
                // Every sender gone = the reader thread ended (EOF, read
                // error, or RCVTIMEO). Zero bytes filled is what `read_frame`
                // turns into `Protocol("client closed")` — the existing hosted
                // EOF path, unchanged (§4.2a).
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Blocking read loop. Ends on EOF, read error, or `SO_RCVTIMEO`; dropping
/// `tx` on the way out is the EOF signal to the operation thread.
fn reader_thread(sock: Arc<TcpStream>, tx: mpsc::Sender<Vec<u8>>, peer: SocketAddr) {
    // `impl Read for &TcpStream`: one shared descriptor, no `try_clone`.
    let mut sock = &*sock;
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_socket_timeout(e.kind()) => {
                debug!(?peer, "blocking reader: receive timeout, ending connection");
                break;
            }
            Err(e) => {
                debug!(?peer, error = %e, "blocking reader: read failed");
                break;
            }
        };
        // The house sync-over-async primitive: parks this thread (no runtime
        // entered) or hands the worker off (hosted). NOT `blocking_send`,
        // which panics inside a runtime context and would make this same
        // file unusable from a hosted worker.
        if !matches!(block_on_sync(tx.send(chunk[..n].to_vec())), Ok(Ok(()))) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Writer side
// ---------------------------------------------------------------------------

/// Wake slot for a `poll_write` that found the frame channel full. The writer
/// thread wakes it after each frame it takes, which is the moment room
/// appears.
#[derive(Default)]
struct WriteRoom {
    waker: Mutex<Option<Waker>>,
}

impl WriteRoom {
    fn park(&self, cx: &Context<'_>) {
        *self.waker.lock().expect("write-room waker poisoned") = Some(cx.waker().clone());
    }

    fn wake(&self) {
        let waker = self.waker.lock().expect("write-room waker poisoned").take();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// `AsyncWrite` over a channel of frames — the blocking driver's stand-in for
/// the tokio socket write half.
///
/// Holds a [`mpsc::WeakSender`], never a strong one, and that is load-bearing
/// rather than tidiness: this adapter is owned by the connection's writer
/// task, which is aborted (not joined) when the connection ends, so the moment
/// its last strong sender drops is not a moment the driver controls. With only
/// a weak handle here, the driver's own sender is the sole thing keeping the
/// channel open, and dropping it ends the writer thread deterministically
/// instead of whenever the runtime gets round to reaping an aborted task.
pub(super) struct ChannelWriter {
    tx: mpsc::WeakSender<Vec<u8>>,
    room: Arc<WriteRoom>,
}

impl ChannelWriter {
    fn new(tx: mpsc::WeakSender<Vec<u8>>, room: Arc<WriteRoom>) -> Self {
        Self { tx, room }
    }
}

fn write_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "PVA writer thread has ended")
}

impl tokio::io::AsyncWrite for ChannelWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let Some(tx) = self.tx.upgrade() else {
            return Poll::Ready(Err(write_closed()));
        };
        // Register interest BEFORE trying, so a take that happens between the
        // try and the return cannot be missed: either `try_send` sees the room
        // that take created, or the take's `wake()` finds this waker.
        self.room.park(cx);
        match tx.try_send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(write_closed())),
        }
        // `tx` drops here. Nothing in this adapter holds a strong sender
        // across a suspension, which is what makes the driver's drop decisive.
    }

    /// Frames are flushed by the writer thread as it takes them; there is no
    /// buffer here to push.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Write one whole frame under a **deadline**, not merely a per-syscall
/// timeout (§3.3).
///
/// The hosted writer bounds `write_all(&frame)` as a unit. Plain SO_SNDTIMEO
/// bounds each `write` syscall instead, so a client that accepts one byte per
/// tick never trips it and holds the writer thread indefinitely — the exact
/// stuck-client hazard the hosted timeout exists to prevent, on a resource (an
/// OS thread) that is scarcer on RTEMS than a task is on the host. The socket
/// timeout here is only what returns control to this loop; the deadline is the
/// bound.
///
/// A partial write on expiry needs no repair: the caller ends the writer and
/// the connection is torn down, so nothing is ever written to this socket
/// again.
fn write_frame_deadline(sock: &TcpStream, frame: &[u8], send_timeout: Duration) -> io::Result<()> {
    // `impl Write for &TcpStream`: rebind so `write`/`flush` have a mutable
    // place to borrow, without needing `&mut TcpStream` from the caller.
    let mut sock = sock;
    let deadline = Instant::now() + send_timeout;
    let mut off = 0;
    while off < frame.len() {
        // Checked at the top so every way round the loop is bounded,
        // including an `Interrupted` storm.
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "send deadline expired with the frame incomplete",
            ));
        }
        match sock.write(&frame[off..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "peer accepted no bytes",
                ));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // A tick with no progress: fall through to the deadline check.
            Err(e) if is_socket_timeout(e.kind()) => {}
            Err(e) => return Err(e),
        }
    }
    sock.flush()
}

/// Drain frames to the socket in order. Ends when the driver drops the last
/// strong sender, or on the first write error / send-deadline expiry.
///
/// Whichever of those ends it, it wakes the connection on the way out (§4.2b).
/// A dead writer means the connection is over, and the operation loop must not
/// wait up to a heartbeat period to find that out — but the fix is the socket
/// shutdown, not a seventh `select!` arm: the reader's `read` then returns 0
/// and the loop unwinds down the existing EOF path, leaving `tcp.rs` and the
/// hosted timing alone. See the module docs.
fn writer_thread(
    sock: Arc<TcpStream>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    room: Arc<WriteRoom>,
    wake: ConnWake,
    send_timeout: Duration,
    peer: SocketAddr,
) {
    // `Ok(None)` = the driver let go of its sender; `Err(_)` = this thread
    // cannot block here at all. Both end the writer.
    while let Ok(Some(frame)) = block_on_sync(rx.recv()) {
        // A slot just opened; let a parked `poll_write` retry.
        room.wake();
        if let Err(e) = write_frame_deadline(&sock, &frame, send_timeout) {
            debug!(?peer, error = %e, "blocking writer: send failed, ending connection");
            break;
        }
    }
    // Whatever parked the producer, it must not stay parked on a dead writer.
    room.wake();
    // Uniform, not special-cased on *why* the writer ended: the only thing
    // that ends it is the connection being over. On the error paths this is
    // what retires the connection at once; on the normal path the driver is
    // already tearing down and repeats the same shutdown a moment later,
    // harmlessly — every frame this thread was given has been written before
    // it gets here.
    wake.wake();
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Serve one PVA connection over a blocking [`TcpStream`], on this thread.
///
/// This call *is* the operation thread: it runs `handle_connection_io` to
/// completion and returns the connection's result, having joined both child
/// threads. The reader and writer are its children and neither decides the
/// connection is over — they can only report (§4.1).
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
pub fn serve_connection_blocking(
    stream: TcpStream,
    peer: SocketAddr,
    source: DynSource,
    config: PvaServerConfig,
    peer_entry: Arc<super::peers::PeerEntry>,
    channel_invalidator: super::source::ChannelInvalidator,
    registry: &ConnRegistry,
) -> PvaResult<()> {
    let init = ConnInit {
        peer_entry,
        x509_identity: None,
        channel_invalidator,
    };
    let _ = stream.set_nodelay(true);
    // Portable SO_RCVTIMEO / SO_SNDTIMEO (both valid on RTEMS), the same move
    // CA's blocking driver makes. Note the receive timeout is NOT a shutdown
    // mechanism here: `op_timeout` defaults to ~64,000 s, so it is effectively
    // infinite (§1.6). What ends a parked reader is the `shutdown` below.
    stream
        .set_read_timeout(Some(config.op_timeout))
        .map_err(PvaError::Io)?;
    let send_timeout = config.send_timeout;
    let send_tick = (send_timeout / SEND_TICKS_PER_DEADLINE).max(Duration::from_millis(1));
    stream
        .set_write_timeout(Some(send_tick))
        .map_err(PvaError::Io)?;

    // One socket, several handles: the SAME descriptor shared through an
    // `Arc` by both child threads and the registry, which owns it from here
    // on. Registering *before* either thread starts is what closes the window
    // where a thread could be parked on a socket `stop` cannot reach.
    //
    // Shared, not duplicated: `try_clone` is `fcntl(F_DUPFD_CLOEXEC)`, which
    // cannot work for a socket on RTEMS 6 — see the same note in
    // `epics-ca-rs/src/server/blocking.rs::handle_client_blocking` for the
    // measured failure and the RTEMS/libbsd source that causes it.
    // `impl Read/Write for &TcpStream` gives both roles with one descriptor.
    let stream = Arc::new(stream);
    let read_sock = stream.clone();
    let write_sock = stream.clone();
    let registration = registry.register(stream);

    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>(CHUNK_QUEUE_DEPTH);
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
    let room = Arc::new(WriteRoom::default());
    let writer_adapter = ChannelWriter::new(frame_tx.downgrade(), room.clone());

    // Both children go through `spawn_child`, which is the seam at
    // `PVA_SERVER_PRIORITY` — like every other thread in this module, see that
    // constant for why all of them share one number — plus the announcement a
    // failure to create either one owes the operator.
    //
    // From here on the reader is owned by a guard. Every exit below — the
    // writer-spawn `?`, and a panic unwinding out of the connection handler —
    // runs `ReaderGuard::drop`, which wakes and joins it. See that type for the
    // leak this closes.
    let reader = ReaderGuard {
        wake: registration.wake_handle(),
        peer,
        handle: Some(spawn_child("reader", peer, move || {
            reader_thread(read_sock, chunk_tx, peer)
        })?),
    };
    let writer_room = room.clone();
    let writer_wake = registration.wake_handle();
    let writer = WriterGuard {
        // The only strong sender moves into the guard, so it cannot be dropped
        // out of order with the join. `writer_adapter` above already took its
        // weak handle.
        frames: Some(frame_tx),
        peer,
        handle: Some(spawn_child("writer", peer, move || {
            writer_thread(
                write_sock,
                frame_rx,
                writer_room,
                writer_wake,
                send_timeout,
                peer,
            )
        })?),
    };

    let outcome = block_on_sync(handle_connection_io(
        source,
        Box::new(ChannelReader::new(chunk_rx)),
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
    // `room` outlives both, so a producer parked on a full queue is released
    // by the writer's exit wake rather than left holding a dead waker.
    drop(room);
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

/// The accept loop's per-connection bookkeeping, undone on every exit path.
///
/// The peer entry and the `max_connections` slot are both taken *before* the
/// connection thread starts, so both have to come back however that thread
/// ends — clean return, I/O error, or a panic unwinding out of the connection.
/// A guard is the only shape that covers the third, and the thread body is the
/// one place that can hold it.
struct ConnSlot {
    peers: Arc<PeerRegistry>,
    active: Arc<AtomicUsize>,
    peer: SocketAddr,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.peers.remove(self.peer);
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A blocking, thread-per-connection PVA TCP server — the accept side of the
/// driver in this module, and the RTEMS counterpart of [`super::accept`].
///
/// [`serve_connection_blocking`] serves one connection on three threads; this
/// is what gives it sockets and the arguments the hosted accept loop assembles
/// in `accept.rs`. An N-client server therefore costs **3N+2** threads — this
/// accept loop, the UDP search responder, and three per connection — where the
/// hosted driver costs two tasks per connection. That is a stated RTEMS budget
/// item, not an accident.
///
/// It owns the [`ConnRegistry`], because [`serve_connection_blocking`] cannot
/// be called without one, and that makes [`shutdown`](Self::shutdown) able to
/// end live connections rather than only stop accepting new ones.
///
/// TLS is not served: the blocking driver is plain-TCP only (§6), so every
/// connection is registered with `tls = false` and authenticates through
/// CONNECTION_VALIDATION.
pub struct BlockingPvaServer {
    listener: TcpListener,
    source: DynSource,
    config: PvaServerConfig,
    peers: Arc<PeerRegistry>,
    channel_invalidator: ChannelInvalidator,
    connections: Arc<ConnRegistry>,
    tcp_port: u16,
    active: Arc<AtomicUsize>,
    shutdown: AtomicBool,
}

impl BlockingPvaServer {
    /// Bind the accept socket.
    ///
    /// Binding happens here and not in [`serve`](Self::serve), so a
    /// constructed server *is* a listening server: [`tcp_port`](Self::tcp_port)
    /// is answerable before any thread exists, and the port is never probed,
    /// released and re-bound.
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        source: DynSource,
        config: PvaServerConfig,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        // Same reason as the CA blocking driver's `bind`: on RTEMS `bind`
        // succeeds and `local_addr` fails (libc omits the BSD `sin_len`
        // byte), so these two failures must stay distinguishable rather than
        // both surfacing as "cannot bind".
        let tcp_port = listener
            .local_addr()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("bind() succeeded; local_addr() on the listener failed: {e}"),
                )
            })?
            .port();
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
        Ok(Self {
            listener,
            source,
            config,
            peers: PeerRegistry::new(),
            channel_invalidator,
            connections: Arc::new(ConnRegistry::new()),
            tcp_port,
            active: Arc::new(AtomicUsize::new(0)),
            shutdown: AtomicBool::new(false),
        })
    }

    /// The actual bound address — the value to use when the configured port
    /// was 0.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The bound TCP port, as SEARCH replies must advertise it.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// This server's GUID, as both SEARCH paths advertise it.
    pub fn guid(&self) -> [u8; 12] {
        self.config.guid
    }

    /// Live per-connection accounting, for the server report (`pvxsr`).
    pub fn peers(&self) -> &Arc<PeerRegistry> {
        &self.peers
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
    pub fn serve_udp_search(&self, socket: UdpSocket) -> PvaResult<()> {
        handle_udp_search_blocking(
            socket,
            &self.source,
            &self.config,
            self.tcp_port,
            &self.shutdown,
        )
    }

    /// Accept until [`shutdown`](Self::shutdown).
    ///
    /// Blocks the calling thread and takes it to `PVA_SERVER_PRIORITY`, so
    /// give it a thread of its own rather than calling it on a thread that has
    /// other work.
    pub fn serve(&self) {
        let _ = enter_ioc_thread(PVA_SERVER_PRIORITY);
        let mut backoff = AcceptBackoff::new();
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    backoff.accepted();
                    // `shutdown` wakes this parked `accept` by dialling our own
                    // socket; that throwaway connection arrives here and is
                    // dropped with the flag already set.
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(peer) = stream.peer_addr() else {
                        continue;
                    };
                    if let Err(e) = self.start_connection(stream, peer) {
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

    /// Take the slot and the peer entry, then hand the socket to a connection
    /// thread. The slot is moved into the thread body, so a failed spawn
    /// returns it exactly as a finished connection would.
    fn start_connection(&self, stream: TcpStream, peer: SocketAddr) -> io::Result<()> {
        if self.active.load(Ordering::Acquire) >= self.config.max_connections {
            // Dropping the stream closes it. Refusing costs more here than on
            // the host: each connection is three threads, not two tasks.
            //
            // `warn!`, not `debug!`: a client that cannot connect is an
            // operator-visible event, and at `debug!` this refusal was below
            // every default filter — including the IOC console subscriber
            // (`epics_base_rs::runtime::log::install_console_subscriber`),
            // which is the same silent-refusal defect the CA driver had at its
            // thread ceiling.
            warn!(
                ?peer,
                limit = self.config.max_connections,
                "blocking PVA server: refusing connection, max_connections reached"
            );
            return Ok(());
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        let peer_entry = PeerEntry::new(false);
        self.peers.insert(peer, peer_entry.clone());
        let slot = ConnSlot {
            peers: self.peers.clone(),
            active: self.active.clone(),
            peer,
        };

        let source = self.source.clone();
        let config = self.config.clone();
        let invalidator = self.channel_invalidator.clone();
        let connections = self.connections.clone();
        // Big: this is the deep one. It runs the whole protocol state machine
        // under `block_on_sync` — channel create, GET/PUT/MONITOR, introspection
        // and every dispatch into the database, including record processing.
        // It is the structural counterpart of C's per-client `camsgtask`, which
        // rsrv creates with `epicsThreadStackBig`
        // (`rsrv/caservertask.c:109-111`).
        spawn_dedicated_thread(
            format!("PVAS-conn {peer}"),
            PVA_SERVER_PRIORITY,
            StackSizeClass::Big,
            move || {
                // Held for the whole connection: its `Drop` returns the slot and
                // the peer entry even if the connection panics.
                let _slot = slot;
                let outcome = serve_connection_blocking(
                    stream,
                    peer,
                    source,
                    config,
                    peer_entry,
                    invalidator,
                    &connections,
                );
                if let Err(e) = outcome {
                    debug!(?peer, error = %e, "blocking PVA connection ended with error");
                }
            },
        )?;
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
    /// thread. Those threads are detached, so a caller that must observe them
    /// gone watches [`active_connections`](Self::active_connections).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(addr) = self.listener.local_addr() {
            let _ = TcpStream::connect(addr);
        }
        self.connections.stop();
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
/// Raw `libc` for the socket options rather than `socket2`, because `socket2`
/// is one of the three crates that do not cross to RTEMS (`mod.rs`) — the
/// same reason CA's blocking driver hand-rolls its bind.
///
/// `SO_REUSEADDR` + `SO_REUSEPORT` are set **before** bind for a well-known
/// port so several PVA processes can share the search port, which is what
/// pvxs does (`epicsSocketEnableAddressUseForDatagramFanout`, and see
/// `udp_collector.cpp:140-151` for why the bind is wildcard rather than to the
/// multicast address). An **ephemeral** port (`0`) is deliberately left bare:
/// with the flags on, the kernel may join this socket to an unrelated reuse
/// group and load-balance SEARCHes away from it — and because reuse means a
/// collision does *not* fail, there would be no error to detect afterwards.
///
/// Nor is there a message to read. CA announces the same situation — a
/// `cas WARNING: ... two or more servers share the same UDP port` from
/// `rsrv` — but PVA has no equivalent on either side of the wire, so a
/// silently shared search port is invisible to the process that shared it.
/// The only positive evidence of exclusive ownership is a client-side
/// listing (`pvxlist`) that sees exactly the expected server.
pub fn bind_udp_search(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    bind_udp_search_socket(addr)
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
    if addr.port() != 0 {
        set_reuse_opt(fd, libc::SO_REUSEADDR)?;
        set_reuse_opt(fd, libc::SO_REUSEPORT)?;
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
    Ok(socket)
}

#[cfg(not(unix))]
fn bind_udp_search_socket(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    // Non-Unix host: plain bind with no pre-bind reuse. RTEMS is Unix-family,
    // so the arm above is the one that matters for the target.
    UdpSocket::bind(addr)
}

/// Answer UDP SEARCHes on one socket until `shutdown` is set.
///
/// The decode is [`process_search_datagram`] — the same function the hosted
/// responder drives, moved onto neutral ground by stage D part 1 — so replies
/// are byte-identical between the two drivers, including the
/// `MustReply`-with-no-matches reply that lets `pvlist` see this server at all.
/// This loop owns only the socket: recv, filter, decode, send.
///
/// Three arguments the hosted responder computes per NIC are fixed here, and
/// each is a *structural* consequence of having one wildcard socket and no
/// interface enumerator, not a simplification:
///
/// * `reply_iface_ip = UNSPECIFIED` — there is no per-NIC bundle to pin a
///   reply to, so the decode resolves `iface_hint` to `None` and the OS routes.
/// * `origin = Direct` — datagrams arrive on the search socket itself, never
///   peeled out of an ORIGIN_TAG prefix.
/// * `origin_tag_forwarding = false` — no loopback multicast socket is bound,
///   so no forward can be emitted.
fn handle_udp_search_blocking(
    socket: UdpSocket,
    source: &DynSource,
    config: &PvaServerConfig,
    tcp_port: u16,
    shutdown: &AtomicBool,
) -> PvaResult<()> {
    socket
        .set_read_timeout(Some(UDP_STOP_TICK))
        .map_err(PvaError::Io)?;
    let _ = enter_ioc_thread(PVA_UDP_PRIORITY);
    let mut buf = vec![0u8; UDP_RECV_BUF];

    while !shutdown.load(Ordering::Acquire) {
        let (len, src) = match socket.recv_from(&mut buf) {
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

        // The decode's only suspension point is the source name lookup. On
        // RTEMS `block_on_sync` parks this thread; on a host test it does the
        // same, since this loop owns its thread outright.
        let outputs = match block_on_sync(process_search_datagram(
            source,
            false,
            config.udp_port,
            &buf[..len],
            src,
            Ipv4Addr::UNSPECIFIED,
            Origin::Direct,
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
                SearchOutput::Reply { dest, bytes, .. } => {
                    // `iface_hint` is `None` by construction here (see the
                    // `reply_iface_ip` note above), so there is no NIC to pin
                    // and nothing to branch on.
                    if let Err(e) = socket.send_to(&bytes, dest) {
                        debug!(%dest, error = %e, "blocking UDP SEARCH reply send failed");
                    }
                }
                // Unreachable while `origin_tag_forwarding` is false, which
                // this responder passes unconditionally. Logged rather than
                // silently dropped, as the hosted dispatcher does for its own
                // unreachable arm.
                SearchOutput::OriginTagForward { dest, .. } => {
                    warn!(%dest, "blocking UDP responder produced an ORIGIN_TAG forward with no loopback multicast socket bound");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
    use std::io::Cursor;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use tokio::io::AsyncReadExt;

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
        ] {
            let loops = src.matches(concat!("self.listener.inco", "ming()")).count()
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

    /// Production scope of a source file: everything before the first
    /// column-0 `#[cfg(test)]`. Same helper the accept-driver guard uses.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
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
        let prod = production_scope(include_str!("blocking.rs"));
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

    // ── adapter: cancel-safety ──────────────────────────────────────────

    /// §1.5's claim, which the design doc flags as the one thing it reasoned
    /// about but did not execute: losing a `select!` race must consume
    /// nothing. `read_frame` is used directly as a `select!` arm, so if this
    /// adapter dropped bytes on a lost race the failure would be silent and
    /// intermittent — a truncated frame long after the fact.
    ///
    /// Both boundaries of "what was in flight when the race was lost":
    ///
    /// * **mid-chunk** — part of a chunk has been handed out and the rest is
    ///   parked in `cur`/`pos`;
    /// * **pending** — no chunk has arrived at all, so the poll registered a
    ///   waker and returned `Pending`.
    #[tokio::test]
    async fn channel_reader_loses_no_bytes_when_a_select_race_is_lost() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(CHUNK_QUEUE_DEPTH);
        let mut reader = ChannelReader::new(rx);

        // Boundary 1: a partially-consumed chunk survives.
        tx.send(b"ABCDEFGH".to_vec()).await.expect("chunk queued");
        let mut small = [0u8; 3];
        let n = reader.read(&mut small).await.expect("first read");
        assert_eq!(&small[..n], b"ABC");
        for _ in 0..4 {
            let mut buf = [0u8; 8];
            tokio::select! {
                biased;
                // This arm always wins, so the read future below is created
                // and dropped without ever completing.
                _ = std::future::ready(()) => {}
                _ = reader.read(&mut buf) => unreachable!("the ready arm wins under `biased`"),
            }
        }
        let mut rest = [0u8; 8];
        let n = reader.read(&mut rest).await.expect("read after lost races");
        assert_eq!(
            &rest[..n],
            b"DEFGH",
            "a lost race must not eat the parked tail of the chunk"
        );

        // Boundary 2: a poll that returned Pending consumed nothing either.
        for _ in 0..4 {
            let mut buf = [0u8; 8];
            tokio::select! {
                biased;
                _ = std::future::ready(()) => {}
                _ = reader.read(&mut buf) => unreachable!("the ready arm wins under `biased`"),
            }
        }
        tx.send(b"IJKL".to_vec())
            .await
            .expect("second chunk queued");
        let mut after = [0u8; 8];
        let n = reader
            .read(&mut after)
            .await
            .expect("read after pending races");
        assert_eq!(
            &after[..n],
            b"IJKL",
            "a chunk must not be taken out of the channel by a poll that returned Pending"
        );

        // And EOF still reads as EOF once every sender is gone.
        drop(tx);
        let mut eof = [0u8; 8];
        assert_eq!(
            reader.read(&mut eof).await.expect("eof read"),
            0,
            "all senders dropped must surface as a zero-length read"
        );
    }

    // ── end-to-end over a real loopback socket ──────────────────────────

    fn scalar_intro() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        }
    }

    fn scalar_value(v: f64) -> PvField {
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(v)))],
        })
    }

    fn test_source() -> DynSource {
        let pv = SharedPV::new();
        pv.open(scalar_intro(), scalar_value(1.5)).expect("open pv");
        let shared = SharedSource::new();
        shared.add("dut", pv);
        Arc::new(shared)
    }

    /// The client end of a connection, with just enough framing to drive the
    /// server. Shared by the per-connection tests and the accept-loop tests
    /// below, so neither grows its own frame reader.
    struct TestClient(TcpStream);

    impl TestClient {
        fn connect(addr: SocketAddr) -> TestClient {
            let sock = TcpStream::connect(addr).expect("connect");
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("client read timeout");
            TestClient(sock)
        }

        fn send(&mut self, bytes: &[u8]) {
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
        fn read_until(&mut self, command: Command) -> Vec<u8> {
            for _ in 0..32 {
                let (header, body) = self.read_frame();
                if !header.flags.is_control() && header.command == command.code() {
                    return body;
                }
            }
            panic!("no {command:?} frame arrived within 32 frames");
        }

        fn close(&self) {
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
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let client = TestClient::connect(addr);
            let (server_sock, peer) = listener.accept().expect("accept");
            let source = test_source();
            let conn = tokio::spawn(async move {
                serve_connection_blocking(
                    server_sock,
                    peer,
                    source,
                    config,
                    PeerEntry::new(false),
                    ChannelInvalidator::new(),
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

    fn isolated_config() -> PvaServerConfig {
        PvaServerConfig {
            wire_byte_order: ByteOrder::Little,
            ..PvaServerConfig::isolated()
        }
    }

    fn app_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Vec<u8> {
        // Client → server: the Server direction bit stays clear.
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        let mut out = Vec::new();
        header.write_into(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    fn create_channel_payload(cid: u32, name: &str, order: ByteOrder) -> Vec<u8> {
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
            fields: Vec::new(),
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
        let err = serve_connection_blocking(
            server_sock,
            peer,
            test_source(),
            isolated_config(),
            PeerEntry::new(false),
            ChannelInvalidator::new(),
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

    // ── writer: the deadline loop ───────────────────────────────────────

    /// The property §3.3 exists for, stated as the case bare SO_SNDTIMEO gets
    /// wrong.
    ///
    /// The peer here is not silent — it accepts a trickle of bytes, one small
    /// read per socket-timeout tick, forever. Under plain SO_SNDTIMEO every
    /// `write` syscall makes progress, so no timeout ever fires and the writer
    /// thread is held indefinitely. The deadline loop bounds the *frame*, so
    /// it gives up at `send_timeout` regardless of the dribble.
    ///
    /// Mutation-checked: delete the loop's top-of-loop deadline check, leaving
    /// only the socket timeout, and this test never returns (the run was
    /// killed at 120 s). That is the whole argument of §3.3, executed.
    #[test]
    fn writer_deadline_loop_ends_a_trickling_client() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        const SEND_TIMEOUT: Duration = Duration::from_millis(400);
        let tick = SEND_TIMEOUT / SEND_TICKS_PER_DEADLINE;
        server.set_write_timeout(Some(tick)).expect("SO_SNDTIMEO");
        // Small enough that a multi-MiB frame cannot be absorbed by the
        // kernel buffers, so the writer really does have to wait on the peer.
        let _ = server.set_nodelay(true);

        let read_total = Arc::new(AtomicUsize::new(0));
        let counted = read_total.clone();
        // The trickle keeps a whole 8 MiB frame's worth of bytes in flight, so
        // it must be told to stop rather than left to drain the socket a byte
        // at a time once the writer has given up.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = stop.clone();
        let trickle = thread::spawn(move || {
            // One byte per tick: always progress, never a stall long enough
            // for SO_SNDTIMEO to fire on its own.
            let mut byte = [0u8; 1];
            while !stop_reader.load(Ordering::Relaxed) {
                match client.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        counted.fetch_add(n, Ordering::Relaxed);
                    }
                }
                thread::sleep(tick / 2);
            }
        });

        let frame = vec![0xA5u8; 8 * 1024 * 1024];
        let started = Instant::now();
        let err = write_frame_deadline(&server, &frame, SEND_TIMEOUT)
            .expect_err("a trickling peer must not be allowed to hold the writer");
        let elapsed = started.elapsed();

        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "the frame deadline is what fires, not a socket error"
        );
        assert!(
            elapsed < SEND_TIMEOUT * 5,
            "the deadline must bound the whole frame; took {elapsed:?} for a \
             {SEND_TIMEOUT:?} deadline"
        );
        assert!(
            elapsed >= SEND_TIMEOUT,
            "the writer must not give up before its deadline; took {elapsed:?}"
        );
        assert!(
            read_total.load(Ordering::Relaxed) > 0,
            "the peer really was accepting bytes — this is the trickle case, \
             not a dead-socket case"
        );

        stop.store(true, Ordering::Relaxed);
        drop(server);
        let _ = trickle.join();
    }

    /// A complete frame to a peer that reads normally still goes out, and the
    /// deadline does not fire. The negative control for the test above: a
    /// deadline loop that failed everything would pass that one.
    #[test]
    fn writer_deadline_loop_delivers_a_frame_to_a_reading_client() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        server
            .set_write_timeout(Some(Duration::from_millis(50)))
            .expect("SO_SNDTIMEO");

        let frame: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
        let expected = frame.clone();
        let echo = thread::spawn(move || {
            let mut got = vec![0u8; expected.len()];
            client.read_exact(&mut got).expect("client reads the frame");
            assert_eq!(got, expected, "bytes arrive intact and in order");
        });

        write_frame_deadline(&server, &frame, Duration::from_secs(5))
            .expect("a reading peer takes the frame well inside the deadline");
        echo.join().expect("client thread");
    }

    // ── writer adapter backpressure ─────────────────────────────────────

    /// `ChannelWriter` holds only a weak sender, so the driver's own sender is
    /// what keeps the frame channel alive. That is what makes teardown
    /// deterministic rather than dependent on when an aborted writer task is
    /// reaped, and it is worth pinning: a future edit that stores a strong
    /// `Sender` here would still pass every other test in this file while
    /// reintroducing a teardown that can hang.
    #[tokio::test]
    async fn channel_writer_does_not_keep_the_frame_channel_open() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        let room = Arc::new(WriteRoom::default());
        let mut writer = ChannelWriter::new(tx.downgrade(), room.clone());

        {
            use tokio::io::AsyncWriteExt;
            writer
                .write_all(b"frame")
                .await
                .expect("first frame queued");
        }
        assert_eq!(rx.recv().await.as_deref(), Some(&b"frame"[..]));

        // The adapter is still alive; the driver lets go of its sender.
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "a live ChannelWriter must not keep the channel open once the driver \
             drops its sender — teardown depends on this"
        );
        // And the adapter reports the closure rather than parking forever.
        {
            use tokio::io::AsyncWriteExt;
            let err = writer
                .write_all(b"late")
                .await
                .expect_err("writing after the channel closed must fail");
            assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
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
    /// until its 30 s deadline, well past the 5 s `recv_timeout` here.
    #[test]
    fn stop_wakes_a_writer_parked_in_the_deadline_loop() {
        // Kept alive and deliberately never read from: this is the stuck-peer
        // case, so the socket buffers fill and the writer blocks.
        let (_client, server, peer) = socket_pair();

        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        let registration = registry.register(server.clone());
        let write_sock = server.clone();
        write_sock
            .set_write_timeout(Some(Duration::from_millis(250)))
            .expect("SO_SNDTIMEO");

        let (tx, rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        // Far longer than this test may take, so only the wake can explain a
        // prompt exit.
        const DEADLINE: Duration = Duration::from_secs(30);
        let room = Arc::new(WriteRoom::default());
        let wake = registration.wake_handle();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = thread::spawn(move || {
            writer_thread(write_sock, rx, room, wake, DEADLINE, peer);
            let _ = done_tx.send(());
        });

        // Bigger than any socket buffer, so the writer really is parked mid
        // frame rather than done before `stop` runs.
        tx.try_send(vec![0xA5u8; 8 * 1024 * 1024])
            .expect("frame queued");
        thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        registry.stop();
        done_rx
            .recv_timeout(RETIRE_BOUND)
            .expect("stop must return a writer parked in the deadline loop");
        let elapsed = started.elapsed();
        assert!(
            elapsed < DEADLINE / 2,
            "the writer must be woken by the shutdown, not released by its own \
             {DEADLINE:?} deadline; took {elapsed:?}"
        );
        writer.join().expect("writer thread");
        drop(tx);
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
                fields: Vec::new(),
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

    /// §4.2b without a seventh `select!` arm: a writer that ends wakes the
    /// connection through the socket, so the reader's parked `read` returns 0
    /// and the loop unwinds down its existing EOF path. This is what lets
    /// `tcp.rs` stay untouched and the hosted `select!` stay byte-identical.
    ///
    /// Mutation-checked: delete the `wake.wake()` at the end of
    /// `writer_thread` and the parked reader here is never released.
    #[test]
    fn a_writer_that_ends_wakes_a_reader_parked_on_the_socket() {
        // The client stays connected and silent, so the only thing that can
        // return the reader's `read` is a shutdown from this side.
        let (_client, server, peer) = socket_pair();

        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        let registration = registry.register(server.clone());

        let read_sock = server.clone();
        let (read_done, read_result) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 64];
            let _ = read_done.send((&*read_sock).read(&mut buf));
        });
        thread::sleep(Duration::from_millis(200));

        let (tx, rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        let wake = registration.wake_handle();
        let write_sock = server.clone();
        let writer = thread::spawn(move || {
            writer_thread(
                write_sock,
                rx,
                Arc::new(WriteRoom::default()),
                wake,
                Duration::from_secs(5),
                peer,
            )
        });

        // The writer's last strong sender goes: it ends, and on the way out it
        // wakes the connection.
        drop(tx);
        let n = read_result
            .recv_timeout(RETIRE_BOUND)
            .expect("a parked reader must be released when the writer ends")
            .expect("the shutdown surfaces as a clean end-of-stream, not an error");
        assert_eq!(n, 0, "the woken read must report end-of-stream");

        writer.join().expect("writer thread");
        reader.join().expect("reader thread");
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

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        {
            let registration = registry.register(server.clone());
            let read_sock = server.clone();
            let _reader = ReaderGuard {
                wake: registration.wake_handle(),
                peer,
                handle: Some(thread::spawn(move || {
                    let mut buf = [0u8; 64];
                    let _ = (&*read_sock).read(&mut buf);
                    let _ = done_tx.send(());
                })),
            };
            assert_eq!(registry.live_connections(), 1);
            thread::sleep(Duration::from_millis(200));
            assert!(
                done_rx.try_recv().is_err(),
                "precondition: the reader must be parked in `read`, or this \
                 test proves nothing"
            );
            // Stand-in for the writer-spawn `?`: leave the scope with the
            // reader running and the writer never created.
        }
        done_rx
            .recv_timeout(RETIRE_BOUND)
            .expect("a reader spawned before a failed setup must still be woken and joined");
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
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let registration = registry.register(server.clone());
            let read_sock = server.clone();
            let _reader = ReaderGuard {
                wake: registration.wake_handle(),
                peer,
                handle: Some(thread::spawn(move || {
                    let mut buf = [0u8; 64];
                    let _ = (&*read_sock).read(&mut buf);
                    let _ = done_tx.send(());
                })),
            };
            panic!("connection handler blew up");
        }));

        assert!(panicked.is_err(), "precondition: the closure must panic");
        done_rx
            .recv_timeout(RETIRE_BOUND)
            .expect("a panic unwinding past the reader must still wake and join it");
        assert_eq!(registry.live_connections(), 0);
    }

    /// The ordering that a naive guard gets wrong: the writer parks on
    /// `frame_rx.recv()` and leaves only when the last strong sender drops, so
    /// a guard that joined *before* dropping the sender would hang forever.
    ///
    /// Holding the sender inside [`WriterGuard`] is what makes that
    /// unexpressible — it cannot be dropped out of order with the join, and
    /// the ordering does not depend on two locals' declaration order.
    #[test]
    fn a_writer_guard_drops_its_sender_before_joining() {
        let (_client, server, peer) = socket_pair();
        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        let registration = registry.register(server.clone());

        let (tx, rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        let wake = registration.wake_handle();
        let write_sock = server.clone();
        let writer = WriterGuard {
            frames: Some(tx),
            peer,
            handle: Some(thread::spawn(move || {
                writer_thread(
                    write_sock,
                    rx,
                    Arc::new(WriteRoom::default()),
                    wake,
                    Duration::from_secs(5),
                    peer,
                )
            })),
        };

        // Drop off-thread so a guard that hangs fails this test instead of
        // hanging the whole suite.
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(writer);
            let _ = dropped_tx.send(());
        });
        dropped_rx.recv_timeout(RETIRE_BOUND).expect(
            "WriterGuard::drop must drop the last strong sender before joining; \
             otherwise the writer stays parked on recv and the join never returns",
        );
    }

    /// Structural closure, as source: both connection threads are owned by a
    /// guard. A bare `let reader = spawn_dedicated_thread(..)` is the shape
    /// that leaked, and it must not come back.
    #[test]
    fn both_connection_threads_are_owned_by_a_joining_guard() {
        let src = include_str!("blocking.rs");
        // Comment lines are skipped: the doc comment above names the banned
        // shape on purpose, and this guard is about CODE. A real binding
        // cannot live on a line starting with `//`. (Needles are split for the
        // same reason — this guard lives in the file it reads.)
        let code: Vec<&str> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        for banned in [
            concat!("let reader = spawn_dedi", "cated_thread("),
            concat!("let writer = spawn_dedi", "cated_thread("),
            concat!("let reader = spawn_", "child("),
            concat!("let writer = spawn_", "child("),
        ] {
            let offenders: Vec<&&str> = code.iter().filter(|l| l.contains(banned)).collect();
            assert!(
                offenders.is_empty(),
                "a connection thread is held in a bare handle again: `{banned}`. \
                 An exit path that does not join it leaks the thread, its socket \
                 and its descriptor for the life of the IOC"
            );
        }
        for required in [
            concat!("let reader = Reader", "Guard {"),
            concat!("let writer = Writer", "Guard {"),
        ] {
            assert!(
                code.iter().any(|l| l.contains(required)),
                "connection setup no longer binds `{required}` — both threads \
                 must be owned by a guard that joins them on every exit path"
            );
        }
    }

    /// A subscriber that keeps what was emitted, so a test can assert an
    /// announcement actually happened.
    ///
    /// `errlog_sev_printf` routes through `tracing` on the
    /// `epics_base_rs::errlog` target *and* to the console fallback; the
    /// `tracing` half is the observable one from in-process, and installing
    /// this for the duration of a drop is how these tests read it.
    #[derive(Clone, Default)]
    struct CapturedLines(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for CapturedLines {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields<'a>(&'a mut String);
            impl tracing::field::Visit for Fields<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    let _ = write!(self.0, " {}={value:?}", field.name());
                }
            }
            let mut line = event.metadata().target().to_string();
            event.record(&mut Fields(&mut line));
            self.0.lock().expect("captured lines").push(line);
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Everything emitted on the calling thread while `f` runs.
    fn lines_while(f: impl FnOnce()) -> Vec<String> {
        let captured = CapturedLines::default();
        tracing::subscriber::with_default(captured.clone(), f);
        let lines = captured.0.lock().expect("captured lines");
        lines.clone()
    }

    /// F3 made a lost child *survivable*; this is what makes it *visible*.
    ///
    /// `ReaderGuard::drop` joined and threw the result away, so a reader that
    /// unwound left nothing behind: the connection's own error is a bland
    /// channel-closed, and the two were unlinkable.
    #[test]
    fn a_panicked_reader_is_reported_and_not_discarded() {
        let (_client, server, peer) = socket_pair();
        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        let registration = registry.register(server.clone());

        let lines = lines_while(|| {
            let _reader = ReaderGuard {
                wake: registration.wake_handle(),
                peer,
                handle: Some(thread::spawn(|| panic!("reader blew up"))),
            };
        });

        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("epics_base_rs::errlog")
                    && l.contains("reader thread panicked")),
            "a panicked reader must reach errlog, which prints whatever the log \
             configuration is — including an RTEMS console. Captured: {lines:?}"
        );
    }

    #[test]
    fn a_panicked_writer_is_reported_and_not_discarded() {
        let (_client, _server, peer) = socket_pair();
        let (frames, _rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);

        let lines = lines_while(|| {
            let _writer = WriterGuard {
                frames: Some(frames),
                peer,
                handle: Some(thread::spawn(|| panic!("writer blew up"))),
            };
        });

        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("epics_base_rs::errlog")
                    && l.contains("writer thread panicked")),
            "a panicked writer dropped whatever frames were still queued; that \
             must not be silent. Captured: {lines:?}"
        );
    }

    /// The other boundary: an ordinary teardown is not a loss. Every
    /// connection that ever closes runs these drops, so announcing there
    /// would bury the real losses on a serial console.
    #[test]
    fn a_child_that_ends_cleanly_is_not_announced() {
        let (_client, server, peer) = socket_pair();
        let registry = ConnRegistry::new();
        let server = Arc::new(server);
        let registration = registry.register(server.clone());

        let lines = lines_while(|| {
            let _reader = ReaderGuard {
                wake: registration.wake_handle(),
                peer,
                handle: Some(thread::spawn(|| {})),
            };
        });

        assert!(
            !lines
                .iter()
                .any(|l| l.contains("was lost") || l.contains("panicked")),
            "an ordinary connection teardown must print nothing: {lines:?}"
        );
    }

    /// Structural closure as source, for the one loss no test can force: a
    /// thread that cannot be created. Both guards and the single spawn site
    /// must report through the one announcement function.
    #[test]
    fn every_child_loss_goes_through_the_announcement() {
        let prod = production_scope(include_str!("blocking.rs"));
        assert_eq!(
            prod.matches(concat!("let _ = han", "dle.join()")).count(),
            0,
            "a discarded join result is a panicked child nobody hears about"
        );
        for owner in [
            "impl Drop for ReaderGuard",
            "impl Drop for WriterGuard",
            "fn spawn_child(",
        ] {
            let at = prod
                .find(owner)
                .unwrap_or_else(|| panic!("`{owner}` is gone from this module"));
            let body = &prod[at..(at + 900).min(prod.len())];
            assert!(
                body.contains(concat!("child_thread_", "lost(")),
                "`{owner}` can lose a per-connection thread without saying so"
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
            move || serving.serve(),
        )
        .expect("accept thread spawned");
        (server, accept)
    }

    /// Poll `f` until it holds, or fail. Connection setup and teardown both
    /// finish on threads this test does not join, so every assertion about
    /// them is "eventually", and it must be a bounded eventually.
    fn eventually(what: &str, mut f: impl FnMut() -> bool) {
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
    /// at `epicsThreadPriorityCAServerLow-2` (`server.cpp:388`) and
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

    /// One rule for every thread in this module: spawned through the runtime
    /// seam, at `PVA_SERVER_PRIORITY`. A raw `thread::Builder` in production
    /// would be a thread with no priority and, on the host, no ambient
    /// runtime — the failure the accept loop's connections would hit first.
    ///
    /// `thread::spawn` is banned for a second reason on top of that one: it
    /// cannot express a stack size at all, so on RTEMS it takes std's generic
    /// 2 MiB `DEFAULT_MIN_STACK_SIZE` instead of C's class. This module makes
    /// three threads per connection, so that is the difference between 6 MiB
    /// and 2 MiB of the target's fixed pool per client. The `Builder` ban above
    /// does not cover it — `thread::spawn` is invisible to a check that keys on
    /// `Builder` — which is why it is named separately here, as
    /// `epics-base-rs`'s `every_thread_in_this_crate_states_a_stack_size` does.
    #[test]
    fn every_server_thread_goes_through_the_seam() {
        let prod = production_scope(include_str!("blocking.rs"));
        assert_eq!(
            prod.matches(concat!("thread", "::Builder")).count(),
            0,
            "spawn server threads with `spawn_dedicated_thread`, not directly: \
             the seam is what applies the priority and carries the ambient runtime"
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
            prod.matches("spawn_dedicated_thread(").count(),
            2,
            "the per-connection thread, and `spawn_child` for the reader and \
             the writer — which share one spawn site so that neither can fail \
             silently"
        );
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
        let (server, accept) = start_server(config);
        let socket = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("bind the blocking search socket");
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
    /// (`server.cpp:388`). Both numbers are pinned, and so is the ordering —
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
        let prod = production_scope(include_str!("blocking.rs"));
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
    /// `server.cpp:654-670` vs `serverconn.cpp:461-467`): a noisy host's
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

    /// An ephemeral search port is bound *bare* — no `SO_REUSEADDR`/
    /// `SO_REUSEPORT` — so a second bind on that number fails loudly instead
    /// of silently joining a reuse group and load-balancing SEARCHes away.
    /// That silent sharing is exactly the failure this rule prevents: with
    /// the flags on there is no error to detect afterwards, and unlike CA
    /// there is no `cas WARNING` printed either — nothing would report it.
    #[test]
    fn an_ephemeral_search_port_is_not_silently_shared() {
        let first = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("first bind");
        let port = first.local_addr().expect("addr").port();
        assert_ne!(port, 0, "an ephemeral bind resolves the port");

        let second = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        assert!(
            second.is_err(),
            "a second bind on an ephemeral port must fail, not silently share it"
        );
    }

    /// The RTEMS driver's socket handles must be SHARED, never duplicated.
    ///
    /// The banned call is `std`'s clone-the-descriptor method, i.e.
    /// `fcntl(F_DUPFD*)`. RTEMS 6 has no `F_DUPFD_CLOEXEC` case at all
    /// (`cpukit/libcsupport/src/fcntl.c` falls to `default: EINVAL`), and
    /// plain `F_DUPFD` goes through `duplicate_iop` (`fcntl.c:47-77`), which
    /// calls the file's `open_h` — and rtems-libbsd installs
    /// `rtems_bsd_sysgen_nodeops` on every socket
    /// (`rtems-bsd-syscall-api.c:205`) whose `.open_h` is
    /// `rtems_bsd_sysgen_open_error`. Measured on target: `dup`, `F_DUPFD`
    /// and `F_DUPFD_CLOEXEC` all fail on an accepted socket with ENXIO, while
    /// `F_DUPFD` on `/dev/console` succeeds.
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
